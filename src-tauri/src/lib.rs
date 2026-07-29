#[cfg(target_os = "macos")]
mod macos_un;
mod nats;
mod notifications;
mod tokens;
#[cfg(target_os = "windows")]
mod windows_toast;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::{Color, NewWindowFeatures, NewWindowResponse, PageLoadEvent, PageLoadPayload},
    AppHandle, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tokens::NativeAuthTokens;

/// App background applied to every window so no white frame shows before the
/// page paints (matches the dark OpenFrame UI).
const WINDOW_BG: Color = Color(22, 22, 22, 255);

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

pub(crate) const MAIN_LABEL: &str = "main";
/// Dedicated window for the BFF OAuth flow — the desktop counterpart of the
/// mobile shell's WKWebView login sheet (openframe-mobile NativeAuthPlugin).
const LOGIN_LABEL: &str = "native-auth";

/// Baked-in shared auth host, set at compile time
/// (`OPENFRAME_SHARED_HOST_URL=https://… cargo build`). Overridable per install
/// by `shared_host` in config.json, which is how a dev build points at a stage
/// gateway without a rebuild.
const DEFAULT_SHARED_HOST: Option<&str> = option_env!("OPENFRAME_SHARED_HOST_URL");

static WINDOW_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// The shell knows exactly one configured URL: the **shared auth host**. There
/// is no tenant host to configure — the bundle's /auth pages discover the
/// tenant from the user's email (`/sas/tenant/discover` on the shared host) and
/// login learns the tenant origin from the OAuth callback. Same model as
/// openframe-mobile, minus mobile's optional build-time single-tenant pin.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AppConfig {
    /// Shared auth host, e.g. "https://auth.openframe.example" — tenant discovery,
    /// /oauth/login, /oauth/dev-exchange and /oauth/refresh all live there.
    /// Empty/absent falls back to [`DEFAULT_SHARED_HOST`]. Read through
    /// [`shared_host`], never directly.
    shared_host: Option<String>,
    /// Tenant origin learned from the OAuth callback, pushed by the frontend
    /// via NativeAuth.setTenantHost. Shell-side networking (token refresh,
    /// NATS) uses it. Written by the shell, not by hand.
    pub(crate) learned_host: Option<String>,
}

