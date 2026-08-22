//! # core::epub
//!
//! Minimal EPUB 3 writer used by the webnovel subscription engine.
//!
//! ## Why hand-rolled?
//! An EPUB is a zip container with a fixed skeleton: an uncompressed
//! `mimetype` entry first, `META-INF/container.xml`, an OPF package manifest,
//! a navigation document, and one XHTML file per chapter.  Writing those
//! templates directly keeps the dependency surface at just the `zip` crate
//! and gives us full control over the output.
//!
//! ## Responsibilities:
//! - Assemble chapters (already-sanitized XHTML bodies) into a valid EPUB 3
//! - Escape all metadata for XML safety
//!
//! ## Dependencies:
//! - `zip` – container writing

use std::fs::File;
use std::io::Write;
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{Result, VaultError};

/// One chapter to embed into the EPUB.
#[derive(Debug, Clone)]
pub struct EpubChapter {
    /// Chapter heading shown in the navigation document.
    pub title: String,
    /// Sanitized XHTML body fragment (block-level content only).
    pub xhtml_body: String,
}

/// Cover image embedded into the EPUB.
#[derive(Debug, Clone)]
pub struct EpubCover {
    /// Image MIME type (`image/jpeg`, `image/png`, `image/webp`).
    pub media_type: String,
    /// Raw image bytes.
    pub bytes: Vec<u8>,
}

impl EpubCover {
    /// File extension matching the media type.
    fn extension(&self) -> &'static str {
        match self.media_type.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            // JPEG is by far the most common cover format; unknown types are
            // still written, readers sniff the actual bytes anyway.
            _ => "jpg",
        }
    }
}

/// Book-level metadata written into the OPF package document.
#[derive(Debug, Clone)]
pub struct EpubMeta {
    /// Book title.
    pub title: String,
    /// Author, if known.
    pub author: Option<String>,
    /// BCP-47 language code, e.g. `en` or `de`.
    pub language: String,
    /// Stable unique identifier (source URL or a URN).
    pub identifier: String,
    /// Optional description shown by readers.
    pub description: Option<String>,
    /// Optional cover image shown by readers and library views.
    pub cover: Option<EpubCover>,
}

/// Writes a complete EPUB 3 file to `target`, replacing any existing file.
///
/// The zip layout follows the EPUB OCF spec: the first entry is the
/// `mimetype` file stored without compression, followed by the container
/// descriptor, the OPF package, the navigation document, and the chapters.
///
/// # Parameters
/// - `target` – Destination path for the `.epub` file
/// - `meta` – Book metadata for the OPF package
/// - `chapters` – Chapters in reading order; bodies must be valid XHTML
///
/// # Errors
/// - `VaultError::InvalidProperty` if `chapters` is empty
/// - `VaultError::Io` on filesystem or zip write failures
pub fn write_epub(target: &Path, meta: &EpubMeta, chapters: &[EpubChapter]) -> Result<()> {
    if chapters.is_empty() {
        return Err(VaultError::InvalidProperty(
            "cannot write an EPUB without chapters".to_string(),
        ));
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(VaultError::from)?;
    }

    let file = File::create(target).map_err(VaultError::from)?;
    let mut zip = ZipWriter::new(file);

    // The `mimetype` entry must be first and stored uncompressed so readers
    // can sniff the container type from the raw bytes (EPUB OCF requirement).
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored).map_err(zip_error)?;
    zip.write_all(b"application/epub+zip")
        .map_err(VaultError::from)?;

    zip.start_file("META-INF/container.xml", deflated)
        .map_err(zip_error)?;
    zip.write_all(CONTAINER_XML.as_bytes())
        .map_err(VaultError::from)?;

    zip.start_file("OEBPS/content.opf", deflated)
        .map_err(zip_error)?;
    zip.write_all(render_opf(meta, chapters).as_bytes())
        .map_err(VaultError::from)?;

    zip.start_file("OEBPS/nav.xhtml", deflated)
        .map_err(zip_error)?;
    zip.write_all(render_nav(meta, chapters).as_bytes())
        .map_err(VaultError::from)?;

    if let Some(cover) = &meta.cover {
        // Images are already compressed; store them without re-deflating.
        zip.start_file(format!("OEBPS/cover.{}", cover.extension()), stored)
            .map_err(zip_error)?;
        zip.write_all(&cover.bytes).map_err(VaultError::from)?;
    }

    for (index, chapter) in chapters.iter().enumerate() {
        zip.start_file(chapter_file_name(index), deflated)
            .map_err(zip_error)?;
        zip.write_all(render_chapter(chapter).as_bytes())
            .map_err(VaultError::from)?;
    }

    zip.finish().map_err(zip_error)?;
    Ok(())
}

