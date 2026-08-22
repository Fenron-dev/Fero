//! Import planning, dry-run output, and manual review prompts.

use std::path::PathBuf;

use crate::core::duplicate::FileFingerprint;
use crate::core::vault::RelativePath;
use crate::error::{Result, VaultError};
use crate::media::MediaType;

const DEFAULT_MANUAL_REVIEW_THRESHOLD: f32 = 0.75;
const DEFAULT_HIGH_CONFIDENCE_THRESHOLD: f32 = 0.90;

/// Configuration that controls dry-run and import planning behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportConfig {
    /// Confidence below this value sends the item to manual review.
    pub manual_review_threshold: f32,
    /// Confidence above this value can be auto-accepted.
    pub high_confidence_threshold: f32,
    /// Whether the planner should assume files will be moved into the vault.
    pub auto_move: bool,
    /// How duplicate files should be handled.
    pub duplicate_policy: DuplicatePolicy,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            manual_review_threshold: DEFAULT_MANUAL_REVIEW_THRESHOLD,
            high_confidence_threshold: DEFAULT_HIGH_CONFIDENCE_THRESHOLD,
            auto_move: true,
            duplicate_policy: DuplicatePolicy::AskUser,
        }
    }
}

/// How duplicates should be handled in the import plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicatePolicy {
    /// Stop and ask the user.
    AskUser,
    /// Keep both files.
    KeepBoth,
    /// Skip the new file.
    Skip,
}

/// The source that produced the current classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationSource {
    /// The user declared the type.
    User,
    /// The folder structure hinted at the type.
    Folder,
    /// The file name pattern hinted at the type.
    Filename,
    /// The extension hinted at the type.
    Extension,
    /// An API detected the type.
    Api,
    /// Local AI detected the type.
    Ai,
    /// The type has not been determined yet.
    Unknown,
}

/// Result of type detection.
#[derive(Debug, Clone, PartialEq)]
pub struct FileClassification {
    /// Suggested media type.
    pub media_type: MediaType,
    /// Detection confidence between 0 and 1.
    pub confidence: f32,
    /// Where the classification came from.
    pub source: ClassificationSource,
}

/// Metadata used to derive the final target path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedMetadata {
    /// Preferred title for naming.
    pub title: Option<String>,
    /// Optional year suffix.
    pub year: Option<u16>,
}

/// File imported from the inbox or another folder.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingFile {
    /// Vault-relative source path.
    pub source_path: RelativePath,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Fingerprint if it has already been computed.
    pub fingerprint: Option<FileFingerprint>,
    /// Current or suggested classification.
    pub classification: Option<FileClassification>,
    /// Metadata that can be used for naming.
    pub metadata: Option<ResolvedMetadata>,
}

impl IncomingFile {
    /// Creates a new incoming file descriptor.
    pub fn new(source_path: RelativePath, size_bytes: u64) -> Self {
        Self {
            source_path,
            size_bytes,
            fingerprint: None,
            classification: None,
            metadata: None,
        }
    }
}

/// One interactive prompt that may appear during import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPrompt {
    /// Name of the field to correct.
    pub field_name: String,
    /// Human-readable prompt text.
    pub message: String,
    /// Suggested options for the user.
    pub options: Vec<String>,
}

/// A single operation in the dry-run plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedImportStep {
    /// Detect the media type.
    DetectType,
    /// Fetch provider metadata.
    FetchMetadata { provider: String },
    /// Ask the user to correct an ambiguous value.
    AskUser { prompt: UserPrompt },
    /// Move the media file.
    MoveFile { target: RelativePath },
    /// Write the YAML sidecar.
    WriteSidecar { target: RelativePath },
    /// Record audit information.
    RecordAudit,
    /// Register a duplicate fingerprint.
    RegisterDuplicate { fingerprint: String },
    /// Send the item to review.
    QueueReview { reason: String },
    /// Skip the item.
    Skip { reason: String },
}

/// Planned actions for one input file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportPlanItem {
    /// Source file path.
    pub source_path: RelativePath,
    /// Planned target path, if any.
    pub target_path: Option<RelativePath>,
    /// Optional fingerprint.
    pub fingerprint: Option<FileFingerprint>,
    /// Optional classification.
    pub classification: Option<FileClassification>,
    /// Whether the item requires user input.
    pub manual_review: bool,
    /// Duplicate target entry or fingerprint if known.
    pub duplicate_of: Option<String>,
    /// Ordered dry-run steps.
    pub steps: Vec<PlannedImportStep>,
}