/// The shared auth host in effect: config.json override, else the compile-time
/// default. `None` means the build was never given one — every auth call will
/// fail, so it is logged loudly at startup.
pub(crate) fn shared_host(cfg: &AppConfig) -> Option<String> {
    [cfg.shared_host.as_deref(), DEFAULT_SHARED_HOST]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn config_path(app: &AppHandle) -> std::path::PathBuf {
    app.path()
        .app_config_dir()
        .expect("resolve app config dir")
        .join("config.json")
}

pub(crate) fn load_config(app: &AppHandle) -> AppConfig {
    std::fs::read_to_string(config_path(app))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_config(app: &AppHandle, cfg: &AppConfig) -> Result<(), String> {
    let data = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    write_atomic(&config_path(app), &data)
}

/// Write via a temp file + rename, so a concurrent reader never sees a
/// half-written file. `std::fs::write` truncates first, and both stores are
/// read from background tasks — a torn read of tokens.json deserializes as "no
/// session", which tears the notification subscription down.
///
/// Owner-only on unix, applied to the temp file so the target is never briefly
/// world-readable: tokens.json is plaintext here, unlike the mobile Keychain.
pub(crate) fn write_atomic(path: &std::path::Path, data: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Accept "acme.openframe.ai", "https://acme.openframe.ai", or "…/" and
/// normalize to a scheme-qualified origin with no trailing slash.
fn normalize_host(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Host is empty".into());
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    url::Url::parse(&with_scheme).map_err(|e| format!("Invalid URL: {e}"))?;
    Ok(with_scheme.trim_end_matches('/').to_string())
}

/// JS injected into every window that hosts the bundled frontend, before any
/// page script runs. It supplies what the SSR server provides on the web:
/// (1) `window.__ENV` — the runtime env next-runtime-env reads (the export
///     build omits `<PublicEnvScript />`). Only the shared auth host is
///     supplied; `NEXT_PUBLIC_TENANT_HOST_URL` is deliberately absent so
///     `runtimeEnv.tenantHostUrl()` falls through to the host login learned
///     (`getStoredTenantHost`), which is what lets one binary serve any tenant.
/// (2) a minimal Capacitor-compatible bridge — the frontend's native-shell.ts
///     detects the shell via `window.Capacitor.isNativePlatform()` and drives
///     login + token custody through `Plugins.NativeAuth`, which is backed here
///     by the native_auth_* Tauri commands.
fn env_init_script(app: &AppHandle) -> String {
    let env = serde_json::json!({
        "NEXT_PUBLIC_SHARED_HOST_URL": shared_host(&load_config(app)).unwrap_or_default(),
        "NEXT_PUBLIC_APP_MODE": "saas-tenant",
        "NEXT_PUBLIC_ENABLE_DEV_TICKET_OBSERVER": "true",
    });
    format!(
        r#"window.__ENV = {env};
window.Capacitor = {{
  isNativePlatform: function () {{ return true; }},
  Plugins: {{
    NativeAuth: {{
      start: function (o) {{
        return window.__TAURI_INTERNALS__.invoke('native_auth_start', {{
          url: o.url, callbackHost: o.callbackHost, callbackPath: o.callbackPath
        }});
      }},
      exchangeTicket: function (o) {{
        return window.__TAURI_INTERNALS__.invoke('native_auth_exchange_ticket', {{ url: o.url }});
      }},
      getTokens: function () {{
        return window.__TAURI_INTERNALS__.invoke('native_auth_get_tokens');
      }},
      setTokens: function (o) {{
        return window.__TAURI_INTERNALS__.invoke('native_auth_set_tokens', {{
          accessToken: o.accessToken || null, refreshToken: o.refreshToken || null
        }});
      }},
      clearTokens: function () {{
        return window.__TAURI_INTERNALS__.invoke('native_auth_clear_tokens');
      }},
      refreshTokens: function () {{
        return window.__TAURI_INTERNALS__.invoke('native_auth_refresh_tokens');
      }},
      setTenantHost: function (o) {{
        return window.__TAURI_INTERNALS__.invoke('native_auth_set_tenant_host', {{ origin: o.origin }});
      }},
      getSafeAreaInsets: function () {{
        return Promise.resolve({{ top: 0, bottom: 0, left: 0, right: 0 }});
      }}
    }}
  }}
}};
(function () {{
  var forward = function (level, message) {{
    try {{
      window.__TAURI_INTERNALS__.invoke('webview_log', {{ level: level, message: String(message).slice(0, 2000) }});
    }} catch (e) {{}}
  }};
  window.addEventListener('error', function (e) {{
    forward('error', e.message + ' @ ' + e.filename + ':' + e.lineno);
  }});
  window.addEventListener('unhandledrejection', function (e) {{
    var r = e.reason;
    forward('error', 'unhandledrejection: ' + ((r && (r.stack || r.message)) || r));
  }});
  var origError = console.error.bind(console);
  console.error = function () {{
    var parts = Array.prototype.slice.call(arguments).map(function (a) {{
      if (typeof a === 'string') return a;
      try {{ return JSON.stringify(a); }} catch (_) {{ return String(a); }}
    }});
    forward('error', parts.join(' '));
    origError.apply(null, arguments);
  }};
  forward('info', 'bundle booting; origin=' + location.origin + ' __ENV=' + JSON.stringify(window.__ENV));
}})();"#
    )
}

/// Webview console/error forwarding into the shell log. Release builds ship
/// without devtools, so without this the bundle's runtime errors are invisible
/// (the mobile shell gets Safari Web Inspector instead). Wired by env_init_script.
#[tauri::command]
fn webview_log(window: WebviewWindow, level: String, message: String) {
    let label = window.label().to_string();
    match level.as_str() {
        "error" => log::error!("[webview:{label}] {message}"),
        "warn" => log::warn!("[webview:{label}] {message}"),
        _ => log::info!("[webview:{label}] {message}"),
    }
}

fn open_main_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(MAIN_LABEL).is_some() {
        show_primary_window(app);
        return Ok(());
    }
    // Close-to-tray drops the app to Accessory; a window created after that
    // (sign-out recreates one) would otherwise come back with no dock icon.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(ActivationPolicy::Regular);

    let handler_app = app.clone();
    let window = WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::App("index.html".into()))
        .initialization_script(env_init_script(app))
        .title("OpenFrame")
        .inner_size(1280.0, 860.0)
        .min_inner_size(1024.0, 700.0)
        .resizable(true)
        .background_color(WINDOW_BG)
        .visible(false)
        .on_new_window(move |url, features| handle_new_window(&handler_app, url, features))
        .on_page_load(handle_page_load)
        .build()
        .map_err(|e| e.to_string())?;
    spawn_show_fallback(&window);
    Ok(())
}

/// Recreate the main window after destroy(). The destroy is processed by the
/// event loop after the calling command returns, so the label can still be
/// taken here — and sync commands run on the main thread, so waiting inline
/// would deadlock the event loop. Poll from a spawned task instead.
fn reopen_main_window(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..50 {
            if app.get_webview_window(MAIN_LABEL).is_none() {
                if let Err(e) = open_main_window(&app) {
                    log::error!("reopen main window: {e}");
                }
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        log::error!("reopen main window: destroyed window never released the label");
    });
}

/// Reveal the window once its page has painted, so the user never sees the
/// webview's default white background flash before the dark UI renders. Also
/// closes the notification click gate again for each new document: the
/// webview's `notification:click` listener does not survive a page load.
fn handle_page_load(window: WebviewWindow, payload: PageLoadPayload<'_>) {
    match payload.event() {
        PageLoadEvent::Started if window.label() == MAIN_LABEL => {
            notifications::reset_click_gate(window.app_handle());
        }
        PageLoadEvent::Finished => {
            let _ = window.show();
            let _ = window.set_focus();
        }
        _ => {}
    }
}

/// Safety net: reveal the window even if the page load stalls or never fires the
/// finished event, so it can't stay invisible forever. show() is idempotent.
fn spawn_show_fallback(window: &WebviewWindow) {
    let window = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let _ = window.show();
    });
}

