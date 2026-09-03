//! # api::novel::wtrlab
//!
//! WTR-Lab adapter (`wtr-lab.com`).
//!
//! ## Why this one needs its own adapter
//! The site is a Next.js application: the series page carries its data in a
//! `__NEXT_DATA__` script rather than in markup, and neither the chapter list
//! nor the chapter text is in the HTML at all. Three sources have to be
//! combined:
//!
//! | What | Where |
//! |---|---|
//! | Metadata | `__NEXT_DATA__` of the series page |
//! | Chapter list | `GET /api/chapters/<raw_id>` |
//! | Chapter text | `POST /api/reader/get` |
//!
//! The chapter text is the reason [`PoliteClient::post_json`] exists: WTR-Lab
//! answers only a request that names the novel and the chapter in a body.
//!
//! ## The two ids
//! Every series has two: the `raw_id` that appears in the URL, and an internal
//! `id` that does not. They are different numbers, and `/api/chapters` keys on
//! the **`raw_id`**. Passing the internal one does not fail — it answers `200`
//! with the chapter list of *a different novel*. That is the trap this adapter
//! exists to not fall into, and why the id always comes from the URL.

use serde_json::Value;

use super::{ChapterContent, ChapterRef, NovelInfo, NovelSource, PoliteClient};
use crate::api::release_date;
use crate::error::{FeroError, Result};

/// Host the adapter answers for.
const HOST: &str = "https://wtr-lab.com";

/// Translation service and language requested for the chapter text.
///
/// Fero asks for what the site itself defaults to. Offering a choice would mean
/// storing it per subscription and re-fetching everything when it changed.
const SERVICE: &str = "ai";
const LANGUAGE: &str = "en";

/// Publication status codes, as the site's own bundle defines them.
const STATUS_COMPLETED: u64 = 1;

/// WTR-Lab adapter.
pub struct WtrLabSource;

impl NovelSource for WtrLabSource {
    fn id(&self) -> &'static str {
        "wtrlab"
    }

    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo> {
        let (_, body) = client.get_text(url)?;
        let mut info = parse_series_page(url, &body)?;

        // Die Serienseite kennt nur die letzten fuenf Kapitel; die vollstaendige
        // Liste kommt aus der API.
        let (raw_id, slug) = series_key(url)?;
        let (_, list) = client.get_text(&format!("{HOST}/api/chapters/{raw_id}"))?;
        let (chapters, latest) = parse_chapter_list(&list, raw_id, &slug)?;
        info.chapters = chapters;
        info.latest_release_unix = latest;
        Ok(info)
    }

    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent> {
        let (raw_id, number) = chapter_key(&chapter.url)?;
        let request = serde_json::json!({
            "translate": SERVICE,
            "language": LANGUAGE,
            "raw_id": raw_id,
            "chapter_no": number,
            "retry": false,
            "force_retry": false,
        })
        .to_string();

        let body = client.post_json(
            &format!("{HOST}/api/reader/get"),
            request,
            &[("Referer", chapter.url.as_str())],
        )?;
        parse_chapter_body(&body, &chapter.title, &chapter.url)
    }
}

