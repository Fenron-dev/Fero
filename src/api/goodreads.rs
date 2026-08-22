//! # api::goodreads
//!
//! Goodreads metadata lookup for webnovels and books.
//!
//! Goodreads retired its official API in 2020; this client uses two public
//! endpoints that remain stable:
//! 1. The autocomplete JSON (`/book/auto_complete?format=json&q=…`) for
//!    search — returns title, author, rating, cover, and the book URL.
//! 2. The book page itself, whose Next.js `__NEXT_DATA__` JSON blob carries
//!    the description and genre list.
//!
//! ## Dependencies:
//! - `api::novel::PoliteClient` – rate-limited HTTP

use serde::{Deserialize, Serialize};

use crate::api::novel::PoliteClient;
use crate::error::{Result, VaultError};

/// Autocomplete endpoint; `q` is appended URL-encoded.
const AUTOCOMPLETE_URL: &str = "https://www.goodreads.com/book/auto_complete?format=json&q=";
/// Base for relative book URLs from the autocomplete payload.
const GOODREADS_BASE: &str = "https://www.goodreads.com";

/// Book metadata assembled from Goodreads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoodreadsBookMetadata {
    /// Canonical Goodreads book URL.
    pub url: String,
    /// Book title.
    pub title: String,
    /// Primary author name.
    pub author: Option<String>,
    /// Average community rating (0.0–5.0).
    pub average_rating: Option<f32>,
    /// Number of ratings behind the average.
    pub ratings_count: Option<u32>,
    /// Cover image URL (full size where derivable).
    pub cover_url: Option<String>,
    /// Description text (plain, HTML stripped).
    pub description: Option<String>,
    /// Genre names from the book page.
    pub genres: Vec<String>,
}

/// Goodreads lookup client.
pub struct GoodreadsClient;

impl GoodreadsClient {
    /// Searches Goodreads and enriches the top hit with book-page details.
    ///
    /// Returns `Ok(None)` when nothing matches. Book-page parsing is best
    /// effort: if the page layout changes, the autocomplete fields alone are
    /// returned.
    ///
    /// # Errors
    /// - `VaultError::ExternalApi` when the search endpoint is unreachable
    pub fn search_book(
        client: &PoliteClient,
        title: &str,
    ) -> Result<Option<GoodreadsBookMetadata>> {
        let query = urlencoding::encode(title);
        let url = format!("{AUTOCOMPLETE_URL}{query}");
        let (_final_url, body) = client.get_text(&url)?;

        let hits: Vec<AutoCompleteHit> = serde_json::from_str(&body)
            .map_err(|e| VaultError::ExternalApi(format!("Goodreads-Suche unlesbar: {e}")))?;
        let Some(hit) = hits.into_iter().next() else {
            return Ok(None);
        };

        let book_url = if hit.book_url.starts_with("http") {
            hit.book_url.clone()
        } else {
            format!("{GOODREADS_BASE}{}", hit.book_url)
        };

        let mut metadata = GoodreadsBookMetadata {
            url: book_url.clone(),
            title: hit.book_title_bare.or(Some(hit.title)).unwrap_or_default(),
            author: hit.author.map(|author| author.name),
            average_rating: hit.avg_rating.and_then(|rating| rating.parse().ok()),
            ratings_count: hit.ratings_count,
            cover_url: hit.image_url.map(|url| full_size_cover(&url)),
            description: None,
            genres: Vec::new(),
        };

        // Description and genres live only on the book page; failures here
        // must not discard the search result.
        if let Ok((_page_url, page)) = client.get_text(&book_url) {
            if let Some(details) = parse_book_page(&page) {
                metadata.description = details.description;
                if !details.genres.is_empty() {
                    metadata.genres = details.genres;
                }
                if metadata.cover_url.is_none() {
                    metadata.cover_url = details.cover_url;
                }
            }
        }

        Ok(Some(metadata))
    }
}

/// Removes Goodreads thumbnail size markers ("._SY75_" etc.) from a cover URL.
fn full_size_cover(url: &str) -> String {
    // Thumbnails look like "…/61859147._SY75_.jpg"; stripping the marker
    // yields the original resolution.
    let mut result = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(start) = rest.find("._") {
        if let Some(len) = rest[start + 1..].find('.') {
            result.push_str(&rest[..start]);
            rest = &rest[start + 1 + len..];
        } else {
            break;
        }
    }
    result.push_str(rest);
    result
}

