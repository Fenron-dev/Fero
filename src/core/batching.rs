//! Deciding which files a serial should be delivered as.
//!
//! The problem this solves: a serial with 4.000 chapters is unusable as 4.000
//! files, and unusable as one file that is rewritten every week — a rewrite
//! shifts every EPUB position, so the library loses where you were reading.
//!
//! The answer is blocks that never change once written, plus exactly one file
//! at the running edge that is allowed to change. Whoever reads inside the
//! marked `[WIP]` file knows it will move; everything else stays put forever.

/// How many chapters go into one block when the source has no volumes.
///
/// Fifty is a compromise: small enough that the running edge is finished
/// reasonably often, large enough that a long serial does not become hundreds
/// of files.
pub const DEFAULT_BATCH_SIZE: u32 = 50;

/// Whether a planned file is finished or still moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Full block — written once, then never touched again.
    Block,
    /// The running edge: rewritten on every run until it is full.
    Wip,
}

/// A file that should exist for a serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// File name inside the work folder, without a path.
    pub name: String,
    /// Chapter indices it contains, in reading order.
    pub chapters: Vec<u32>,
    /// Whether it is finished or still moving.
    pub kind: FileKind,
}

impl PlannedFile {
    /// Whether this file may be overwritten.
    pub fn is_rewritable(&self) -> bool {
        self.kind == FileKind::Wip
    }
}

/// Plans the block files for a serial.
///
/// `chapters` are the indices present locally, in any order. `batch_size` of
/// zero falls back to the default rather than dividing by zero or producing one
/// file per chapter.
///
/// Blocks are cut at fixed boundaries derived from the chapter *number*, not
/// from how many chapters happen to be downloaded: chapters 1–50 are always the
/// first block, even if number 7 arrives last. Otherwise a gap that fills in
/// later would renumber every block after it — and renaming a delivered file is
/// exactly what this design exists to prevent.
pub fn plan(title: &str, chapters: &[u32], batch_size: u32) -> Vec<PlannedFile> {
    let size = if batch_size == 0 {
        DEFAULT_BATCH_SIZE
    } else {
        batch_size
    };

    let mut sorted: Vec<u32> = chapters.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        return Vec::new();
    }

    let highest = *sorted.last().expect("checked non-empty");
    let mut files = Vec::new();

    // Walk fixed windows [1..size], [size+1..2*size], … up to the highest
    // chapter present.
    let mut start = 1u32;
    while start <= highest {
        let end = start.saturating_add(size - 1);
        let members: Vec<u32> = sorted
            .iter()
            .copied()
            .filter(|index| *index >= start && *index <= end)
            .collect();

        if !members.is_empty() {
            // A window counts as finished once its last chapter is there. A gap
            // in the middle is a chapter that will never arrive — waiting for it
            // forever would keep the block from ever being written.
            let complete = members.last() == Some(&end);
            files.push(PlannedFile {
                name: if complete {
                    format!("{title} - {start:04}-{end:04}.epub")
                } else {
                    format!("{title} - {start:04}+ [WIP].epub")
                },
                chapters: members,
                kind: if complete {
                    FileKind::Block
                } else {
                    FileKind::Wip
                },
            });
        }
        start = match start.checked_add(size) {
            Some(next) => next,
            None => break,
        };
    }

    files
}

/// Name of the single-file edition built once a serial is finished.
pub fn complete_edition_name(title: &str) -> String {
    format!("{title} - Gesamtausgabe.epub")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(files: &[PlannedFile]) -> Vec<&str> {
        files.iter().map(|file| file.name.as_str()).collect()
    }

    #[test]
    fn full_blocks_are_finished() {
        let files = plan("Titel", &(1..=100).collect::<Vec<_>>(), 50);

        assert_eq!(
            names(&files),
            ["Titel - 0001-0050.epub", "Titel - 0051-0100.epub"]
        );
        assert!(files.iter().all(|file| file.kind == FileKind::Block));
    }

    #[test]
    fn the_running_edge_is_marked() {
        let files = plan("Titel", &(1..=63).collect::<Vec<_>>(), 50);

        assert_eq!(
            names(&files),
            ["Titel - 0001-0050.epub", "Titel - 0051+ [WIP].epub"]
        );
        assert_eq!(files[0].kind, FileKind::Block);
        assert_eq!(files[1].kind, FileKind::Wip);
        assert!(files[1].is_rewritable());
        assert!(!files[0].is_rewritable());
    }

    /// The decisive property: a finished block keeps its name and contents when
    /// more chapters arrive. Otherwise every delivered file would move and the
    /// library would lose the reading position.
    #[test]
    fn finished_blocks_do_not_change_when_the_serial_grows() {
        let before = plan("Titel", &(1..=63).collect::<Vec<_>>(), 50);
        let after = plan("Titel", &(1..=140).collect::<Vec<_>>(), 50);

        assert_eq!(before[0], after[0], "the first block must be untouched");
        assert_eq!(after[1].name, "Titel - 0051-0100.epub");
    }

    /// Blocks are cut by chapter number, not by download order — a late arrival
    /// must not renumber anything.
    #[test]
    fn a_late_chapter_does_not_renumber_blocks() {
        let mut chapters: Vec<u32> = (1..=50).collect();
        chapters.remove(6); // chapter 7 missing for now
        let with_gap = plan("Titel", &chapters, 50);
        assert_eq!(
            with_gap[0].kind,
            FileKind::Block,
            "50 is present, so it is full"
        );

        let filled = plan("Titel", &(1..=50).collect::<Vec<_>>(), 50);
        assert_eq!(with_gap[0].name, filled[0].name);
    }

    /// A window whose last chapter is missing stays open — it is still growing.
    #[test]
    fn a_window_without_its_last_chapter_stays_open() {
        let files = plan("Titel", &(1..=49).collect::<Vec<_>>(), 50);
        assert_eq!(files[0].kind, FileKind::Wip);
        assert_eq!(files[0].name, "Titel - 0001+ [WIP].epub");
    }

    #[test]
    fn unordered_input_is_handled() {
        let files = plan("Titel", &[3, 1, 2], 3);
        assert_eq!(files[0].chapters, [1, 2, 3]);
        assert_eq!(files[0].kind, FileKind::Block);
    }

    #[test]
    fn duplicates_are_dropped() {
        let files = plan("Titel", &[1, 1, 2, 2], 2);
        assert_eq!(files[0].chapters, [1, 2]);
    }

    #[test]
    fn nothing_downloaded_plans_nothing() {
        assert!(plan("Titel", &[], 50).is_empty());
    }

    /// A batch size of zero must not divide by zero or make one file per
    /// chapter — it means "not configured".
    #[test]
    fn zero_batch_size_falls_back_to_the_default() {
        let files = plan("Titel", &(1..=DEFAULT_BATCH_SIZE).collect::<Vec<_>>(), 0);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind, FileKind::Block);
    }

    /// Chapters that start far from 1 must not produce a file per empty window.
    #[test]
    fn empty_windows_produce_no_files() {
        let files = plan("Titel", &[240, 241], 50);
        assert_eq!(names(&files), ["Titel - 0201+ [WIP].epub"]);
    }

    #[test]
    fn the_complete_edition_has_its_own_name() {
        assert_eq!(complete_edition_name("Titel"), "Titel - Gesamtausgabe.epub");
    }
}
