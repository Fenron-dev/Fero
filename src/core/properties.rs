//! YAML sidecar generation and property path helpers.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::core::vault::RelativePath;
use crate::error::Result;
use crate::media::{MediaEntry, MediaProperties};

/// Suffix appended to a media file name to form its sidecar name.
pub const SIDECAR_SUFFIX: &str = ".fero.yaml";

/// Returns the relative sidecar path for a media file.
///
/// The suffix is **appended** to the full file name
/// (`Film.mkv` → `Film.mkv.fero.yaml`) rather than replacing the
/// extension. Replacing it made `Film.mkv` and `Film.mp4` share a single
/// sidecar, so importing both silently overwrote one file's metadata with the
/// other's.
///
/// See [`legacy_sidecar_path_for`] for the old shape, which is still read.
pub fn sidecar_path_for(media_path: &RelativePath) -> Result<RelativePath> {
    let file_name = media_path
        .file_name()
        .ok_or(crate::error::VaultError::MissingFileName)?
        .to_string_lossy()
        .to_string();

    let mut path = PathBuf::from(media_path.as_path());
    path.set_file_name(format!("{file_name}{SIDECAR_SUFFIX}"));
    RelativePath::new(path)
}

/// Returns the pre-1.0 sidecar path (`Film.mkv` → `Film.fero.yaml`).
///
/// Vaults written by earlier versions still contain these files; they are read
/// as a fallback and replaced the next time the sidecar is written.
pub fn legacy_sidecar_path_for(media_path: &RelativePath) -> Result<RelativePath> {
    let mut path = PathBuf::from(media_path.as_path());
    path.set_extension("fero.yaml");
    RelativePath::new(path)
}

/// Builds a YAML frontmatter document for a media entry.
pub fn render_sidecar_yaml(entry: &MediaEntry) -> Result<String> {
    let mut document = String::new();
    document.push_str("---\n");
    render_scalar(&mut document, "id", &entry.id);
    let media_type = entry.media_type.to_string();
    let source_file_path = entry.relative_path.to_string();
    render_scalar(&mut document, "media_type", &media_type);
    render_scalar(&mut document, "source_file_path", &source_file_path);
    render_scalar(&mut document, "original_filename", &entry.original_filename);

    render_properties(&mut document, &entry.properties);
    document.push_str("---\n");
    Ok(document)
}

fn render_properties(document: &mut String, properties: &MediaProperties) {
    render_optional_string(document, "title", properties.title.clone());
    render_optional_string(document, "title_de", properties.title_de.clone());
    render_optional_string(
        document,
        "title_original",
        properties.title_original.clone(),
    );
    render_optional_string(document, "author", properties.author.clone());
    render_optional_string(document, "description", properties.description.clone());
    render_optional_string(
        document,
        "description_de",
        properties.description_de.clone(),
    );
    render_optional_string(
        document,
        "description_original",
        properties.description_original.clone(),
    );
    render_optional_u64(document, "year", properties.year.map(u64::from));
    render_optional_string(document, "language", properties.language.clone());
    render_optional_string(
        document,
        "status",
        properties.status.map(|status| status.to_string()),
    );
    render_optional_f32(document, "rating", properties.rating);
    render_optional_f32(document, "rating_external", properties.rating_external);
    render_optional_u64(document, "anilist_id", properties.anilist_id.map(u64::from));
    render_optional_string(document, "anilist_url", properties.anilist_url.clone());
    render_optional_string(document, "series_title", properties.series_title.clone());
    render_optional_u64(
        document,
        "season_number",
        properties.season_number.map(u64::from),
    );
    render_optional_string(document, "season_name", properties.season_name.clone());
    render_optional_u64(
        document,
        "episode_start",
        properties.episode_start.map(u64::from),
    );
    render_optional_u64(
        document,
        "episode_end",
        properties.episode_end.map(u64::from),
    );
    render_optional_string(document, "episode_title", properties.episode_title.clone());
    render_optional_u64(
        document,
        "episode_count",
        properties.episode_count.map(u64::from),
    );
    render_optional_u64(
        document,
        "runtime_minutes",
        properties.runtime_minutes.map(u64::from),
    );
    render_optional_f32(document, "average_score", properties.average_score);
    render_optional_string(document, "format", properties.format.clone());
    render_optional_string(document, "airing_season", properties.airing_season.clone());
    render_optional_string(
        document,
        "cover_path",
        properties.cover_path.as_ref().map(|path| path.to_string()),
    );
    render_optional_string(
        document,
        "thumbnail_path",
        properties
            .thumbnail_path
            .as_ref()
            .map(|path| path.to_string()),
    );
    render_optional_string(document, "notes", properties.notes.clone());
    render_optional_string(
        document,
        "import_batch_id",
        properties.import_batch_id.clone(),
    );

    if !properties.tags.is_empty() {
        render_list(document, "tags", &properties.tags);
    }
    if !properties.genres.is_empty() {
        render_list(document, "genres", &properties.genres);
    }
    if !properties.categories.is_empty() {
        render_list(document, "categories", &properties.categories);
    }
    if !properties.custom_fields.is_empty() {
        render_map(document, "custom_fields", &properties.custom_fields);
    }
}

