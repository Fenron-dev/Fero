//! # api::manga::packed
//!
//! Decoder for JavaScript minified with the Dean Edwards packer
//! (`eval(function(p,a,c,k,e,d){…})`).
//!
//! ## Why this exists
//! FanFox delivers the image URLs of a chapter inside packed scripts.  Packing
//! is a **minification** technique — the payload, the base, and the full
//! dictionary all travel in plain sight in the same response, and the decoder
//! below is a direct transcription of the unpacking routine the page ships to
//! every browser.  Running it here avoids embedding a JS engine, or driving a
//! browser window for what is a string substitution.
//!
//! ## Format
//! ```text
//! eval(function(p,a,c,k,e,d){…}('<payload>', <base>, <count>, '<a|b|c>'.split('|'), 0, {}))
//! ```
//! Each token in `payload` is a number written in base `<base>`; it indexes
//! into the `|`-separated dictionary.  Empty dictionary slots mean "keep the
//! token as-is".
//!
//! ## Dependencies:
//! - none (hand-rolled parsing, no regex crate in the dependency set)

/// Extracts the arguments of a packed script and expands the payload.
///
/// Returns `None` when `script` is not in packer format — callers treat that
/// as "the site changed" and surface a parse error.
pub fn unpack(script: &str) -> Option<String> {
    let (payload, base, count, dictionary) = parse_arguments(script)?;
    Some(expand(&payload, base, count, &dictionary))
}

/// Splits the packer call into `(payload, base, count, dictionary)`.
fn parse_arguments(script: &str) -> Option<(String, usize, usize, Vec<String>)> {
    // The payload is the first single-quoted string after the closing `}(` of
    // the unpacker function; everything before it is the fixed boilerplate.
    let call_start = script.find("}(").or_else(|| script.find("}, ("))?;
    let rest = &script[call_start..];
    let payload_start = rest.find('\'')?;
    let (payload, after_payload) = read_js_string(&rest[payload_start..])?;

    // `, <base>, <count>, '<dictionary>'.split('|')`
    let mut numbers = after_payload
        .trim_start()
        .trim_start_matches(',')
        .split(',')
        .map(str::trim);
    let base: usize = numbers.next()?.parse().ok()?;
    let count: usize = numbers.next()?.parse().ok()?;

    let dictionary_start = after_payload.find('\'')?;
    let (dictionary, _) = read_js_string(&after_payload[dictionary_start..])?;
    let dictionary: Vec<String> = dictionary.split('|').map(str::to_string).collect();

    Some((payload, base, count, dictionary))
}

/// Reads a single-quoted JavaScript string, honoring backslash escapes.
///
/// Returns the decoded contents and the remainder after the closing quote.
fn read_js_string(text: &str) -> Option<(String, &str)> {
    let mut chars = text.char_indices();
    if chars.next()?.1 != '\'' {
        return None;
    }
    let mut value = String::new();
    let mut escaped = false;
    for (offset, ch) in chars {
        if escaped {
            // Only the escapes the packer actually emits need decoding; any
            // other escaped character stands for itself.
            value.push(match ch {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '\'' => return Some((value, &text[offset + ch.len_utf8()..])),
            other => value.push(other),
        }
    }
    None
}

/// Replaces every base-`base` token in `payload` with its dictionary entry.
///
/// ## Single pass, by design
/// The original routine loops `count` times and rewrites the whole payload on
/// each iteration, so a dictionary entry can itself be rewritten by a later
/// iteration.  Real packer output never relies on that (the packer builds the
/// dictionary from the source's own identifiers), and a single pass cannot
/// loop or corrupt tokens that a substitution introduced — so this walks the
/// payload once and substitutes each word token exactly once.
fn expand(payload: &str, base: usize, count: usize, dictionary: &[String]) -> String {
    let mut result = String::with_capacity(payload.len() * 2);
    let mut token = String::new();

    let flush = |token: &mut String, result: &mut String| {
        if token.is_empty() {
            return;
        }
        match decode_token(token, base) {
            // Empty dictionary slots mean "leave the token alone" — that is
            // what the `k[c] || e(c)` fallback in the original expresses.
            Some(index) if index < count => match dictionary.get(index) {
                Some(word) if !word.is_empty() => result.push_str(word),
                _ => result.push_str(token),
            },
            _ => result.push_str(token),
        }
        token.clear();
    };

    for ch in payload.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            flush(&mut token, &mut result);
            result.push(ch);
        }
    }
    flush(&mut token, &mut result);
    result
}