/// Escapes text for embedding into XML/XHTML element content or attributes.
pub fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

/// Zip entry name for the chapter at `index` (0-based).
fn chapter_file_name(index: usize) -> String {
    // 1-based, zero-padded names keep alphabetical order equal to reading order.
    format!("OEBPS/chapter_{:04}.xhtml", index + 1)
}

/// Href of a chapter relative to the OPF file.
fn chapter_href(index: usize) -> String {
    format!("chapter_{:04}.xhtml", index + 1)
}

fn zip_error(error: zip::result::ZipError) -> VaultError {
    VaultError::Io(format!("epub zip write failed: {error}"))
}

fn render_opf(meta: &EpubMeta, chapters: &[EpubChapter]) -> String {
    let mut manifest = String::new();
    let mut spine = String::new();
    for index in 0..chapters.len() {
        let id = format!("chapter{:04}", index + 1);
        let href = chapter_href(index);
        manifest.push_str(&format!(
            "    <item id=\"{id}\" href=\"{href}\" media-type=\"application/xhtml+xml\"/>\n"
        ));
        spine.push_str(&format!("    <itemref idref=\"{id}\"/>\n"));
    }

    if let Some(cover) = &meta.cover {
        manifest.push_str(&format!(
            "    <item id=\"cover-image\" href=\"cover.{ext}\" media-type=\"{media_type}\" properties=\"cover-image\"/>\n",
            ext = cover.extension(),
            media_type = escape_xml(&cover.media_type),
        ));
    }
    // Legacy EPUB 2 cover hint so older readers find the image too.
    let cover_meta = if meta.cover.is_some() {
        "    <meta name=\"cover\" content=\"cover-image\"/>\n"
    } else {
        ""
    };

    let author = meta
        .author
        .as_deref()
        .map(|author| format!("    <dc:creator>{}</dc:creator>\n", escape_xml(author)))
        .unwrap_or_default();
    let description = meta
        .description
        .as_deref()
        .map(|text| {
            format!(
                "    <dc:description>{}</dc:description>\n",
                escape_xml(text)
            )
        })
        .unwrap_or_default();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="pub-id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="pub-id">{identifier}</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:language>{language}</dc:language>
{author}{description}{cover_meta}    <meta property="dcterms:modified">2000-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
{manifest}  </manifest>
  <spine>
{spine}  </spine>
</package>
"#,
        identifier = escape_xml(&meta.identifier),
        title = escape_xml(&meta.title),
        language = escape_xml(&meta.language),
    )
}

