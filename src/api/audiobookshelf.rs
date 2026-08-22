//! # api::audiobookshelf
//!
//! Audiobookshelf (ABS) REST API client.
//!
//! ## Responsibilities:
//! - Connect to a local or remote ABS server using an API key
//! - List libraries and their items
//! - Read and write per-item playback progress
//!
//! ## Dependencies:
//! - `reqwest::blocking` – synchronous HTTP inside Tauri URI-scheme handlers

use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};

use crate::error::{Result, VaultError};

const REQUEST_TIMEOUT_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// ABS response types
// ---------------------------------------------------------------------------

/// A single ABS library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsLibrary {
    pub id: String,
    pub name: String,
    /// Media type string (e.g. `"book"`, `"podcast"`).
    #[serde(rename = "mediaType")]
    pub media_type: String,
}

/// Top-level response for `GET /api/libraries`.
#[derive(Debug, Deserialize)]
struct AbsLibrariesResponse {
    libraries: Vec<AbsLibrary>,
}

/// A slim item descriptor returned by `GET /api/libraries/{id}/items`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsLibraryItem {
    pub id: String,
    #[serde(default)]
    pub path: String,
    pub media: AbsItemMedia,
}

/// Media sub-object inside a library item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsItemMedia {
    pub metadata: AbsItemMetadata,
    #[serde(default)]
    pub duration: f64,
    #[serde(default, rename = "coverPath")]
    pub cover_path: String,
}

/// Core metadata for an ABS media item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsItemMetadata {
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "authorName")]
    pub author_name: String,
    #[serde(default, rename = "narratorName")]
    pub narrator_name: String,
}

/// Top-level response for `GET /api/libraries/{id}/items`.
#[derive(Debug, Deserialize)]
struct AbsLibraryItemsResponse {
    results: Vec<AbsLibraryItem>,
}

/// Progress record returned by `GET /api/me/progress/{item_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsProgress {
    #[serde(rename = "libraryItemId")]
    pub library_item_id: String,
    #[serde(rename = "currentTime", default)]
    pub current_time: f64,
    #[serde(rename = "duration", default)]
    pub duration: f64,
    #[serde(rename = "progress", default)]
    pub progress: f64,
    #[serde(rename = "isFinished", default)]
    pub is_finished: bool,
}

/// Payload sent to `PATCH /api/me/progress/{item_id}`.
#[derive(Serialize)]
struct AbsProgressPatch {
    #[serde(rename = "currentTime")]
    current_time: f64,
    duration: f64,
    #[serde(rename = "isFinished")]
    is_finished: bool,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Audiobookshelf API client.
///
/// Holds a base URL and API key for making authenticated requests.
pub struct AbsClient {
    base_url: String,
    api_key: String,
    client: Client,
}

impl AbsClient {
    /// Creates a new client.
    ///
    /// # Parameters
    /// - `base_url` – ABS server root, e.g. `http://localhost:13378`
    /// - `api_key` – ABS API key from user settings
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` if the HTTP client cannot be built.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| VaultError::ExternalApi(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            client,
        })
    }

    /// Verifies connectivity by fetching the server ping endpoint.
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` if the server cannot be reached or
    ///   returns an unexpected status code.
    pub fn test_connection(&self) -> Result<()> {
        let url = format!("{}/ping", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .map_err(|e| VaultError::ExternalApi(format!("connection failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(VaultError::ExternalApi(format!(
                "server returned status {}",
                resp.status()
            )));
        }
        Ok(())
    }

    /// Returns all libraries available on the ABS server.
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` on network or parse errors.
    pub fn list_libraries(&self) -> Result<Vec<AbsLibrary>> {
        let url = format!("{}/api/libraries", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .send()
            .map_err(|e| VaultError::ExternalApi(format!("list_libraries failed: {e}")))?;
        let body: AbsLibrariesResponse = resp
            .json()
            .map_err(|e| VaultError::ExternalApi(format!("list_libraries parse error: {e}")))?;
        Ok(body.libraries)
    }

    /// Returns all items in the given library.
    ///
    /// # Parameters
    /// - `library_id` – ABS library identifier
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` on network or parse errors.
    pub fn list_library_items(&self, library_id: &str) -> Result<Vec<AbsLibraryItem>> {
        let url = format!(
            "{}/api/libraries/{}/items?limit=100",
            self.base_url, library_id
        );
        let resp = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .send()
            .map_err(|e| VaultError::ExternalApi(format!("list_library_items failed: {e}")))?;
        let body: AbsLibraryItemsResponse = resp
            .json()
            .map_err(|e| VaultError::ExternalApi(format!("list_library_items parse error: {e}")))?;
        Ok(body.results)
    }

    /// Reads the current progress for a single item.
    ///
    /// Returns `None` if no progress record exists yet.
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` on network errors.
    pub fn get_progress(&self, item_id: &str) -> Result<Option<AbsProgress>> {
        let url = format!("{}/api/me/progress/{}", self.base_url, item_id);
        let resp = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .send()
            .map_err(|e| VaultError::ExternalApi(format!("get_progress failed: {e}")))?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let progress: AbsProgress = resp
            .json()
            .map_err(|e| VaultError::ExternalApi(format!("get_progress parse error: {e}")))?;
        Ok(Some(progress))
    }

    /// Writes playback progress for a single item to the ABS server.
    ///
    /// # Parameters
    /// - `item_id` – ABS library item identifier
    /// - `current_time` – playback position in seconds
    /// - `duration` – total duration in seconds (0 if unknown)
    /// - `is_finished` – whether the item has been completed
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` on network errors or non-2xx status.
    pub fn set_progress(
        &self,
        item_id: &str,
        current_time: f64,
        duration: f64,
        is_finished: bool,
    ) -> Result<()> {
        let url = format!("{}/api/me/progress/{}", self.base_url, item_id);
        let payload = AbsProgressPatch {
            current_time,
            duration,
            is_finished,
        };
        let resp = self
            .client
            .patch(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .json(&payload)
            .send()
            .map_err(|e| VaultError::ExternalApi(format!("set_progress failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(VaultError::ExternalApi(format!(
                "set_progress returned status {}",
                resp.status()
            )));
        }
        Ok(())
    }
}