/// Decodes one base-`base` token back into its dictionary index.
///
/// Digits run `0-9`, then `a-z`; bases above 36 continue with the character
/// range the packer's encoder produces via `String.fromCharCode(c + 29)`.
fn decode_token(token: &str, base: usize) -> Option<usize> {
    if base == 0 {
        return None;
    }
    let mut value = 0usize;
    for ch in token.chars() {
        let digit = match ch {
            '0'..='9' => ch as usize - '0' as usize,
            'a'..='z' => ch as usize - 'a' as usize + 10,
            // Mirror of `String.fromCharCode(c + 29)` for digits above 35.
            _ => (ch as usize).checked_sub(29)?,
        };
        if digit >= base {
            return None;
        }
        value = value.checked_mul(base)?.checked_add(digit)?;
    }
    Some(value)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The unpacker boilerplate, identical in every packed response.
    const PREFIX: &str = "eval(function(p,a,c,k,e,d){e=function(c){return c};\
                          while(c--)if(k[c])p=p.replace(new RegExp('\\b'+e(c)+'\\b','g'),k[c]);\
                          return p;}(";

    #[test]
    fn expands_a_simple_dictionary() {
        let script = format!("{PREFIX}'0 1=2;',3,3,'var|x|41'.split('|'),0,{{}}))");
        assert_eq!(unpack(&script).as_deref(), Some("var x=41;"));
    }

    #[test]
    fn keeps_tokens_without_a_dictionary_entry() {
        // Slot 1 is empty: the token must survive verbatim.
        let script = format!("{PREFIX}'0 1 2',3,3,'var||end'.split('|'),0,{{}}))");
        assert_eq!(unpack(&script).as_deref(), Some("var 1 end"));
    }

    #[test]
    fn decodes_tokens_above_base_ten() {
        // Base 16: token `a` is index 10, `10` is index 16.
        let script = format!(
            "{PREFIX}'a 10',16,17,'0|1|2|3|4|5|6|7|8|9|ten|11|12|13|14|15|sixteen'.split('|'),0,{{}}))"
        );
        assert_eq!(unpack(&script).as_deref(), Some("ten sixteen"));
    }

    #[test]
    fn decodes_the_shape_fanfox_ships() {
        // Reduced from a real chapter response: a key assembled by concatenation.
        let script = format!(
            "{PREFIX}'7 3=\\'\\'+\\'9\\'+\\'2\\';$(\"#a\").b(3);',12,12,\
             '||e|guidkey||||var|d||dm5_key|val'.split('|'),0,{{}}))"
        );
        let unpacked = unpack(&script).expect("packed script should decode");
        assert!(unpacked.contains("var guidkey="));
        assert!(unpacked.contains("$(\"#dm5_key\").val(guidkey);"));
    }

    #[test]
    fn preserves_punctuation_and_quoted_content() {
        let script = format!("{PREFIX}'0(\"1\",[2]);',3,3,'call|arg|9'.split('|'),0,{{}}))");
        assert_eq!(unpack(&script).as_deref(), Some("call(\"arg\",[9]);"));
    }

    #[test]
    fn rejects_scripts_that_are_not_packed() {
        assert!(unpack("var x = 1;").is_none());
        assert!(unpack("").is_none());
    }

    #[test]
    fn oversized_tokens_do_not_panic() {
        // A token far longer than any real index must fail cleanly, not wrap.
        let script = format!("{PREFIX}'zzzzzzzzzzzzzzzzzzzzzzzz',36,2,'a|b'.split('|'),0,{{}}))");
        let unpacked = unpack(&script).expect("script should still decode");
        assert_eq!(unpacked, "zzzzzzzzzzzzzzzzzzzzzzzz");
    }
}