/// Reads the metadata out of a series page's `__NEXT_DATA__`.
///
/// The chapter list stays empty here — the caller fills it from the API.
fn parse_series_page(page_url: &str, body: &str) -> Result<NovelInfo> {
    let data = next_data(body).ok_or_else(|| {
        FeroError::ExternalApi(format!(
            "WTR-Lab: __NEXT_DATA__ nicht gefunden (Cloudflare-Block?): {page_url}"
        ))
    })?;
    let props = data.pointer("/props/pageProps").ok_or_else(|| {
        FeroError::ExternalApi(format!("WTR-Lab: unerwarteter Seitenaufbau: {page_url}"))
    })?;
    let serie = props.pointer("/serie/serie_data").ok_or_else(|| {
        FeroError::ExternalApi(format!("WTR-Lab: Serienangaben fehlen: {page_url}"))
    })?;
    let meta = serie.pointer("/data");

    let title = meta
        .and_then(|meta| meta.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .ok_or_else(|| FeroError::ExternalApi(format!("WTR-Lab: Titel fehlt: {page_url}")))?
        .to_string();

    Ok(NovelInfo {
        title,
        author: text_field(meta, "author"),
        cover_url: text_field(meta, "image"),
        description: text_field(meta, "description"),
        // Der Status der Uebersetzung, nicht der der Vorlage: `raw_status`
        // beschreibt das Original, und danach richtet sich nicht, ob hier noch
        // Kapitel nachkommen.
        completed_hint: serie
            .get("status")
            .and_then(Value::as_u64)
            .map(|status| status == STATUS_COMPLETED),
        latest_release_unix: None,
        // Genres stehen nur als Zahlen auf der Seite, ohne Namenstabelle
        // daneben — anders als die Tags. Eine Id ins Regal zu schreiben waere
        // schlechter als nichts.
        genres: Vec::new(),
        tags: tag_names(props),
        chapters: Vec::new(),
    })
}

/// Turns the chapter API's answer into references, newest release date aside.
///
/// The API answers newest-last already, but the order is rebuilt from the
/// `order` field rather than trusted: the adapter contract promises reading
/// order, and a sorted list is cheap insurance against a changed endpoint.
fn parse_chapter_list(
    body: &str,
    raw_id: u64,
    slug: &str,
) -> Result<(Vec<ChapterRef>, Option<u64>)> {
    let parsed: Value = serde_json::from_str(body)
        .map_err(|e| FeroError::ExternalApi(format!("WTR-Lab: Kapitelliste unlesbar: {e}")))?;
    let entries = parsed
        .get("chapters")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            FeroError::ExternalApi("WTR-Lab: Kapitelliste ohne Eintraege.".to_string())
        })?;

    let now = crate::core::subscription::unix_now();
    let mut latest: Option<u64> = None;
    let mut chapters: Vec<(u64, ChapterRef)> = Vec::with_capacity(entries.len());
    for entry in entries {
        // Die beiden Endpunkte tippen dieselben Felder unterschiedlich: auf der
        // Seite Zahlen, in der API Zeichenketten. Beides muss durchgehen.
        let Some(order) = flexible_u64(entry.get("order")) else {
            continue;
        };
        if let Some(released) = entry
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(|text| release_date::parse_release(text, now))
        {
            latest = Some(latest.unwrap_or(released).max(released));
        }
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Chapter {order}"));
        chapters.push((
            order,
            ChapterRef {
                title,
                url: format!("{HOST}/en/novel/{raw_id}/{slug}/chapter-{order}"),
            },
        ));
    }

    if chapters.is_empty() {
        return Err(FeroError::ExternalApi(
            "WTR-Lab: Kapitelliste war leer.".to_string(),
        ));
    }
    chapters.sort_by_key(|(order, _)| *order);
    Ok((
        chapters.into_iter().map(|(_, chapter)| chapter).collect(),
        latest,
    ))
}

/// Turns the reader answer into chapter content.
///
/// Locked and unreleased chapters are reported rather than stored: an empty
/// chapter would be written into the EPUB as a real one, and the block it sits
/// in never gets rebuilt.
fn parse_chapter_body(body: &str, fallback_title: &str, url: &str) -> Result<ChapterContent> {
    let parsed: Value = serde_json::from_str(body)
        .map_err(|e| FeroError::ExternalApi(format!("WTR-Lab: Antwort unlesbar: {e}")))?;

    if parsed.get("success").and_then(Value::as_bool) != Some(true) {
        let reason = match parsed.get("code").and_then(Value::as_str) {
            Some("CHAPTER_LOCKED") => "Kapitel ist gesperrt (kostenpflichtig)".to_string(),
            Some(code) => format!("Quelle meldet {code}"),
            None if parsed.get("requireTurnstile").and_then(Value::as_bool) == Some(true) => {
                "Cloudflare-Prüfung nötig — im Abo unter „Zugang zur Quelle\" lösen".to_string()
            }
            None => "Quelle lieferte kein Kapitel".to_string(),
        };
        return Err(FeroError::ExternalApi(format!("{reason}: {url}")));
    }
    if parsed.pointer("/chapter/locked").and_then(Value::as_bool) == Some(true) {
        return Err(FeroError::ExternalApi(format!(
            "Kapitel ist gesperrt (kostenpflichtig): {url}"
        )));
    }

    let paragraphs = parsed
        .pointer("/data/data/body")
        .and_then(Value::as_array)
        .ok_or_else(|| FeroError::ExternalApi(format!("WTR-Lab: Kapiteltext fehlt: {url}")))?;

    // Die Absaetze kommen als reiner Text, nicht als HTML. Sie werden trotzdem
    // durch den Allowlist-Sanitizer geschickt: er entschaerft die Zeichen, die
    // ein EPUB sonst ungueltig machen — und die Regel steht an einer Stelle.
    let fragment: String = paragraphs
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| format!("<p>{line}</p>"))
        .collect();
    if fragment.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "WTR-Lab: Kapitel ohne Text: {url}"
        )));
    }

    let title = parsed
        .pointer("/data/data/title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(fallback_title)
        .to_string();

    Ok(ChapterContent {
        title,
        xhtml: super::sanitize_to_xhtml(&fragment),
    })
}

