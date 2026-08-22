//! # api::audible
//!
//! Metadata lookup for audiobooks via the Audible catalog REST API.
//!
//! ## Responsibilities:
//! - Search for audiobooks by title on audible.de (primary) and audible.com (fallback)
//! - Return structured metadata including authors, narrators, cover images, and series info
//!
//! ## API notes:
//! - The Audible catalog API at `api.audible.{tld}/1.0/catalog/products` is publicly
//!   accessible without authentication for read operations.
//! - Audiobookshelf uses the same endpoint for its built-in metadata provider.
//!
//! ## Dependencies:
//! - `reqwest::blocking` – synchronous HTTP inside Tauri URI-scheme handlers

use std::collections::HashMap;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::error::{Result, VaultError};

const REQUEST_TIMEOUT_SECS: u64 = 15;
// `media` is required so the API returns the `product_images` map.
const RESPONSE_GROUPS: &str =
    "product_desc,product_attrs,contributors,rating,series,category_ladders,media";
// Request both sizes the frontend consumes (grid thumbnail + large cover).
const IMAGE_SIZE: &str = "500,1024";

// ---------------------------------------------------------------------------
// Response types (deserialized from Audible JSON)
// ---------------------------------------------------------------------------

/// A named contributor (author or narrator) on an Audible product.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleContributor {
    pub name: String,
    #[serde(default)]
    pub asin: String,
}

/// Overall rating distribution for an Audible product.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleRatingDistribution {
    #[serde(default)]
    pub display_stars: f32,
    #[serde(default)]
    pub num_ratings: u64,
}

/// Rating wrapper returned by the Audible API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleRating {
    pub overall_distribution: Option<AudibleRatingDistribution>,
}

/// One entry in a series returned by the Audible API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleSeriesEntry {
    pub title: String,
    pub sequence: Option<String>,
    pub asin: Option<String>,
}

/// A single rung inside a category ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleCategoryRung {
    pub name: String,
    #[serde(default)]
    pub id: String,
}

/// A full category ladder (root → leaf).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleCategoryLadder {
    pub ladder: Vec<AudibleCategoryRung>,
}

/// One audiobook result returned by the Audible catalog search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudibleProduct {
    pub asin: String,
    pub title: String,
    #[serde(default)]
    pub authors: Vec<AudibleContributor>,
    #[serde(default)]
    pub narrators: Vec<AudibleContributor>,
    pub publisher_name: Option<String>,
    /// HTML description – consumers must strip tags.
    pub publisher_summary: Option<String>,
    pub runtime_length_min: Option<u32>,
    pub release_date: Option<String>,
    /// Image URLs keyed by pixel size (e.g. "500" → URL).
    pub product_images: Option<HashMap<String, String>>,
    pub rating: Option<AudibleRating>,
    #[serde(default)]
    pub series: Vec<AudibleSeriesEntry>,
    #[serde(default)]
    pub category_ladders: Vec<AudibleCategoryLadder>,
}

impl AudibleProduct {
    /// Returns the best available cover image URL.
    pub fn best_cover_url(&self) -> Option<&str> {
        let images = self.product_images.as_ref()?;
        images
            .get("1024")
            .or_else(|| images.get("500"))
            .map(|s| s.as_str())
    }

    /// Returns the leaf category name from the first category ladder, if any.
    pub fn primary_genre(&self) -> Option<&str> {
        self.category_ladders
            .first()
            .and_then(|ladder| ladder.ladder.last())
            .map(|rung| rung.name.as_str())
    }
}

/// Top-level Audible API response for a product search.
#[derive(Debug, Deserialize)]
struct AudibleProductsResponse {
    products: Vec<AudibleProduct>,
}

// ---------------------------------------------------------------------------
// Response type for the Fero API endpoint
// ---------------------------------------------------------------------------

/// Serialized response returned to the frontend by `/api/audible-search`.
#[derive(Debug, Serialize)]
pub struct AudibleSearchResponse {
    /// First result (for auto-apply), if any.
    pub metadata: Option<AudibleProduct>,
    /// All results found.
    pub results: Vec<AudibleProduct>,
    pub error: Option<String>,
}

impl AudibleSearchResponse {
    pub fn error(msg: String) -> Self {
        Self {
            metadata: None,
            results: Vec::new(),
            error: Some(msg),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Audible catalog search client.
///
/// Tries `api.audible.de` first (German catalog) and falls back to
/// `api.audible.com` if the German endpoint returns no results.
pub struct AudibleClient {
    client: Client,
}

impl AudibleClient {
    /// Creates a new client.
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` if the HTTP client cannot be built.
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            // Audible returns 403 without a recognizable User-Agent.
            .user_agent("Mozilla/5.0 (compatible; Fero/1.0)")
            .build()
            .map_err(|e| VaultError::ExternalApi(format!("failed to build HTTP client: {e}")))?;
        Ok(Self { client })
    }

    /// Searches Audible for audiobooks matching `title` and optionally `author`.
    ///
    /// Tries the German store first. If it returns no results, retries on the
    /// US store as fallback so obscure titles are still found.
    ///
    /// # Parameters
    /// - `title` – book title (required)
    /// - `author` – optional author name to narrow results
    /// - `limit` – maximum number of results to return
    ///
    /// # Errors
    /// - Returns `VaultError::ExternalApi` if both regions fail.
    pub fn search(
        &self,
        title: &str,
        author: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AudibleProduct>> {
        match self.search_region(title, author, limit, "de") {
            Ok(results) if !results.is_empty() => return Ok(results),
            _ => {}
        }
        self.search_region(title, author, limit, "com")
    }

    fn search_region(
        &self,
        title: &str,
        author: Option<&str>,
        limit: usize,
        tld: &str,
    ) -> Result<Vec<AudibleProduct>> {
        let url = format!("https://api.audible.{tld}/1.0/catalog/products");
        let limit_str = limit.to_string();

        let mut params: Vec<(&str, &str)> = vec![
            ("title", title),
            ("num_results", limit_str.as_str()),
            ("response_groups", RESPONSE_GROUPS),
            ("image_sizes", IMAGE_SIZE),
        ];
        if let Some(auth) = author {
            params.push(("author", auth));
        }

        let resp = self.client.get(&url).query(&params).send().map_err(|e| {
            VaultError::ExternalApi(format!("Audible (.{tld}) request failed: {e}"))
        })?;

        if !resp.status().is_success() {
            return Err(VaultError::ExternalApi(format!(
                "Audible (.{tld}) returned HTTP {}",
                resp.status()
            )));
        }

        let body: AudibleProductsResponse = resp.json().map_err(|e| {
            VaultError::ExternalApi(format!("Audible (.{tld}) response parse error: {e}"))
        })?;

        Ok(body.products)
    }
}
