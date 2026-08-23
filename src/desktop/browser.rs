//! The visible foreign window: Cloudflare checks, site logins and the
//! embedded-browser fetch engine.
//!
//! These three belong together because they share one mechanism — a real window
//! the user can see and act in — and one piece of state: the captured session
//! per host. Splitting them would split that state.

use super::*;

// ---------------------------------------------------------------------------
// Interactive Cloudflare solve window
// ---------------------------------------------------------------------------

/// App handle captured from the URI-scheme context (set on first request).
pub(super) static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Per-host state of the solve flow: `pending`, `done`, `failed:<msg>`.
static SOLVE_STATES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Window label of the solve window (one at a time).
const SOLVE_WINDOW_LABEL: &str = "cf-solve";
/// Fixed browser user agent for the solve window AND the follow-up requests —
/// Cloudflare binds its clearance cookie to the user agent.
const SOLVE_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15";
/// How long the user gets to solve the challenge.
const SOLVE_TIMEOUT_SECS: u64 = 240;
/// Poll interval for the (local, cheap) cookie check.
const SOLVE_POLL_SECS: u64 = 3;
/// Grace period before probing sites that never issue an interactive
/// challenge (auto-pass) — probing too early burns a running challenge.
const SOLVE_GRACE_SECS: u64 = 15;
/// Probe attempts after a clearance cookie appeared. Each failed probe can
/// invalidate the clearance again (TLS binding), so we stop early with a
/// clear message instead of looping the user's challenge forever.
const SOLVE_MAX_PROBES: u32 = 3;