/// The `raw_id` and slug of a series URL.
///
/// Always from the URL, never from the page: the id in the JSON is the internal
/// one, and the chapter API answers it with someone else's novel.
fn series_key(url: &str) -> Result<(u64, String)> {
    let rest = url
        .split("/novel/")
        .nth(1)
        .ok_or_else(|| FeroError::ExternalApi(format!("WTR-Lab: keine Serien-URL: {url}")))?;
    let mut parts = rest.split('/');
    let raw_id: u64 = parts
        .next()
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| FeroError::ExternalApi(format!("WTR-Lab: Serien-Id fehlt: {url}")))?;
    let slug = parts
        .next()
        .map(|slug| slug.trim_end_matches('/'))
        .filter(|slug| !slug.is_empty())
        .ok_or_else(|| FeroError::ExternalApi(format!("WTR-Lab: Serien-Kürzel fehlt: {url}")))?;
    Ok((raw_id, slug.to_string()))
}

/// The `raw_id` and chapter number of a chapter URL.
fn chapter_key(url: &str) -> Result<(u64, u64)> {
    let (raw_id, _) = series_key(url)?;
    let number = url
        .rsplit_once("/chapter-")
        .and_then(|(_, tail)| {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .ok_or_else(|| FeroError::ExternalApi(format!("WTR-Lab: Kapitelnummer fehlt: {url}")))?;
    Ok((raw_id, number))
}

/// The `__NEXT_DATA__` payload of a Next.js page.
fn next_data(body: &str) -> Option<Value> {
    let start = body.find(r#"<script id="__NEXT_DATA__""#)?;
    let open = body[start..].find('>')? + start + 1;
    let end = body[open..].find("</script>")? + open;
    serde_json::from_str(&body[open..end]).ok()
}

/// A trimmed, non-empty string field.
fn text_field(container: Option<&Value>, key: &str) -> Option<String> {
    container?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Tag names for the series, resolved through the page's own lookup table.
fn tag_names(props: &Value) -> Vec<String> {
    props
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| tag.get("title").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A number that the site writes sometimes as a number, sometimes as a string.
fn flexible_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES_PAGE: &str = r##"<html><body>
      <script id="__NEXT_DATA__" type="application/json">
      {"props":{"pageProps":{
        "serie":{"serie_data":{
          "id":91945,"raw_id":95971,"slug":"ein-titel","status":1,"raw_status":0,
          "chapter_count":3,
          "data":{"title":" Ein Titel ","author":"Jemand","image":"https://img.example/c.png",
                  "description":"Worum es geht."}
        }},
        "tags":[{"id":417,"title":"Male Protagonist"},{"id":696,"title":"Cultivation"}]
      }}}
      </script></body></html>"##;

    #[test]
    fn reads_metadata_out_of_the_next_data_script() {
        let info = parse_series_page("https://wtr-lab.com/en/novel/95971/ein-titel", SERIES_PAGE)
            .expect("series page should parse");

        assert_eq!(info.title, "Ein Titel");
        assert_eq!(info.author.as_deref(), Some("Jemand"));
        assert_eq!(info.description.as_deref(), Some("Worum es geht."));
        assert_eq!(info.completed_hint, Some(true));
        assert_eq!(info.tags, vec!["Male Protagonist", "Cultivation"]);
        // Die Liste fuellt der Aufrufer aus der API.
        assert!(info.chapters.is_empty());
    }

    /// Der Fehlerfall, der eine Serie stillschweigend leer laesst.
    #[test]
    fn a_page_without_the_data_script_is_an_error() {
        let result = parse_series_page("https://wtr-lab.com/en/novel/1/x", "<html></html>");
        assert!(matches!(result, Err(FeroError::ExternalApi(_))));
    }

    /// Die URL traegt `raw_id`, das JSON eine andere Id. Wer die interne nimmt,
    /// bekommt von der Kapitel-API die Kapitel einer *fremden* Serie — mit
    /// Status 200. Deshalb kommt die Id immer aus der Adresse.
    #[test]
    fn the_id_comes_from_the_url_not_from_the_page() {
        let (raw_id, slug) = series_key("https://wtr-lab.com/en/novel/95971/ein-titel").unwrap();
        assert_eq!(raw_id, 95971);
        assert_eq!(slug, "ein-titel");

        let data = next_data(SERIES_PAGE).expect("script should parse");
        let internal = data
            .pointer("/props/pageProps/serie/serie_data/id")
            .unwrap();
        assert_ne!(
            internal.as_u64(),
            Some(raw_id),
            "die beiden Ids sind nicht dieselbe"
        );
    }

    #[test]
    fn chapter_urls_carry_the_number_back_out() {
        let (raw_id, number) =
            chapter_key("https://wtr-lab.com/en/novel/95971/ein-titel/chapter-42").unwrap();
        assert_eq!((raw_id, number), (95971, 42));
        assert!(chapter_key("https://wtr-lab.com/en/novel/95971/ein-titel").is_err());
    }

    /// Die API tippt `order` als Zeichenkette, die Seite als Zahl.
    #[test]
    fn the_chapter_list_sorts_into_reading_order() {
        let body = r#"{"chapters":[
          {"order":"3","title":"Drittes","updated_at":"2026-08-10 11:39:16.258+00"},
          {"order":"1","title":"Erstes","updated_at":"2026-08-01 09:00:00.000+00"},
          {"order":2,"title":"Zweites","updated_at":"2026-08-05 09:00:00.000+00"}
        ]}"#;
        let (chapters, latest) = parse_chapter_list(body, 95971, "ein-titel").unwrap();

        let titles: Vec<&str> = chapters.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["Erstes", "Zweites", "Drittes"]);
        assert_eq!(
            chapters[0].url,
            "https://wtr-lab.com/en/novel/95971/ein-titel/chapter-1"
        );
        // Das juengste der drei Daten, nicht das zuletzt gelesene.
        assert_eq!(latest, Some(1_786_320_000));
    }

    #[test]
    fn a_chapter_becomes_escaped_xhtml() {
        let body = r#"{"success":true,"chapter":{"locked":false},
          "data":{"data":{"title":"Kapitel 1","body":["Erster Absatz.","Zweiter & <dritter>.","  "]}}}"#;
        let content = parse_chapter_body(body, "Ersatztitel", "https://wtr-lab.com/x").unwrap();

        assert_eq!(content.title, "Kapitel 1");
        assert!(content.xhtml.contains("<p>Erster Absatz.</p>"));
        // Leere Absaetze fallen weg, Sonderzeichen werden entschaerft.
        assert!(content.xhtml.contains("&amp;"), "{}", content.xhtml);
        assert!(!content.xhtml.contains("<dritter>"), "{}", content.xhtml);
    }

    /// Ein gesperrtes Kapitel muss auffallen. Ein leeres Kapitel landete sonst
    /// als echtes im EPUB, und der Block darum wird nie neu gebaut.
    #[test]
    fn locked_and_empty_chapters_are_reported_not_stored() {
        for body in [
            r#"{"success":false,"code":"CHAPTER_LOCKED"}"#,
            r#"{"success":true,"chapter":{"locked":true},"data":{"data":{"body":["x"]}}}"#,
            r#"{"success":true,"data":{"data":{"body":["   "]}}}"#,
            r#"{"success":false,"requireTurnstile":true}"#,
        ] {
            let result = parse_chapter_body(body, "t", "https://wtr-lab.com/x");
            assert!(matches!(result, Err(FeroError::ExternalApi(_))), "{body}");
        }
    }
}