fn render_nav(meta: &EpubMeta, chapters: &[EpubChapter]) -> String {
    let mut items = String::new();
    for (index, chapter) in chapters.iter().enumerate() {
        items.push_str(&format!(
            "        <li><a href=\"{href}\">{title}</a></li>\n",
            href = chapter_href(index),
            title = escape_xml(&chapter.title),
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="{language}">
  <head>
    <title>{title}</title>
  </head>
  <body>
    <nav epub:type="toc">
      <h1>{title}</h1>
      <ol>
{items}      </ol>
    </nav>
  </body>
</html>
"#,
        language = escape_xml(&meta.language),
        title = escape_xml(&meta.title),
    )
}

fn render_chapter(chapter: &EpubChapter) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>{title}</title>
  </head>
  <body>
    <h2>{title}</h2>
{body}
  </body>
</html>
"#,
        title = escape_xml(&chapter.title),
        body = chapter.xhtml_body,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample_meta() -> EpubMeta {
        EpubMeta {
            title: "Test & Novel".to_string(),
            author: Some("Jane <Doe>".to_string()),
            language: "en".to_string(),
            identifier: "https://example.com/novel?id=1&x=2".to_string(),
            description: Some("A \"test\" novel".to_string()),
            cover: None,
        }
    }

    fn sample_chapters() -> Vec<EpubChapter> {
        vec![
            EpubChapter {
                title: "Chapter 1".to_string(),
                xhtml_body: "<p>Hello world.</p>".to_string(),
            },
            EpubChapter {
                title: "Chapter 2 & More".to_string(),
                xhtml_body: "<p>Second chapter.</p>".to_string(),
            },
        ]
    }

    #[test]
    fn writes_valid_container_layout() {
        let dir = std::env::temp_dir().join(format!("epub-test-{}", std::process::id()));
        let target = dir.join("test.epub");
        write_epub(&target, &sample_meta(), &sample_chapters()).expect("epub should write");

        let file = File::open(&target).expect("epub should open");
        let mut archive = zip::ZipArchive::new(file).expect("epub should be a zip");

        // The mimetype entry must be the FIRST entry and stored uncompressed.
        {
            let first = archive.by_index(0).expect("first entry should exist");
            assert_eq!(first.name(), "mimetype");
            assert_eq!(first.compression(), CompressionMethod::Stored);
        }
        {
            let mut first = archive.by_index(0).expect("first entry should re-open");
            let mut mimetype = String::new();
            first
                .read_to_string(&mut mimetype)
                .expect("mimetype should read");
            assert_eq!(mimetype, "application/epub+zip");
        }

        for name in [
            "META-INF/container.xml",
            "OEBPS/content.opf",
            "OEBPS/nav.xhtml",
            "OEBPS/chapter_0001.xhtml",
            "OEBPS/chapter_0002.xhtml",
        ] {
            archive.by_name(name).expect("entry should exist");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opf_escapes_metadata() {
        let opf = render_opf(&sample_meta(), &sample_chapters());
        assert!(opf.contains("Test &amp; Novel"));
        assert!(opf.contains("Jane &lt;Doe&gt;"));
        assert!(opf.contains("id=1&amp;x=2"));
        assert!(!opf.contains("Jane <Doe>"));
    }

    #[test]
    fn nav_lists_all_chapters_in_order() {
        let nav = render_nav(&sample_meta(), &sample_chapters());
        let first = nav.find("chapter_0001.xhtml").expect("chapter 1 in nav");
        let second = nav.find("chapter_0002.xhtml").expect("chapter 2 in nav");
        assert!(first < second);
        assert!(nav.contains("Chapter 2 &amp; More"));
    }

    #[test]
    fn embeds_cover_image() {
        let mut meta = sample_meta();
        meta.cover = Some(EpubCover {
            media_type: "image/jpeg".to_string(),
            bytes: vec![0xFF, 0xD8, 0xFF, 0xE0],
        });

        let dir = std::env::temp_dir().join(format!("epub-cover-{}", std::process::id()));
        let target = dir.join("cover.epub");
        write_epub(&target, &meta, &sample_chapters()).expect("epub should write");

        let file = File::open(&target).expect("epub should open");
        let mut archive = zip::ZipArchive::new(file).expect("epub should be a zip");
        archive
            .by_name("OEBPS/cover.jpg")
            .expect("cover entry should exist");

        let opf = render_opf(&meta, &sample_chapters());
        assert!(opf.contains("properties=\"cover-image\""));
        assert!(opf.contains("<meta name=\"cover\" content=\"cover-image\"/>"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_empty_chapter_list() {
        let dir = std::env::temp_dir().join("epub-test-empty");
        let target = dir.join("empty.epub");
        let result = write_epub(&target, &sample_meta(), &[]);
        assert!(result.is_err());
    }
}