/// Routes `window.open` / `target="_blank"` requests, which a webview otherwise drops:
/// bundle pages open in a new in-app window, external http(s) links open in the
/// system browser, other schemes are blocked.
///
/// "In-app" means the target origin matches a live window that already hosts
/// bundle content (main/child) — i.e. the app origin Tauri serves the bundle
/// from (`tauri://localhost`; `http://tauri.localhost` on Windows). Matching
/// against a live window instead of hardcoding the origin keeps this correct
/// across platforms and dev/release. Tenant-origin links now open in the
/// browser, since app content no longer lives on the tenant origin.
fn handle_new_window(
    app: &AppHandle,
    url: url::Url,
    features: NewWindowFeatures,
) -> NewWindowResponse<tauri::Wry> {
    if !matches!(url.scheme(), "http" | "https" | "tauri") {
        log::warn!("blocked new window for scheme '{}': {url}", url.scheme());
        return NewWindowResponse::Deny;
    }

    // NOT Url::origin(): tauri:// is a non-special scheme, so origin() is
    // opaque — and opaque origins are unique per parse, never equal. Compare
    // the (scheme, host, port) triple instead, which also covers Windows'
    // http://tauri.localhost.
    let same_origin = |a: &url::Url, b: &url::Url| {
        a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port() == b.port()
    };
    let in_app = app.webview_windows().values().any(|w| {
        (w.label() == MAIN_LABEL || w.label().starts_with("child-"))
            && w.url().map(|u| same_origin(&u, &url)).unwrap_or(false)
    });

    if in_app {
        let n = WINDOW_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Cascade so each window is offset instead of stacking exactly on top of
        // the previous one (which made multiple windows look like one). Wraps after 6.
        let offset = (n % 6) as f64 * 36.0;
        // Attach the same handler to the child so links opened from it also work.
        let child_app = app.clone();
        match WebviewWindowBuilder::new(
            app,
            format!("child-{n}"),
            WebviewUrl::External(url.clone()),
        )
        .initialization_script(env_init_script(app))
        .title(url.as_str())
        .inner_size(1100.0, 800.0)
        .position(140.0 + offset, 120.0 + offset)
        .background_color(WINDOW_BG)
        .visible(false)
        .on_new_window(move |u, f| handle_new_window(&child_app, u, f))
        .on_page_load(handle_page_load)
        .window_features(features)
        .build()
        {
            Ok(window) => {
                spawn_show_fallback(&window);
                return NewWindowResponse::Create { window };
            }
            Err(e) => {
                log::error!("failed to create child window: {e}");
                return NewWindowResponse::Deny;
            }
        }
    }

    if !matches!(url.scheme(), "http" | "https") {
        log::warn!("blocked new window for non-app url: {url}");
        return NewWindowResponse::Deny;
    }
    if let Err(e) = open_external(url.as_str()) {
        log::error!("failed to open external url {url}: {e}");
    }
    NewWindowResponse::Deny
}

