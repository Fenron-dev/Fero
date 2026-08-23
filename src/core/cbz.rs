//! # core::cbz
//!
//! CBZ (comic book zip) writer used by the manga subscription engine.
//!
//! ## Why hand-rolled?
//! A CBZ is nothing but a zip archive of page images that readers display in
//! file-name order, plus an optional `ComicInfo.xml` metadata entry.  Writing
//! it directly keeps the dependency surface at the `zip` crate that
//! [`crate::core::epub`] already uses, and gives full control over page
//! ordering — the one thing a comic reader cannot recover on its own.
//!
//! ## Page ordering
//! Pages are stored as `0001.jpg`, `0002.png`, … in the order supplied by the
//! caller.  Zero-padding to four digits is what makes the plain lexicographic
//! sort every reader applies match reading order; three digits would break at
//! chapter 1000 for long-running series.
//!
//! ## Metadata
//! `ComicInfo.xml` follows the ComicRack schema, which Komga, Kavita, Mihon
//! and YACReader all read.  It is written first so readers that stream the
//! archive find it without scanning to the end.
//!
//! ## Responsibilities:
//! - Assemble downloaded page images into a valid CBZ
//! - Escape all metadata for XML safety
//!
//! ## Dependencies:
//! - `zip` – container writing
//! - `core::epub` – shared XML escaping

use std::fs::File;
use std::io::Write;
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::core::epub::escape_xml;
use crate::error::{Result, FeroError};

/// Width of the zero-padded page number in archive entry names.
const PAGE_NUMBER_WIDTH: usize = 4;

/// One page image to embed into the archive.
#[derive(Debug, Clone)]
pub struct CbzPage {
    /// Image MIME type (`image/jpeg`, `image/png`, `image/webp`, `image/gif`).
    pub media_type: String,
    /// Raw image bytes as downloaded.
    pub bytes: Vec<u8>,
}

impl CbzPage {
    /// File extension matching the media type.
    fn extension(&self) -> &'static str {
        match self.media_type.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            "image/gif" => "gif",
            // JPEG dominates scanlation output; unknown types are still
            // written, readers sniff the actual bytes anyway.
            _ => "jpg",
        }
    }
}

/// Chapter-level metadata written into `ComicInfo.xml`.
#[derive(Debug, Clone, Default)]
pub struct CbzMeta {
    /// Series name, e.g. `Barakamon`.
    pub series: String,
    /// Chapter title, when the source names chapters beyond their number.
    pub title: Option<String>,
    /// Chapter number as shown by the source (kept as text: sources use
    /// `10.5` for extras, which is not an integer).
    pub number: Option<String>,
    /// Volume label, when the source groups chapters into volumes.
    pub volume: Option<String>,
    /// Series synopsis.
    pub summary: Option<String>,
    /// Writer/author.
    pub writer: Option<String>,
    /// Illustrator, where the source separates it from the writer.
    pub penciller: Option<String>,
    /// Genre names.
    pub genres: Vec<String>,
    /// Source URL of the chapter, for provenance.
    pub web: Option<String>,
    /// ISO language code of this release, e.g. `en`.
    pub language: Option<String>,
    /// Right-to-left reading direction (true for Japanese manga).
    pub right_to_left: bool,
}

/// Writes a CBZ file to `target`, replacing any existing file.
///
/// # Parameters
/// - `target` – Destination path for the `.cbz` file
/// - `meta` – Chapter metadata for `ComicInfo.xml`
/// - `pages` – Page images in reading order
///
/// # Returns
/// - `Ok(())` – Archive written and flushed
///
/// # Errors
/// - `FeroError::InvalidProperty` if `pages` is empty
/// - `FeroError::Io` on filesystem or zip write failures
pub fn write_cbz(target: &Path, meta: &CbzMeta, pages: &[CbzPage]) -> Result<()> {
    if pages.is_empty() {
        return Err(FeroError::InvalidProperty(
            "cannot write a CBZ without pages".to_string(),
        ));
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(FeroError::from)?;
    }

    let file = File::create(target).map_err(FeroError::from)?;
    let mut zip = ZipWriter::new(file);

    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    // Page images arrive already compressed (JPEG/PNG/WebP); deflating them
    // again costs CPU and grows the archive, so they are stored verbatim.
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    zip.start_file("ComicInfo.xml", deflated)
        .map_err(zip_error)?;
    zip.write_all(render_comic_info(meta, pages.len()).as_bytes())
        .map_err(FeroError::from)?;

    for (index, page) in pages.iter().enumerate() {
        zip.start_file(page_file_name(index, page), stored)
            .map_err(zip_error)?;
        zip.write_all(&page.bytes).map_err(FeroError::from)?;
    }

    zip.finish().map_err(zip_error)?;
    Ok(())
}