#[derive(Debug, Deserialize)]
struct AutoCompleteHit {
    title: String,
    #[serde(rename = "bookTitleBare")]
    book_title_bare: Option<String>,
    #[serde(rename = "bookUrl")]
    book_url: String,
    #[serde(rename = "avgRating")]
    avg_rating: Option<String>,
    #[serde(rename = "ratingsCount")]
    ratings_count: Option<u32>,
    #[serde(rename = "imageUrl")]
    image_url: Option<String>,
    author: Option<AutoCompleteAuthor>,
}

#[derive(Debug, Deserialize)]
struct AutoCompleteAuthor {
    name: String,
}

/// Fields extracted from the book page's `__NEXT_DATA__` JSON.
struct BookPageDetails {
    description: Option<String>,
    genres: Vec<String>,
    cover_url: Option<String>,
}

/// Extracts description/genres from the embedded Next.js state.
///
/// The Apollo cache inside `__NEXT_DATA__` holds `Book:…` entities; the one
/// with a `description` is the page's main book.
fn parse_book_page(page: &str) -> Option<BookPageDetails> {
    let start_marker = "<script id=\"__NEXT_DATA__\" type=\"application/json\">";
    let start = page.find(start_marker)? + start_marker.len();
    let end = page[start..].find("</script>")? + start;
    let json: serde_json::Value = serde_json::from_str(&page[start..end]).ok()?;

    let apollo = json
        .get("props")?
        .get("pageProps")?
        .get("apolloState")?
        .as_object()?;

    for (key, value) in apollo {
        if !key.starts_with("Book:") {
            continue;
        }
        let Some(description) = value.get("description").and_then(|d| d.as_str()) else {
            continue;
        };
        let genres = value
            .get("bookGenres")
            .and_then(|genres| genres.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("genre")?.get("name")?.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let cover_url = value
            .get("imageUrl")
            .and_then(|url| url.as_str())
            .map(str::to_string);
        return Some(BookPageDetails {
            description: Some(strip_html(description)),
            genres,
            cover_url,
        });
    }
    None
}

/// Drops HTML tags from Goodreads descriptions, keeping the text.
fn strip_html(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_tag = false;
    for ch in value.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                if in_tag {
                    in_tag = false;
                    // Tag boundaries often join words; keep them separated.
                    if !result.ends_with(' ') && !result.is_empty() {
                        result.push(' ');
                    }
                }
            }
            other if !in_tag => result.push(other),
            _ => {}
        }
    }
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_autocomplete_payload() {
        let payload = r#"[{"imageUrl":"https://i.gr-assets.com/books/123._SY75_.jpg",
            "bookId":"61859147","bookUrl":"/book/show/61859147-shadow-slave",
            "title":"Shadow Slave: Book1","bookTitleBare":"Shadow Slave: Book1",
            "avgRating":"4.56","ratingsCount":3356,
            "author":{"id":1,"name":"Guiltythree"}}]"#;
        let hits: Vec<AutoCompleteHit> = serde_json::from_str(payload).expect("should parse");
        assert_eq!(
            hits[0].author.as_ref().map(|a| a.name.as_str()),
            Some("Guiltythree")
        );
        assert_eq!(hits[0].avg_rating.as_deref(), Some("4.56"));
    }

    #[test]
    fn strips_thumbnail_size_markers() {
        assert_eq!(
            full_size_cover("https://x.com/books/123._SY75_.jpg"),
            "https://x.com/books/123.jpg"
        );
        assert_eq!(
            full_size_cover("https://x.com/books/123.jpg"),
            "https://x.com/books/123.jpg"
        );
    }

    #[test]
    fn parses_book_page_next_data() {
        let page = r#"<html><script id="__NEXT_DATA__" type="application/json">
        {"props":{"pageProps":{"apolloState":{
            "Book:kca://book/1": {"title":"X","description":"<b>Bold</b> story text.",
                "imageUrl":"https://img/x.jpg",
                "bookGenres":[{"genre":{"name":"Fantasy"}},{"genre":{"name":"Progression"}}]}
        }}}}</script></html>"#;
        let details = parse_book_page(page).expect("should parse");
        assert_eq!(details.description.as_deref(), Some("Bold story text."));
        assert_eq!(
            details.genres,
            vec!["Fantasy".to_string(), "Progression".to_string()]
        );
    }

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(strip_html("a<br>b <i>c</i>"), "a b c");
    }
}