/// Open a URL in the user's default browser without going through a shell
/// (each arg is passed literally, so URL contents can't be interpreted as flags/commands).
fn open_external(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = std::process::Command::new("xdg-open");

    cmd.arg(url).spawn().map(|_| ())
}

pub(crate) fn show_primary_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(MAIN_LABEL) {
        #[cfg(target_os = "macos")]
        let _ = app.set_activation_policy(ActivationPolicy::Regular);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Tray "Sign Out": drop the session and the learned tenant, then recreate the
/// main window so the bundle boots to its sign-in screen, which rediscovers the
/// tenant from the user's email against the shared host. destroy(), not close():
/// close() emits CloseRequested, which the tray handler turns into a hide.
fn sign_out(app: &AppHandle) -> Result<(), String> {
    let mut cfg = load_config(app);
    cfg.learned_host = None;
    save_config(app, &cfg)?;
    native_auth_clear_tokens(app.clone())?;
    match app.get_webview_window(MAIN_LABEL) {
        Some(main) => {
            let _ = main.destroy();
            reopen_main_window(app);
        }
        None => open_main_window(app)?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// NativeAuth bridge — backs the window.Capacitor.Plugins.NativeAuth shim (see
// env_init_script). Mirrors openframe-mobile's NativeAuthPlugin.swift: login
// window -> ?devTicket= capture -> native token exchange -> local token store.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AuthState {
    pending_login: Mutex<Option<tokio::sync::oneshot::Sender<Result<String, String>>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeAuthStartResult {
    callback_url: String,
}

fn url_carries_ticket(url: &url::Url) -> bool {
    url.query_pairs().any(|(k, _)| k == "devTicket")
}

/// Resolve the pending native_auth_start() and tear down the login window.
/// Only the first resolution wins; later calls (e.g. the Destroyed event after
/// we destroy the window ourselves) are no-ops.
fn finish_login(app: &AppHandle, result: Result<String, String>) {
    let state: State<'_, AuthState> = app.state();
    let Some(tx) = state.pending_login.lock().unwrap().take() else {
        return;
    };
    match &result {
        // Origin only — the devTicket rides in the query string.
        Ok(callback) => log::info!(
            "[auth] devTicket callback captured (host: {})",
            url::Url::parse(callback)
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_else(|_| "<unparsed>".into())
        ),
        Err(e) => log::info!("[auth] login ended without a ticket: {e}"),
    }
    let _ = tx.send(result);
    // Destroy off the event-callback stack; destroy() so the close can't be
    // intercepted or re-enter the webview callbacks.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(win) = app.get_webview_window(LOGIN_LABEL) {
            let _ = win.destroy();
        }
    });
}