/// Aggregated import statistics.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportSummary {
    /// Total number of files in the plan.
    pub total_files: usize,
    /// Number of files that need manual review.
    pub items_needing_review: usize,
    /// Number of duplicate files.
    pub duplicates: usize,
    /// Number of planned moves.
    pub planned_moves: usize,
    /// Number of planned sidecar writes.
    pub planned_sidecars: usize,
    /// Number of planned API fetches.
    pub planned_api_fetches: usize,
}

/// The complete dry-run plan for a batch import.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportPlan {
    /// Indicates that no files have been changed yet.
    pub dry_run: bool,
    /// Per-file plans.
    pub items: Vec<ImportPlanItem>,
    /// Aggregated statistics.
    pub summary: ImportSummary,
}

/// Produces dry-run plans for the import workflow.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportPlanner {
    config: ImportConfig,
}

impl ImportPlanner {
    /// Creates a planner with the provided configuration.
    pub fn new(config: ImportConfig) -> Self {
        Self { config }
    }

    /// Builds a dry-run plan for a single file.
    pub fn plan_file(&self, file: &IncomingFile) -> Result<ImportPlanItem> {
        let mut steps = vec![PlannedImportStep::DetectType];
        let mut manual_review = false;
        let duplicate_of = None;

        let classification = file.classification.clone();
        if let Some(classification) = classification.as_ref() {
            if let Some(provider) = classification.media_type.preferred_provider() {
                steps.push(PlannedImportStep::FetchMetadata {
                    provider: provider.to_string(),
                });
            }

            if classification.confidence < self.config.manual_review_threshold {
                manual_review = true;
                steps.push(PlannedImportStep::AskUser {
                    prompt: UserPrompt {
                        field_name: "media_type".to_string(),
                        message: "Kategorie oder Genre bestätigen".to_string(),
                        options: MediaType::all()
                            .iter()
                            .map(|value| value.to_string())
                            .collect(),
                    },
                });
                steps.push(PlannedImportStep::QueueReview {
                    reason: format!(
                        "confidence below threshold: {:.2}",
                        classification.confidence
                    ),
                });
            } else if classification.confidence < self.config.high_confidence_threshold {
                steps.push(PlannedImportStep::QueueReview {
                    reason: format!(
                        "confidence below high threshold: {:.2}",
                        classification.confidence
                    ),
                });
            }
        } else {
            manual_review = true;
            steps.push(PlannedImportStep::AskUser {
                prompt: UserPrompt {
                    field_name: "media_type".to_string(),
                    message: "Kategorie, Genre oder Name eintragen".to_string(),
                    options: MediaType::all()
                        .iter()
                        .map(|value| value.to_string())
                        .collect(),
                },
            });
            steps.push(PlannedImportStep::QueueReview {
                reason: "type not known yet".to_string(),
            });
        }

        if let Some(fingerprint) = file.fingerprint.as_ref() {
            // Key on hash *and* size: the hash is a 64-bit non-cryptographic
            // fold, so the length is what keeps an accidental collision from
            // marking two unrelated files as the same content.
            steps.push(PlannedImportStep::RegisterDuplicate {
                fingerprint: format!("{}:{}", fingerprint.hash, fingerprint.byte_len),
            });
        }

        let target_path = self.build_target_path(file)?;
        let should_move = self.config.auto_move
            && !manual_review
            && !matches!(self.config.duplicate_policy, DuplicatePolicy::Skip);

        if should_move {
            steps.push(PlannedImportStep::MoveFile {
                target: target_path.clone(),
            });
            steps.push(PlannedImportStep::WriteSidecar {
                target: target_path.clone(),
            });
        }

        steps.push(PlannedImportStep::RecordAudit);

        if !self.config.auto_move {
            steps.push(PlannedImportStep::Skip {
                reason: "auto move disabled in config".to_string(),
            });
        }

        Ok(ImportPlanItem {
            source_path: file.source_path.clone(),
            target_path: Some(target_path),
            fingerprint: file.fingerprint.clone(),
            classification,
            manual_review,
            duplicate_of,
            steps,
        })
    }