/// Archive entry name for the page at `index` (0-based).
fn page_file_name(index: usize, page: &CbzPage) -> String {
    format!(
        "{:0width$}.{}",
        index + 1,
        page.extension(),
        width = PAGE_NUMBER_WIDTH
    )
}

/// Renders the `ComicInfo.xml` document.
fn render_comic_info(meta: &CbzMeta, page_count: usize) -> String {
    let mut fields = String::new();

    let mut push = |tag: &str, value: &str| {
        if !value.is_empty() {
            fields.push_str(&format!("  <{tag}>{}</{tag}>\n", escape_xml(value)));
        }
    };

    push("Series", &meta.series);
    if let Some(title) = &meta.title {
        push("Title", title);
    }
    if let Some(number) = &meta.number {
        push("Number", number);
    }
    if let Some(volume) = &meta.volume {
        push("Volume", volume);
    }
    if let Some(summary) = &meta.summary {
        push("Summary", summary);
    }
    if let Some(writer) = &meta.writer {
        push("Writer", writer);
    }
    if let Some(penciller) = &meta.penciller {
        push("Penciller", penciller);
    }
    if !meta.genres.is_empty() {
        push("Genre", &meta.genres.join(", "));
    }
    if let Some(web) = &meta.web {
        push("Web", web);
    }
    if let Some(language) = &meta.language {
        push("LanguageISO", language);
    }
    push("PageCount", &page_count.to_string());
    // ComicRack encodes reading direction in the `Manga` field; readers use it
    // to decide whether page 2 goes left or right of page 1 in spread view.
    push(
        "Manga",
        if meta.right_to_left {
            "YesAndRightToLeft"
        } else {
            "Yes"
        },
    );

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <ComicInfo xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
         xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\n\
         {fields}</ComicInfo>\n"
    )
}

/// Converts a zip error into the shared vault error type.
fn zip_error(error: zip::result::ZipError) -> FeroError {
    FeroError::Io(format!("cbz write failed: {error}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn page(media_type: &str) -> CbzPage {
        CbzPage {
            media_type: media_type.to_string(),
            bytes: vec![0xFF, 0xD8, 0xFF, 0x00],
        }
    }

    fn temp_target(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cbz-test-{label}-{}.cbz", std::process::id()))
    }

    #[test]
    fn pages_are_named_in_sortable_reading_order() {
        assert_eq!(page_file_name(0, &page("image/jpeg")), "0001.jpg");
        assert_eq!(page_file_name(9, &page("image/png")), "0010.png");
        assert_eq!(page_file_name(1233, &page("image/webp")), "1234.webp");

        // The padding must keep lexicographic order equal to reading order.
        let mut names: Vec<String> = (0..12)
            .map(|index| page_file_name(index, &page("image/jpeg")))
            .collect();
        let expected = names.clone();
        names.sort();
        assert_eq!(names, expected);
    }

    #[test]
    fn empty_page_list_is_rejected() {
        let target = temp_target("empty");
        let result = write_cbz(&target, &CbzMeta::default(), &[]);
        assert!(matches!(result, Err(FeroError::InvalidProperty(_))));
        assert!(!target.exists());
    }

    #[test]
    fn writes_a_readable_archive_with_metadata() {
        let target = temp_target("write");
        let meta = CbzMeta {
            series: "Test & Serie".to_string(),
            title: Some("Kapitel <1>".to_string()),
            number: Some("10.5".to_string()),
            volume: Some("v02".to_string()),
            writer: Some("Autor".to_string()),
            genres: vec!["Action".to_string(), "Drama".to_string()],
            right_to_left: true,
            ..CbzMeta::default()
        };
        let pages = vec![page("image/jpeg"), page("image/png")];

        write_cbz(&target, &meta, &pages).expect("write should succeed");

        let file = File::open(&target).expect("archive should open");
        let mut archive = zip::ZipArchive::new(file).expect("archive should parse");
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        assert!(names.contains(&"ComicInfo.xml".to_string()));
        assert!(names.contains(&"0001.jpg".to_string()));
        assert!(names.contains(&"0002.png".to_string()));

        let mut xml = String::new();
        archive
            .by_name("ComicInfo.xml")
            .expect("metadata should exist")
            .read_to_string(&mut xml)
            .expect("metadata should read");
        // Metadata reaching the XML comes from scraped pages, so escaping is
        // what keeps a title with markup from breaking the document.
        assert!(xml.contains("<Series>Test &amp; Serie</Series>"));
        assert!(xml.contains("<Title>Kapitel &lt;1&gt;</Title>"));
        assert!(xml.contains("<Number>10.5</Number>"));
        assert!(xml.contains("<PageCount>2</PageCount>"));
        assert!(xml.contains("<Manga>YesAndRightToLeft</Manga>"));
        assert!(xml.contains("<Genre>Action, Drama</Genre>"));

        std::fs::remove_file(&target).ok();
    }
}