fn render_scalar(document: &mut String, key: &str, value: &str) {
    document.push_str(key);
    document.push_str(": ");
    document.push_str(&yaml_string(value));
    document.push('\n');
}

fn render_optional_string(document: &mut String, key: &str, value: Option<String>) {
    if let Some(value) = value {
        render_scalar(document, key, &value);
    }
}

fn render_optional_u64(document: &mut String, key: &str, value: Option<u64>) {
    if let Some(value) = value {
        document.push_str(key);
        document.push_str(": ");
        document.push_str(&value.to_string());
        document.push('\n');
    }
}

fn render_optional_f32(document: &mut String, key: &str, value: Option<f32>) {
    if let Some(value) = value {
        document.push_str(key);
        document.push_str(": ");
        document.push_str(&f32_to_yaml_number(value));
        document.push('\n');
    }
}

fn render_list(document: &mut String, key: &str, values: &[String]) {
    document.push_str(key);
    document.push_str(":\n");
    for value in values {
        document.push_str("  - ");
        document.push_str(&yaml_string(value));
        document.push('\n');
    }
}

fn render_map(document: &mut String, key: &str, values: &BTreeMap<String, String>) {
    document.push_str(key);
    document.push_str(":\n");
    for (map_key, map_value) in values {
        document.push_str("  ");
        document.push_str(&yaml_string(map_key));
        document.push_str(": ");
        document.push_str(&yaml_string(map_value));
        document.push('\n');
    }
}

fn yaml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn f32_to_yaml_number(value: f32) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vault::{RelativePath, Vault};
    use crate::media::{MediaEntry, MediaType, PropertySource};

    #[test]
    fn builds_sidecar_path_next_to_media_file() {
        let media_path =
            RelativePath::new("Anime/Violet Evergarden.mkv").expect("media path should be valid");
        let sidecar = sidecar_path_for(&media_path).expect("sidecar path should be valid");
        assert_eq!(sidecar.to_string(), "Anime/Violet Evergarden.mkv.fero.yaml");
    }

    /// Same stem, different container: both files must keep their own sidecar.
    #[test]
    fn sidecar_paths_do_not_collide_across_extensions() {
        let mkv = RelativePath::new("Filme/Film.mkv").expect("media path should be valid");
        let mp4 = RelativePath::new("Filme/Film.mp4").expect("media path should be valid");

        let mkv_sidecar = sidecar_path_for(&mkv).expect("sidecar path should be valid");
        let mp4_sidecar = sidecar_path_for(&mp4).expect("sidecar path should be valid");

        assert_ne!(mkv_sidecar, mp4_sidecar);
        assert_eq!(
            legacy_sidecar_path_for(&mkv).expect("legacy path should be valid"),
            legacy_sidecar_path_for(&mp4).expect("legacy path should be valid"),
            "the legacy scheme is exactly what collided"
        );
    }

    #[test]
    fn renders_portable_yaml_without_absolute_vault_paths() {
        let mut entry = MediaEntry::new(
            "entry-1",
            MediaType::Anime,
            RelativePath::new("Anime/Violet Evergarden.mkv").expect("valid relative path"),
            "Violet Evergarden.mkv",
        );
        entry.source = PropertySource::Api;
        entry.properties.title = Some("Violet Evergarden".to_string());
        entry.properties.year = Some(2018);
        entry.properties.tags = vec!["anime".to_string(), "drama".to_string()];

        let yaml = render_sidecar_yaml(&entry).expect("yaml should render");
        assert!(yaml.contains("source_file_path"));
        assert!(yaml.contains("Anime/Violet Evergarden.mkv"));
        assert!(!yaml.contains("/vault/"));
    }

    #[test]
    fn sidecar_renderer_accepts_vault_entries() {
        let _vault = Vault::new("/vault").expect("vault root should be valid");
        let entry = MediaEntry::new(
            "entry-2",
            MediaType::Book,
            RelativePath::new("Books/Dune.pdf").expect("valid relative path"),
            "Dune.pdf",
        );
        let yaml = render_sidecar_yaml(&entry).expect("yaml should render");
        assert!(yaml.contains("media_type"));
    }
}