fn set_solve_state(host: &str, state: impl Into<String>) {
    if let Ok(mut states) = SOLVE_STATES.lock() {
        states.insert(host.to_string(), state.into());
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelSolveRequest {
    url: String,
}

// Steht hier statt in mod.rs: das Makro greift auf das private error-Feld zu
// und muss deshalb dort stehen, wo die Strukturen definiert sind.
impl_outcome!(WebnovelSolveResponse, WebnovelLoginResponse);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelSolveResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Opens a visible, isolated browser window on the target URL so the user can
/// solve the Cloudflare/captcha challenge manually. A background thread polls
/// the window's cookies; once `cf_clearance` appears the cookies (plus the
/// matching user agent) are stored for this host and the window closes.
///
/// Security: the window has no IPC capabilities and no access to app
/// internals — it is equivalent to opening the site in a normal browser.
pub(super) fn build_webnovel_solve_response(body: &[u8]) -> WebnovelSolveResponse {
    let req: WebnovelSolveRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return WebnovelSolveResponse {
                host: None,
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };
    if !req.url.starts_with("http://") && !req.url.starts_with("https://") {
        return WebnovelSolveResponse {
            host: None,
            error: Some("Nur http/https-Links erlaubt.".to_string()),
        };
    }
    let Some(host) = host_of(&req.url) else {
        return WebnovelSolveResponse {
            host: None,
            error: Some("URL ohne gültigen Host.".to_string()),
        };
    };
    let Some(handle) = APP_HANDLE.get().cloned() else {
        return WebnovelSolveResponse {
            host: None,
            error: Some("App-Fenster noch nicht bereit — bitte erneut versuchen.".to_string()),
        };
    };
    let Ok(target_url) = req.url.parse::<tauri::Url>() else {
        return WebnovelSolveResponse {
            host: None,
            error: Some("URL konnte nicht geparst werden.".to_string()),
        };
    };

    set_solve_state(&host, "pending");

    // Window creation must happen on the main thread on macOS.
    let build_handle = handle.clone();
    let build_url = target_url.clone();
    let _ = handle.run_on_main_thread(move || {
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        if let Some(existing) = build_handle.get_webview_window(SOLVE_WINDOW_LABEL) {
            let _ = existing.close();
        }
        let _ = WebviewWindowBuilder::new(
            &build_handle,
            SOLVE_WINDOW_LABEL,
            WebviewUrl::External(build_url),
        )
        .title("Sicherheitsprüfung bestätigen — Fenster schließt sich automatisch")
        .inner_size(1024.0, 820.0)
        .user_agent(SOLVE_USER_AGENT)
        .build();
    });

    // Poll on a worker thread: adopt whatever cookies the window has and
    // verify with a real probe request. Cloudflare sometimes waves the
    // WebView through WITHOUT a visible challenge (no cf_clearance cookie at
    // all) — only the probe tells us whether plain requests now pass.
    let poll_host = host.clone();
    let probe_url = req.url.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(SOLVE_TIMEOUT_SECS);
        let grace = std::time::Duration::from_secs(SOLVE_GRACE_SECS);
        let mut cycle: u64 = 0;
        let mut probes_after_clearance: u32 = 0;

        loop {
            std::thread::sleep(std::time::Duration::from_secs(SOLVE_POLL_SECS));
            cycle += 1;

            let window = handle.get_webview_window(SOLVE_WINDOW_LABEL);

            // Adopt current cookies (whatever they are) + matching UA.
            let mut has_clearance = false;
            if let Some(active) = window.as_ref() {
                if let Ok(cookies) = active.cookies_for_url(target_url.clone()) {
                    has_clearance = cookies.iter().any(|cookie| cookie.name() == "cf_clearance");
                    if !cookies.is_empty() {
                        let header = cookies
                            .iter()
                            .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
                            .collect::<Vec<_>>()
                            .join("; ");
                        set_browser_session(
                            &poll_host,
                            BrowserSession {
                                cookie_header: header,
                                user_agent: SOLVE_USER_AGENT.to_string(),
                            },
                        );
                    }
                }
            }

            // NEVER probe while an interactive challenge is still running:
            // a probe with freshly issued cookies but a different TLS
            // fingerprint makes Cloudflare revoke the clearance — the user
            // then sees the checkbox loop forever. Probe only once the
            // clearance cookie exists, or (auto-pass sites without any
            // interactive challenge) after a grace period, spaced out.
            let should_probe = if has_clearance {
                probes_after_clearance < SOLVE_MAX_PROBES
            } else {
                started.elapsed() >= grace && cycle.is_multiple_of(4)
            };

            if should_probe {
                if solve_probe_passes(&probe_url) {
                    set_solve_state(&poll_host, "done");
                    if let Some(active) = window {
                        let _ = active.close();
                    }
                    return;
                }
                if has_clearance {
                    probes_after_clearance += 1;
                    if probes_after_clearance >= SOLVE_MAX_PROBES {
                        set_solve_state(
                            &poll_host,
                            "failed:Die Freigabe ist strikt an das Browserfenster gebunden —                              App-Downloads bleiben für diese Seite blockiert.",
                        );
                        if let Some(active) = window {
                            let _ = active.close();
                        }
                        return;
                    }
                }
            }

            if window.is_none() {
                // Window closed by the user: one final probe decides.
                if solve_probe_passes(&probe_url) {
                    set_solve_state(&poll_host, "done");
                } else {
                    set_solve_state(
                        &poll_host,
                        "failed:Fenster geschlossen, Zugriff weiterhin blockiert.",
                    );
                }
                return;
            }
            if std::time::Instant::now() >= deadline {
                set_solve_state(&poll_host, "failed:Zeitüberschreitung.");
                if let Some(active) = handle.get_webview_window(SOLVE_WINDOW_LABEL) {
                    let _ = active.close();
                }
                return;
            }
        }
    });

    WebnovelSolveResponse {
        host: Some(host),
        error: None,
    }
}