/// NativeAuth.start: run the gateway BFF OAuth flow in a dedicated window and
/// resolve with the redirect URL carrying `?devTicket=`. Like the mobile
/// plugin, ANY url carrying the ticket is accepted (the deployed BFF drops the
/// redirectTo path and delivers the ticket on the tenant root), and the check
/// runs in both on_navigation and on_page_load because server 302 hops don't
/// always surface as navigation-policy callbacks. The window is not listed in
/// any capability, so the remote login page cannot reach Tauri commands.
#[tauri::command]
async fn native_auth_start(
    app: AppHandle,
    state: State<'_, AuthState>,
    url: String,
    callback_host: String,
    callback_path: String,
) -> Result<NativeAuthStartResult, String> {
    // Part of the NativeAuth plugin API; the desktop capture keys on ?devTicket= alone.
    let _ = (callback_host, callback_path);
    let login_url = url::Url::parse(&url).map_err(|e| format!("Invalid login URL: {e}"))?;
    log::info!(
        "[auth] opening login window at {}",
        login_url.origin().ascii_serialization()
    );

    if let Some(stale) = app.get_webview_window(LOGIN_LABEL) {
        let _ = stale.destroy();
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Some(stale) = state.pending_login.lock().unwrap().replace(tx) {
        let _ = stale.send(Err("USER_CANCELED".into()));
    }

    let nav_app = app.clone();
    let load_app = app.clone();
    WebviewWindowBuilder::new(&app, LOGIN_LABEL, WebviewUrl::External(login_url))
        .title("Sign in to OpenFrame")
        .inner_size(520.0, 720.0)
        .background_color(WINDOW_BG)
        .on_navigation(move |u| {
            if url_carries_ticket(u) {
                finish_login(&nav_app, Ok(u.to_string()));
                return false;
            }
            true
        })
        .on_page_load(move |_win, payload| {
            if url_carries_ticket(payload.url()) {
                finish_login(&load_app, Ok(payload.url().to_string()));
            }
        })
        .build()
        .map_err(|e| e.to_string())?;

    match rx.await {
        Ok(result) => result.map(|callback_url| NativeAuthStartResult { callback_url }),
        Err(_) => Err("USER_CANCELED".into()),
    }
}

/// NativeAuth.exchangeTicket: native (CORS-free) GET; the tokens ride on the
/// Access-Token / Refresh-Token response headers, which a cross-origin webview
/// fetch could not read.
#[tauri::command]
async fn native_auth_exchange_ticket(url: String) -> Result<NativeAuthTokens, String> {
    // Log the origin only — the ticket rides in the query string.
    let origin = url::Url::parse(&url)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| "<invalid url>".into());
    let response = reqwest::get(&url)
        .await
        .inspect_err(|e| log::warn!("[auth] ticket exchange at {origin} failed: {e}"))
        .map_err(|e| format!("Ticket exchange failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        log::warn!("[auth] ticket exchange at {origin} rejected: HTTP {status} (404 can mean dev-ticket is disabled on that gateway)");
        return Err(format!("Ticket exchange failed: HTTP {status}"));
    }
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let tokens = NativeAuthTokens {
        access_token: header("Access-Token"),
        refresh_token: header("Refresh-Token"),
    };
    log::info!(
        "[auth] ticket exchange at {origin}: HTTP {status} (access: {}, refresh: {})",
        tokens.access_token.is_some(),
        tokens.refresh_token.is_some()
    );
    Ok(tokens)
}

/// Fresh-on-read: refreshes first when the stored access token is missing or
/// expiring, so the webview resumes instantly after long idle.
#[tauri::command]
async fn native_auth_get_tokens(app: AppHandle) -> Result<NativeAuthTokens, String> {
    Ok(tokens::ensure_fresh(&app).await)
}

/// Shell-owned refresh, delegated to by the webview's token-refresh-manager on
/// upstream 401s. Force-refreshes (exp can't predict revocation), dampened by
/// the previous access token so parallel callers don't rotate twice.
#[tauri::command]
async fn native_auth_refresh_tokens(app: AppHandle) -> Result<NativeAuthTokens, String> {
    log::info!("[tokens] webview requested a refresh (upstream 401)");
    let prev_access = tokens::load_tokens(&app).access_token;
    tokens::refresh(&app, true, prev_access).await
}

/// Merge semantics: only fields that arrive are overwritten — token-rotation
/// responses may carry one token or both (matches the mobile Keychain plugin).
#[tauri::command]
fn native_auth_set_tokens(
    app: AppHandle,
    access_token: Option<String>,
    refresh_token: Option<String>,
) -> Result<(), String> {
    log::info!(
        "[tokens] webview stored tokens (access: {}, refresh: {})",
        access_token.is_some(),
        refresh_token.is_some()
    );
    let mut stored = tokens::load_tokens(&app);
    if access_token.is_some() {
        stored.access_token = access_token;
    }
    if refresh_token.is_some() {
        stored.refresh_token = refresh_token;
    }
    tokens::save_tokens(&app, &stored)?;
    // A sign-in on a connection that never dropped (the previous user signed
    // out under it) gets no Connected event — subscribe for the new user here.
    nats::resubscribe(&app);
    Ok(())
}

#[tauri::command]
fn native_auth_clear_tokens(app: AppHandle) -> Result<(), String> {
    // Who cleared the session matters in post-mortems: this path is the
    // webview (force-logout / sign-out) or the tray's Sign Out — the shell's
    // own refresh logs its 401-clear separately in tokens.rs.
    log::info!("[tokens] tokens cleared (webview logout or tray sign-out)");
    // Every logout funnels through here, so this is the one place that reliably
    // tears down the signed-out user's notification subscription.
    notifications::end_session(&app);
    tokens::clear_tokens(&app)
}

/// Persist the tenant host the frontend learned from the OAuth callback, so
/// shell-side networking (token refresh, the NATS notification plane) has a
/// gateway without depending on webview localStorage.
#[tauri::command]
fn native_auth_set_tenant_host(app: AppHandle, origin: String) -> Result<(), String> {
    let normalized = normalize_host(&origin)?;
    let mut cfg = load_config(&app);
    if cfg.learned_host.as_deref() != Some(normalized.as_str()) {
        log::info!("learned tenant host: {normalized}");
        cfg.learned_host = Some(normalized);
        save_config(&app, &cfg)?;
        // Login writes tokens before it pushes the host, so this — not
        // native_auth_set_tokens — is where a tenant switch becomes visible.
        // The live connection belongs to the tenant being left.
        nats::reconnect(&app);
    }
    Ok(())
}

/// Called once per webview document, when its `notification:click` listener
/// mounts. Opens the click gate and returns a click that happened before the
/// listener existed (a notification click that cold-started the app), if any.
///
/// Child windows run the same bundle and so call this too; only the main window
/// is ever emitted to, so anyone else must not open the gate or drain the stash.
#[tauri::command]
fn take_pending_notification_click(window: WebviewWindow) -> Option<serde_json::Value> {
    if window.label() != MAIN_LABEL {
        return None;
    }
    notifications::take_startup_click(window.app_handle())
}

/// Deliver a notification-activation URI carried in a process's arguments —
/// the Windows toast click path, either our own argv (cold start) or a second
/// launch forwarded by the single-instance plugin (warm click). Returns true
/// when one was found and delivered; the caller then skips its own
/// window-raising, since delivery raises the window itself.
#[cfg(target_os = "windows")]
fn handle_notification_argv(app: &AppHandle, args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter()
        .any(|arg| notifications::handle_notification_uri(app, &arg))
}

/// Register the `openframe-desktop://` URI scheme (HKCU, no elevation).
/// Windows toast clicks activate through it — see windows_toast.rs. Re-written
/// on every launch so the command always points at the current exe path.
#[cfg(target_os = "windows")]
fn register_url_scheme() {
    use winreg::{enums::*, RegKey};

    let Ok(exe) = std::env::current_exe() else {
        log::warn!("url scheme: current_exe unavailable — skipping registration");
        return;
    };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = format!(r"Software\Classes\{}", notifications::URI_SCHEME);
    let Ok((key, _)) = hkcu.create_subkey(&path) else {
        log::warn!("url scheme: failed to create registry key {path}");
        return;
    };
    let _ = key.set_value("", &"URL:OpenFrame Desktop");
    let _ = key.set_value("URL Protocol", &"");
    if let Ok((command, _)) = key.create_subkey(r"shell\open\command") {
        let _ = command.set_value("", &format!("\"{}\" \"%1\"", exe.display()));
    }
}

fn tray_icon() -> Image<'static> {
    #[cfg(target_os = "macos")]
    {
        Image::from_bytes(include_bytes!("../icons/tray-macos44x44.png")).expect("tray icon")
    }
    #[cfg(not(target_os = "macos"))]
    {
        Image::from_bytes(include_bytes!("../icons/tray-windows32x32.png")).expect("tray icon")
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
    let signout_i = MenuItem::with_id(app, "signout", "Sign Out", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &signout_i, &quit_i])?;

    TrayIconBuilder::new()
        .icon(tray_icon())
        .icon_as_template(cfg!(target_os = "macos"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("OpenFrame")
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_primary_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_primary_window(app),
            "signout" => {
                if let Err(e) = sign_out(app) {
                    log::error!("sign_out failed: {e}");
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // A Windows toast click activates the openframe-desktop:// URI,
            // which reaches the running instance as a second launch carrying it
            // in argv. Anything else is the user re-launching the app.
            #[cfg(target_os = "windows")]
            let handled = handle_notification_argv(app, argv);
            #[cfg(not(target_os = "windows"))]
            let handled = {
                let _ = argv;
                false
            };
            if !handled {
                show_primary_window(app);
            }
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("openframe-desktop".into()),
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                ])
                .level(log::LevelFilter::Info)
                // The 40KB default rotated away the evidence the first time a
                // token incident needed post-mortem — keep a real window.
                .max_file_size(5_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne)
                .build(),
        )
        .manage(AuthState::default())
        .manage(tokens::TokenLifecycle::default())
        .invoke_handler(tauri::generate_handler![
            native_auth_start,
            native_auth_exchange_ticket,
            native_auth_get_tokens,
            native_auth_refresh_tokens,
            native_auth_set_tokens,
            native_auth_clear_tokens,
            native_auth_set_tenant_host,
            take_pending_notification_click,
            webview_log
        ])
        .setup(|app| {
            build_tray(app)?;
            #[cfg(target_os = "windows")]
            register_url_scheme();
            tokens::spawn_refresh_loop(app.handle().clone());
            notifications::init(app.handle());
            nats::spawn(app.handle().clone());

            let handle = app.handle().clone();
            match shared_host(&load_config(&handle)) {
                Some(host) => log::info!("shared auth host: {host}"),
                // Nothing to sign in against: discovery, /oauth/* and refresh
                // all live on the shared host. Still open the window so the
                // failure is visible in the UI rather than an empty screen.
                None => log::error!(
                    "no shared auth host — build with OPENFRAME_SHARED_HOST_URL or set \
                     shared_host in config.json; every auth call will fail"
                ),
            }
            // Cold start from a Windows toast click: the protocol launch put
            // the URI in our own argv. Runs before the window exists so the
            // payload is stashed (the webview pulls it via
            // take_pending_notification_click) instead of being shown early —
            // handle_page_load still owns the reveal, so no unpainted flash.
            // `args_os`, not `args`: the latter panics on non-UTF-8 arguments.
            #[cfg(target_os = "windows")]
            handle_notification_argv(
                &handle,
                std::env::args_os().filter_map(|arg| arg.into_string().ok()),
            );

            open_main_window(&handle).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Close the main window to tray; let child (new-tab) windows close normally.
            WindowEvent::CloseRequested { api, .. } => {
                if window.label() == MAIN_LABEL {
                    api.prevent_close();
                    let _ = window.hide();
                    #[cfg(target_os = "macos")]
                    let _ = window
                        .app_handle()
                        .set_activation_policy(ActivationPolicy::Accessory);
                }
            }
            // Login window closed by the user before the ticket arrived → cancel.
            WindowEvent::Destroyed if window.label() == LOGIN_LABEL => {
                finish_login(window.app_handle(), Err("USER_CANCELED".into()));
            }
            // Sign-out destroys and recreates the main window; park clicks from
            // here until the replacement's listener mounts, rather than emitting
            // them at a label that currently has no window.
            WindowEvent::Destroyed if window.label() == MAIN_LABEL => {
                notifications::reset_click_gate(window.app_handle());
            }
            // The user is looking at the app — clear the unread badge.
            WindowEvent::Focused(true) if window.label() == MAIN_LABEL => {
                notifications::on_main_focused(window.app_handle());
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