    /// Builds a dry-run plan for a full batch of files.
    pub fn plan_files(&self, files: &[IncomingFile]) -> Result<ImportPlan> {
        let mut items = Vec::with_capacity(files.len());
        let mut summary = ImportSummary::default();

        for file in files {
            let item = self.plan_file(file)?;
            summary.total_files += 1;
            if item.manual_review {
                summary.items_needing_review += 1;
            }
            if item.duplicate_of.is_some() {
                summary.duplicates += 1;
            }
            if item
                .steps
                .iter()
                .any(|step| matches!(step, PlannedImportStep::MoveFile { .. }))
            {
                summary.planned_moves += 1;
            }
            if item
                .steps
                .iter()
                .any(|step| matches!(step, PlannedImportStep::WriteSidecar { .. }))
            {
                summary.planned_sidecars += 1;
            }
            summary.planned_api_fetches += item
                .steps
                .iter()
                .filter(|step| matches!(step, PlannedImportStep::FetchMetadata { .. }))
                .count();
            items.push(item);
        }

        Ok(ImportPlan {
            dry_run: true,
            items,
            summary,
        })
    }

    fn build_target_path(&self, file: &IncomingFile) -> Result<RelativePath> {
        let classification = match file.classification.as_ref() {
            Some(classification) => classification,
            None => {
                return Self::build_inbox_target(file);
            }
        };

        if classification.media_type == MediaType::Unclassified
            || classification.confidence < self.config.manual_review_threshold
        {
            return Self::build_inbox_target(file);
        }

        let folder_segment = classification.media_type.folder_segment();
        let metadata = file.metadata.as_ref();
        let title = metadata
            .and_then(|data| data.title.as_deref())
            .map(sanitize_segment)
            .or_else(|| {
                file.source_path
                    .file_stem()
                    .map(|stem| sanitize_segment(&stem.to_string_lossy()))
            })
            .unwrap_or_else(|| "untitled".to_string());

        let mut folder = PathBuf::from(folder_segment);
        let year_suffix = metadata
            .and_then(|data| data.year)
            .map(|year| format!(" ({year})"))
            .unwrap_or_default();
        folder.push(format!("{title}{year_suffix}"));

        let original_file_name = file
            .source_path
            .file_name()
            .ok_or(VaultError::MissingFileName)?
            .to_string_lossy()
            .to_string();
        let extension = file
            .source_path
            .extension()
            .map(|ext| ext.to_string_lossy().to_string());
        let generated_file_name = match extension {
            Some(extension) if !extension.is_empty() => format!("{title}{year_suffix}.{extension}"),
            _ => original_file_name,
        };

        folder.push(generated_file_name);
        RelativePath::new(folder)
    }

    fn build_inbox_target(file: &IncomingFile) -> Result<RelativePath> {
        let inbox_name = file
            .source_path
            .file_name()
            .ok_or(VaultError::MissingFileName)?
            .to_string_lossy()
            .to_string();
        let inbox_target = PathBuf::from("Inbox").join(inbox_name);
        RelativePath::new(inbox_target)
    }
}

fn sanitize_segment(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());

    for character in value.chars() {
        let replacement = match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            control if control.is_control() => ' ',
            other => other,
        };
        sanitized.push(replacement);
    }

    sanitized
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vault::RelativePath;

    #[test]
    fn plans_unknown_files_for_manual_review() {
        let planner = ImportPlanner::new(ImportConfig::default());
        let file = IncomingFile::new(
            RelativePath::new("Inbox/unknown_file.mkv").expect("valid relative path"),
            1_024,
        );

        let plan = planner.plan_file(&file).expect("planning should succeed");
        assert!(plan.manual_review);
        assert!(matches!(
            plan.target_path.as_ref(),
            Some(path) if path.to_string().starts_with("Inbox/")
        ));
        assert!(plan
            .steps
            .iter()
            .any(|step| matches!(step, PlannedImportStep::AskUser { .. })));
    }

    #[test]
    fn plans_high_confidence_files_for_final_storage() {
        let planner = ImportPlanner::new(ImportConfig::default());
        let mut file = IncomingFile::new(
            RelativePath::new("Inbox/Violet Evergarden.mkv").expect("valid relative path"),
            1_024,
        );
        file.classification = Some(FileClassification {
            media_type: MediaType::Anime,
            confidence: 0.95,
            source: ClassificationSource::Api,
        });
        file.metadata = Some(ResolvedMetadata {
            title: Some("Violet Evergarden".to_string()),
            year: Some(2018),
        });

        let plan = planner.plan_file(&file).expect("planning should succeed");
        assert!(!plan.manual_review);
        assert!(matches!(
            plan.target_path.as_ref(),
            Some(path) if path.to_string().contains("Anime/")
        ));
        assert!(plan
            .steps
            .iter()
            .any(|step| matches!(step, PlannedImportStep::MoveFile { .. })));
    }
}