/// One plain request against the blocked URL — success means the stored
/// session (or the now-trusted client) passes without a challenge.
fn solve_probe_passes(url: &str) -> bool {
    match PoliteClient::new() {
        Ok(client) => client.get_text(url).is_ok(),
        Err(_) => false,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelSolveStatusResponse {
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

pub(super) fn build_webnovel_solve_status_response(query: Option<&str>) -> WebnovelSolveStatusResponse {
    let host = query
        .and_then(|q| extract_query_value(q, "host"))
        .unwrap_or_default();
    let raw = SOLVE_STATES
        .lock()
        .ok()
        .and_then(|states| states.get(&host).cloned())
        .unwrap_or_else(|| "unknown".to_string());
    match raw.strip_prefix("failed:") {
        Some(message) => WebnovelSolveStatusResponse {
            state: "failed".to_string(),
            message: Some(message.to_string()),
        },
        None => WebnovelSolveStatusResponse {
            state: raw,
            message: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Site login (NovelUpdates & co. — session captured from a visible window)
// ---------------------------------------------------------------------------
//
// Some sites (NovelUpdates) only expose the real release/chapter links to
// logged-in users. The user signs in through a visible, sandboxed browser
// window; because every app webview shares the same cookie store, the routed
// render window is then logged in too. We additionally capture the login
// cookies into a persisted `BrowserSession` so plain requests carry them and
// the login survives an app restart.

/// Window label of the login window (one at a time).
const LOGIN_WINDOW_LABEL: &str = "mv-login";
/// How long the login window stays watched before giving up.
const LOGIN_TIMEOUT_SECS: u64 = 900;
/// Per-host login state: `pending`, `done`, `failed:<msg>`.
static LOGIN_STATES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn set_login_state(host: &str, state: impl Into<String>) {
    if let Ok(mut states) = LOGIN_STATES.lock() {
        states.insert(host.to_lowercase(), state.into());
    }
}

/// The sign-in page for a host (site root when unknown).
fn login_url_for(host: &str) -> String {
    if host.ends_with("novelupdates.com") {
        "https://www.novelupdates.com/login/".to_string()
    } else {
        format!("https://{host}/")
    }
}

/// Whether a cookie name marks an authenticated session. NovelUpdates runs on
/// WordPress (`wordpress_logged_in_<hash>`); a few common others are covered
/// for generic sites.
fn is_login_cookie(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("logged_in") || lower == "sessionid" || lower.starts_with("wordpress_sec")
}

/// Path of the persisted session store (`~/.fero/webnovel_sessions.json`).
fn webnovel_sessions_path() -> Option<PathBuf> {
    debug_log_path().and_then(|p| p.parent().map(|dir| dir.join("webnovel_sessions.json")))
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    host: String,
    cookie_header: String,
    user_agent: String,
}

fn load_stored_sessions() -> Vec<StoredSession> {
    webnovel_sessions_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persists the session store.
///
/// The file holds live login cookies for third-party sites, so it is written
/// with owner-only permissions — the default `0644` would expose every
/// captured session to any other user or process on the machine.
fn write_stored_sessions(sessions: &[StoredSession]) {
    let Some(path) = webnovel_sessions_path() else {
        return;
    };
    if let Ok(json) = serde_json::to_string_pretty(sessions) {
        let _ = write_private_file(&path, &json);
    }
}

/// Persists (and activates) a captured login session for a host.
fn persist_session(host: &str, session: &BrowserSession) {
    let host = host.to_lowercase();
    let mut all = load_stored_sessions();
    all.retain(|entry| entry.host != host);
    all.push(StoredSession {
        host: host.clone(),
        cookie_header: session.cookie_header.clone(),
        user_agent: session.user_agent.clone(),
    });
    write_stored_sessions(&all);
    set_browser_session(&host, session.clone());
}

/// Loads persisted sessions into the RAM store (called once at startup).
pub(super) fn restore_webnovel_sessions() {
    // Tighten permissions on stores written by older versions, which created
    // the file with the default (world-readable) mode.
    if let Some(path) = webnovel_sessions_path() {
        if path.exists() {
            restrict_to_owner(&path, false);
        }
        if let Some(parent) = path.parent() {
            restrict_to_owner(parent, true);
        }
    }

    for entry in load_stored_sessions() {
        set_browser_session(
            &entry.host,
            BrowserSession {
                cookie_header: entry.cookie_header,
                user_agent: entry.user_agent,
            },
        );
    }
}

/// Drops a host's persisted + active session (logout).
fn drop_session(host: &str) {
    let host = host.to_lowercase();
    let mut all = load_stored_sessions();
    all.retain(|entry| entry.host != host);
    write_stored_sessions(&all);
    clear_browser_session(&host);
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelLoginRequest {
    /// Host to log in to (e.g. "novelupdates.com"); a URL is also accepted.
    host: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelLoginResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Opens a visible login window for a host and captures the session cookies as
/// soon as the user is signed in.
pub(super) fn build_webnovel_login_response(body: &[u8]) -> WebnovelLoginResponse {
    let req: WebnovelLoginRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return WebnovelLoginResponse {
                host: None,
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };
    // Accept either a bare host or a full URL.
    let host = host_of(&req.host).unwrap_or_else(|| req.host.trim().to_lowercase());
    if host.is_empty() || host.contains(' ') {
        return WebnovelLoginResponse {
            host: None,
            error: Some("Ungültiger Host.".to_string()),
        };
    }
    let Some(handle) = APP_HANDLE.get().cloned() else {
        return WebnovelLoginResponse {
            host: None,
            error: Some("App-Fenster noch nicht bereit — bitte erneut versuchen.".to_string()),
        };
    };
    let Ok(login_url) = login_url_for(&host).parse::<tauri::Url>() else {
        return WebnovelLoginResponse {
            host: None,
            error: Some("Login-URL konnte nicht gebildet werden.".to_string()),
        };
    };

    set_login_state(&host, "pending");
    debug_log(&format!("login: Fenster geöffnet für {host}"));

    // Window creation must run on the main thread on macOS.
    let build_handle = handle.clone();
    let build_url = login_url.clone();
    let _ = handle.run_on_main_thread(move || {
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        if let Some(existing) = build_handle.get_webview_window(LOGIN_WINDOW_LABEL) {
            let _ = existing.close();
        }
        let _ = WebviewWindowBuilder::new(
            &build_handle,
            LOGIN_WINDOW_LABEL,
            WebviewUrl::External(build_url),
        )
        .title("Anmelden — nach dem Login kannst du dieses Fenster schließen")
        .inner_size(1024.0, 820.0)
        .user_agent(SOLVE_USER_AGENT)
        .build();
    });

    // Poll the window's cookies; capture the session once a login cookie shows
    // up. The window stays open (2FA, verification) until the user closes it.
    let poll_host = host.clone();
    let cookie_url = login_url.clone();
    std::thread::spawn(move || {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS);
        let mut captured = false;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let window = handle.get_webview_window(LOGIN_WINDOW_LABEL);

            if let Some(active) = window.as_ref() {
                if let Ok(cookies) = active.cookies_for_url(cookie_url.clone()) {
                    let logged_in = cookies.iter().any(|cookie| is_login_cookie(cookie.name()));
                    if logged_in {
                        let header = cookies
                            .iter()
                            .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
                            .collect::<Vec<_>>()
                            .join("; ");
                        persist_session(
                            &poll_host,
                            &BrowserSession {
                                cookie_header: header,
                                user_agent: SOLVE_USER_AGENT.to_string(),
                            },
                        );
                        if !captured {
                            captured = true;
                            debug_log(&format!("login: {poll_host} — Sitzung erfasst"));
                        }
                        set_login_state(&poll_host, "done");
                    }
                }
            }

            if window.is_none() {
                // Window closed by the user; keep whatever we captured.
                if !captured {
                    set_login_state(
                        &poll_host,
                        "failed:Fenster geschlossen — kein Login erkannt.",
                    );
                    debug_log(&format!(
                        "login: {poll_host} — Fenster ohne Login geschlossen"
                    ));
                }
                return;
            }
            if std::time::Instant::now() >= deadline {
                if !captured {
                    set_login_state(&poll_host, "failed:Zeitüberschreitung.");
                }
                if let Some(active) = handle.get_webview_window(LOGIN_WINDOW_LABEL) {
                    let _ = active.close();
                }
                return;
            }
        }
    });

    WebnovelLoginResponse {
        host: Some(host),
        error: None,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelLoginStatusResponse {
    logged_in: bool,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

pub(super) fn build_webnovel_login_status_response(query: Option<&str>) -> WebnovelLoginStatusResponse {
    let host = query
        .and_then(|q| extract_query_value(q, "host"))
        .map(|h| host_of(&h).unwrap_or(h).to_lowercase())
        .unwrap_or_default();
    let raw = LOGIN_STATES
        .lock()
        .ok()
        .and_then(|states| states.get(&host).cloned())
        .unwrap_or_else(|| "unknown".to_string());
    // A persisted session for the host means "logged in" across restarts.
    let logged_in = load_stored_sessions()
        .iter()
        .any(|entry| entry.host == host);
    match raw.strip_prefix("failed:") {
        Some(message) => WebnovelLoginStatusResponse {
            logged_in,
            state: "failed".to_string(),
            message: Some(message.to_string()),
        },
        None => WebnovelLoginStatusResponse {
            logged_in,
            state: raw,
            message: None,
        },
    }
}

pub(super) fn build_webnovel_logout_response(body: &[u8]) -> WebnovelSimpleResponse {
    let req: WebnovelLoginRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let host = host_of(&req.host).unwrap_or_else(|| req.host.trim().to_lowercase());
    drop_session(&host);
    set_login_state(&host, "unknown");
    debug_log(&format!("logout: {host} — Sitzung entfernt"));
    WebnovelSimpleResponse::ok()
}

// ---------------------------------------------------------------------------
// Embedded-browser fetch engine (whitelisted, manual-only)
// ---------------------------------------------------------------------------
//
// For hosts in `WEBVIEW_ROUTED_HOSTS` a plain HTTP request either can't pass
// Cloudflare (clearance is TLS-fingerprint-bound) or never sees the content
// (rendered client-side by JavaScript). We drive a visible, sandboxed browser
// window page-by-page and read the fully-rendered HTML back through the window
// TITLE — the only Rust-readable channel that is NOT sent to the server (unlike
// cookies, which overflow request headers). An injected script gzip+hex-encodes
// the rendered `outerHTML` into a JS variable; Rust pulls it in chunks by
// eval-ing "set title to chunk i" and reading the title. The window has no
// IPC / app access whatsoever.

/// Label of the persistent fetch/browser window.
const BROWSER_WINDOW_LABEL: &str = "mv-browser";
/// Hard cap for rendering a single page (excluding manual challenge time).
const RENDER_TIMEOUT_SECS: u64 = 45;
/// How long the user may take to solve an in-window challenge per page.
const CHALLENGE_WAIT_SECS: u64 = 180;
/// Characters of hex per title chunk pulled from the window.
const TITLE_CHUNK_LEN: usize = 4000;
/// Largest hex payload accepted from the browser window (≈8 MB of page HTML
/// before compression). The relay script runs inside the foreign page, so the
/// page can replace it and announce any size it likes.
const MAX_RELAY_HEX_LEN: usize = 16 * 1024 * 1024;
/// Cap for the decompressed HTML, so a crafted gzip stream cannot exhaust
/// memory.
const MAX_RENDERED_HTML_BYTES: u64 = 32 * 1024 * 1024;

/// Reflects the current browser-fetch state to the running job's message.
static CURRENT_JOB_ID: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

pub(super) fn set_current_job_id(job_id: Option<String>) {
    if let Ok(mut guard) = CURRENT_JOB_ID.lock() {
        *guard = job_id;
    }
}

fn note_browser_status(message: &str) {
    let job_id = CURRENT_JOB_ID.lock().ok().and_then(|g| g.clone());
    if let Some(job_id) = job_id {
        let owned = message.to_string();
        update_webnovel_job(&job_id, move |status| status.message = Some(owned));
    }
}

/// Injected once per navigation. Runs in the foreign page context, has no
/// access to the app. Exposes `window.__mvGet(kind, i)` for the Rust puller
/// and builds the encoded payload after the page settles.
const BROWSER_RELAY_SCRIPT: &str = r#"
(function(){
  if (window.__mvInstalled) { return; }
  window.__mvInstalled = true;
  window.__mvData = null;
  window.__mvMode = 'gz';
  window.__mvPath = '';
  function isChallenge(){
    var t=(document.title||'').toLowerCase();
    if(t.indexOf('just a moment')>=0) return true;
    if(t.indexOf('attention required')>=0) return true;
    if(document.querySelector('#challenge-running,#cf-challenge-running,.cf-browser-verification,#challenge-form,#turnstile-wrapper')) return true;
    return false;
  }
  window.__mvGet = function(kind, i){
    try {
      if (isChallenge()) { return 'CH'; }
      if (window.__mvData === null) { return 'WAIT'; }
      if (kind === 'meta') { return 'READY:' + window.__mvData.length + ':' + window.__mvMode + ':' + (window.__mvPath||''); }
      return window.__mvData.substr(i * 4000, 4000);
    } catch(e) { return 'ERR'; }
  };
  async function build(){
    try{
      if (isChallenge()) { done=false; return; }
      var html=document.documentElement.outerHTML;
      html=html.replace(/<script[\s\S]*?<\/script>/gi,'');
      html=html.replace(/<style[\s\S]*?<\/style>/gi,'');
      html=html.replace(/<svg[\s\S]*?<\/svg>/gi,'');
      html=html.replace(/<!--[\s\S]*?-->/g,'');
      var enc=new TextEncoder().encode(html);
      var mode='raw'; var bytes=enc;
      if(typeof CompressionStream!=='undefined'){
        try{
          var cs=new CompressionStream('gzip');
          var w=cs.writable.getWriter(); w.write(enc); w.close();
          var ab=await new Response(cs.readable).arrayBuffer();
          bytes=new Uint8Array(ab); mode='gz';
        }catch(e){ bytes=enc; mode='raw'; }
      }
      var d='0123456789abcdef'; var hex='';
      for(var i=0;i<bytes.length;i++){ hex+=d[(bytes[i]>>>4)&15]+d[bytes[i]&15]; }
      window.__mvData=hex; window.__mvMode=mode; window.__mvPath=location.pathname;
    }catch(e){ window.__mvData=''; window.__mvMode='err'; }
  }
  // Capture once the DOM has quiesced AND carries enough text: SPA chapter
  // bodies (Next.js) arrive via an async fetch that resolves after the initial
  // render settles, so waiting for mere DOM-stillness fires too early on a
  // near-empty skeleton. Gate on visible-text volume; a hard cap still
  // guarantees delivery (short chapters, pages that never fill).
  var done=false, settleTimer=null;
  function textLen(){
    try { return ((document.body && document.body.innerText) || '').replace(/\s+/g,'').length; }
    catch(e){ return 999999; }
  }
  function fire(){
    if(done) return;
    if(isChallenge()){ setTimeout(bump, 1000); return; }
    done=true; build();
  }
  function bump(){
    if(done) return;
    if(settleTimer){ clearTimeout(settleTimer); }
    settleTimer=setTimeout(function(){
      if(done) return;
      if(isChallenge()){ setTimeout(bump, 1000); return; }
      // Enough text → capture; otherwise keep polling for the lazy body.
      if(textLen() >= 1500){ fire(); } else { bump(); }
    }, 900);
  }
  function start(){
    try{
      var obs=new MutationObserver(bump);
      obs.observe(document.documentElement,{childList:true,subtree:true,characterData:true});
    }catch(e){}
    bump();
    // Hard cap: deliver whatever is present so extraction/diagnostics can run.
    setTimeout(function(){ if(!done){ done=true; build(); } }, 15000);
  }
  if(document.readyState==='complete'){ start(); }
  else { window.addEventListener('load', start); }
})();
"#;

/// Decodes a lowercase-hex string into bytes.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

/// Runs a closure on the main thread and returns its value (needed because
/// webview title/eval calls must run there).
fn on_main_thread<T, F>(handle: &tauri::AppHandle, f: F) -> Option<T>
where
    F: FnOnce(&tauri::AppHandle) -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let inner = handle.clone();
    if handle
        .run_on_main_thread(move || {
            let _ = tx.send(f(&inner));
        })
        .is_err()
    {
        return None;
    }
    rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
}

/// Navigates the browser window to `url` (creating it on first use).
fn navigate_browser_window(handle: &tauri::AppHandle, url: &tauri::Url) {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    if let Some(window) = handle.get_webview_window(BROWSER_WINDOW_LABEL) {
        let target = serde_json::to_string(url.as_str()).unwrap_or_else(|_| "\"\"".to_string());
        // Invalidate the current page's payload before navigating so a poll
        // that races the navigation reads WAIT, never the previous page.
        let _ = window.eval(format!(
            "try{{window.__mvData=null;window.__mvPath='';}}catch(e){{}}window.location.href = {target};"
        ));
    } else {
        let _ = WebviewWindowBuilder::new(
            handle,
            BROWSER_WINDOW_LABEL,
            WebviewUrl::External(url.clone()),
        )
        .title("Fero-Browser — lädt Novel-Seiten (bitte geöffnet lassen)")
        .inner_size(1024.0, 820.0)
        .user_agent(SOLVE_USER_AGENT)
        .initialization_script(BROWSER_RELAY_SCRIPT)
        .build();
    }
}

/// Asks the window for `__mvGet(kind, i)` and returns the value.
///
/// The value is written to BOTH the URL hash and the window title, and read
/// back from whichever channel carries our request nonce. The hash survives
/// SPA title management (React/Next.js reset `document.title`) and is never
/// sent to the server; the title is a fallback for engines where the hash
/// isn't reflected by `url()`. A per-request nonce guards against stale reads.
fn pull_from_title(handle: &tauri::AppHandle, kind: &str, index: usize) -> Option<String> {
    let nonce = format!("{}", unix_now_ms());
    let arg = if kind == "meta" {
        "'meta',0".to_string()
    } else {
        format!("'chunk',{index}")
    };
    let script = format!(
        "try{{var v='MV:{nonce}:'+window.__mvGet({arg});location.hash=v;document.title=v;}}catch(e){{location.hash='MV:{nonce}:ERR';}}"
    );
    let _ = on_main_thread(handle, move |h| {
        if let Some(window) = h.get_webview_window(BROWSER_WINDOW_LABEL) {
            let _ = window.eval(&script);
        }
    });
    let prefix = format!("MV:{nonce}:");
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(120));
        let read = on_main_thread(handle, |h| {
            let window = h.get_webview_window(BROWSER_WINDOW_LABEL)?;
            // Prefer the hash; fall back to the title.
            let from_hash = window
                .url()
                .ok()
                .and_then(|url| url.fragment().map(str::to_string));
            let from_title = window.title().ok();
            Some((from_hash, from_title))
        })
        .flatten();
        if let Some((from_hash, from_title)) = read {
            for candidate in [from_hash, from_title].into_iter().flatten() {
                if let Some(rest) = candidate.strip_prefix(&prefix) {
                    return Some(rest.to_string());
                }
            }
        }
    }
    None
}

/// Milliseconds since the UNIX epoch (title nonce).
fn unix_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Closes the browser window at the end of a manual run.
pub(crate) fn close_browser_window() {
    if let Some(handle) = APP_HANDLE.get() {
        let handle = handle.clone();
        let inner = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            if let Some(window) = inner.get_webview_window(BROWSER_WINDOW_LABEL) {
                let _ = window.close();
            }
        });
    }
}

/// Fetches a page's fully-rendered HTML through the browser window.
///
/// Blocks the calling (worker) thread until the injected relay delivers the
/// page via the window title, a challenge times out, or rendering times out.
pub(crate) fn render_page_via_window(url: &str) -> Result<String> {
    let handle = APP_HANDLE
        .get()
        .cloned()
        .ok_or_else(|| FeroError::ExternalApi("App-Fenster noch nicht bereit.".to_string()))?;
    let target = url
        .parse::<tauri::Url>()
        .map_err(|_| FeroError::ExternalApi(format!("URL ungültig: {url}")))?;

    debug_log(&format!("render: navigate → {url}"));
    let nav_url = target.clone();
    let _ = on_main_thread(&handle, move |h| navigate_browser_window(h, &nav_url));

    let start = std::time::Instant::now();
    let mut challenge_seen = false;
    let mut last_meta = String::new();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(800));

        // Give the window a grace period to appear (created async on main).
        if handle.get_webview_window(BROWSER_WINDOW_LABEL).is_none() {
            if start.elapsed() < std::time::Duration::from_secs(8) {
                continue;
            }
            debug_log("render: FAIL Fenster nicht vorhanden");
            return Err(FeroError::ExternalApi(
                "Browserfenster wurde geschlossen.".to_string(),
            ));
        }

        let meta = pull_from_title(&handle, "meta", 0).unwrap_or_else(|| "<none>".to_string());
        if meta != last_meta {
            debug_log(&format!(
                "render: meta='{meta}' (t={}s)",
                start.elapsed().as_secs()
            ));
            last_meta = meta.clone();
        }
        if meta == "CH" {
            challenge_seen = true;
            note_browser_status(
                "Bitte die Sicherheitsprüfung im Browserfenster bestätigen (Fenster offen lassen) …",
            );
            if start.elapsed() >= std::time::Duration::from_secs(CHALLENGE_WAIT_SECS) {
                debug_log("render: FAIL Challenge-Timeout");
                return Err(FeroError::ExternalApi(
                    "Zeitüberschreitung bei der Sicherheitsprüfung im Fenster.".to_string(),
                ));
            }
            continue;
        }
        if let Some(ready) = meta.strip_prefix("READY:") {
            let mut parts = ready.split(':');
            let total: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let mode = parts.next().unwrap_or("gz").to_string();
            let page_path = parts.next().unwrap_or("").to_string();
            if total == 0 {
                continue;
            }
            if total > MAX_RELAY_HEX_LEN {
                debug_log(&format!("render: FAIL Payload zu groß ({total} Zeichen)"));
                return Err(FeroError::ExternalApi(
                    "Die Seite hat eine unerwartet große Antwort geliefert.".to_string(),
                ));
            }
            // Reject a capture that belongs to the previously loaded page: the
            // relay reports its own `location.pathname`, which must match the
            // page we navigated to. Guards against reading stale content while
            // an SPA navigation is still in flight.
            if !page_path.is_empty() {
                let want = target.path().trim_end_matches('/');
                let got = page_path.trim_end_matches('/');
                if want != got {
                    debug_log(&format!(
                        "render: stale Seite path='{page_path}' erwartet='{}' → warte",
                        target.path()
                    ));
                    continue;
                }
            }
            // Content is ready — drop any lingering challenge hint so the
            // normal progress line shows again.
            note_browser_status("");
            // Pull the hex payload chunk by chunk over the title.
            let chunk_count = total.div_ceil(TITLE_CHUNK_LEN);
            debug_log(&format!(
                "render: READY total={total} mode={mode} chunks={chunk_count}"
            ));
            let mut hex = String::with_capacity(total);
            let mut ok = true;
            for i in 0..chunk_count {
                match pull_from_title(&handle, "chunk", i) {
                    Some(chunk) if chunk != "ERR" && chunk != "WAIT" => hex.push_str(&chunk),
                    other => {
                        debug_log(&format!(
                            "render: chunk {i}/{chunk_count} fehlgeschlagen (got={:?})",
                            other.as_deref().unwrap_or("<none>")
                        ));
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || hex.len() != total {
                debug_log(&format!(
                    "render: unvollständig ok={ok} hex.len={} erwartet={total} → retry",
                    hex.len()
                ));
                // Content changed mid-pull (SPA still rendering) — retry.
                if start.elapsed() >= std::time::Duration::from_secs(RENDER_TIMEOUT_SECS) {
                    debug_log("render: FAIL unvollständig nach Timeout");
                    return Err(FeroError::ExternalApi(
                        "Seite konnte nicht vollständig gelesen werden.".to_string(),
                    ));
                }
                continue;
            }
            let bytes = hex_decode(&hex).ok_or_else(|| {
                debug_log("render: FAIL hex-decode");
                FeroError::ExternalApi("Ungültige Daten aus dem Browserfenster.".to_string())
            })?;
            let html = if mode == "gz" {
                use std::io::Read;
                // `take` bounds the *decompressed* size — the compression
                // ratio is chosen by the remote page, not by us.
                let mut decoder =
                    flate2::read::GzDecoder::new(&bytes[..]).take(MAX_RENDERED_HTML_BYTES);
                let mut text = String::new();
                decoder.read_to_string(&mut text).map_err(|e| {
                    debug_log(&format!("render: FAIL gunzip: {e}"));
                    FeroError::ExternalApi(format!("Dekomprimierung fehlgeschlagen: {e}"))
                })?;
                text
            } else {
                String::from_utf8(bytes)
                    .map_err(|e| FeroError::ExternalApi(format!("Ungültiges UTF-8: {e}")))?
            };
            debug_log(&format!("render: OK html.len={}", html.len()));
            return Ok(html);
        }

        // meta == "WAIT"/"ERR"/empty → keep waiting up to the render budget.
        let budget = if challenge_seen {
            CHALLENGE_WAIT_SECS
        } else {
            RENDER_TIMEOUT_SECS
        };
        if start.elapsed() >= std::time::Duration::from_secs(budget) {
            debug_log(&format!("render: FAIL Timeout (letztes meta='{meta}')"));
            return Err(FeroError::ExternalApi(
                "Seite konnte im Browserfenster nicht gerendert werden (Timeout).".to_string(),
            ));
        }
    }
}
