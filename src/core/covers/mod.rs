//! Cover selection and fallback chain scaffolding.

use crate::core::vault::RelativePath;

/// Source used to obtain a cover image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoverSource {
    /// User-selected local file.
    User,
    /// External API cover artwork.
    Api,
    /// Locally generated AI artwork.
    Ai,
    /// Frame or artwork extracted from the media file.
    Extracted,
    /// Placeholder fallback.
    Placeholder,
}

/// A candidate cover together with its selection priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverCandidate {
    /// The cover source.
    pub source: CoverSource,
    /// Relative path to the cover file if one exists.
    pub path: Option<RelativePath>,
    /// Lower numbers win.
    pub priority: u8,
}

/// A simple ordered fallback chain for cover selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CoverFallbackChain {
    candidates: Vec<CoverCandidate>,
}

impl CoverFallbackChain {
    /// Creates a new fallback chain from a list of candidates.
    pub fn new(candidates: Vec<CoverCandidate>) -> Self {
        Self { candidates }
    }

    /// Adds a candidate to the chain.
    pub fn push(&mut self, candidate: CoverCandidate) {
        self.candidates.push(candidate);
    }

    /// Returns the best available candidate.
    pub fn best(&self) -> Option<&CoverCandidate> {
        self.candidates
            .iter()
            .min_by_key(|candidate| candidate.priority)
    }

    /// Returns all known candidates.
    pub fn candidates(&self) -> &[CoverCandidate] {
        &self.candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::vault::RelativePath;

    #[test]
    fn best_candidate_is_the_lowest_priority_value() {
        let chain = CoverFallbackChain::new(vec![
            CoverCandidate {
                source: CoverSource::Placeholder,
                path: None,
                priority: 3,
            },
            CoverCandidate {
                source: CoverSource::User,
                path: Some(RelativePath::new("covers/manual.jpg").expect("valid relative path")),
                priority: 0,
            },
        ]);

        assert!(matches!(chain.best(), Some(candidate) if candidate.source == CoverSource::User));
    }
}
