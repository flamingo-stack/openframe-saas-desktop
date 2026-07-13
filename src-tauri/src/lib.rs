mod notifications;
mod tokens;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use serde::{Deserialize, Serialize};
use tokens::NativeAuthTokens;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::{Color, NewWindowFeatures, NewWindowResponse, PageLoadEvent, PageLoadPayload},
    AppHandle, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

/// App background applied to every window so no white frame shows before the
/// page paints (matches the dark OpenFrame UI).
const WINDOW_BG: Color = Color(22, 22, 22, 255);

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

pub(crate) const MAIN_LABEL: &str = "main";
const CONNECT_LABEL: &str = "connect";
/// Dedicated window for the BFF OAuth flow — the desktop counterpart of the
/// mobile shell's WKWebView login sheet (openframe-mobile NativeAuthPlugin).
const LOGIN_LABEL: &str = "native-auth";

static WINDOW_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AppConfig {
    /// Fully-qualified tenant origin, e.g. "https://acme.openframe.ai" (no trailing slash).
    pub(crate) host: Option<String>,
    /// Shared auth host for SaaS builds (injected as NEXT_PUBLIC_SHARED_HOST_URL).
    pub(crate) shared_host: Option<String>,
    /// Frontend app mode (injected as NEXT_PUBLIC_APP_MODE); defaults to "oss-tenant".
    app_mode: Option<String>,
    /// Tenant host learned from the OAuth callback in saas-tenant (dynamic)
    /// mode, pushed by the frontend via NativeAuth.setTenantHost. Shell-side
    /// networking (token refresh, future NATS) uses it when no pinned host
    /// exists.
    pub(crate) learned_host: Option<String>,
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
    let path = config_path(app);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, data).map_err(|e| e.to_string())
}

/// saas-tenant mode boots without a pinned tenant host: the bundle's /auth
/// pages discover the tenant (email → /sas/tenant/discover on the shared
/// host), and native-login persists the host learned from the OAuth callback.
/// Requires `shared_host` in config.json to be useful.
fn is_saas_tenant(cfg: &AppConfig) -> bool {
    cfg.app_mode.as_deref() == Some("saas-tenant")
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
///     build omits `<PublicEnvScript />`); the tenant host comes from the
///     connect-window pick persisted in config.json, so one binary serves any
///     instance, and
/// (2) a minimal Capacitor-compatible bridge — the frontend's native-shell.ts
///     detects the shell via `window.Capacitor.isNativePlatform()` and drives
///     login + token custody through `Plugins.NativeAuth`, which is backed here
///     by the native_auth_* Tauri commands.
fn env_init_script(app: &AppHandle) -> String {
    let cfg = load_config(app);
    let env = serde_json::json!({
        "NEXT_PUBLIC_TENANT_HOST_URL": cfg.host.clone().unwrap_or_default(),
        "NEXT_PUBLIC_SHARED_HOST_URL": cfg.shared_host.clone().unwrap_or_default(),
        "NEXT_PUBLIC_APP_MODE": cfg.app_mode.clone().unwrap_or_else(|| "oss-tenant".into()),
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
    if let Some(win) = app.get_webview_window(MAIN_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
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
        .on_page_load(show_when_loaded)
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
/// webview's default white background flash before the dark UI renders.
fn show_when_loaded(window: WebviewWindow, payload: PageLoadPayload<'_>) {
    if payload.event() == PageLoadEvent::Finished {
        let _ = window.show();
        let _ = window.set_focus();
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

    let target_origin = url.origin();
    let in_app = app.webview_windows().values().any(|w| {
        (w.label() == MAIN_LABEL || w.label().starts_with("child-"))
            && w.url().map(|u| u.origin() == target_origin).unwrap_or(false)
    });

    if in_app {
        let n = WINDOW_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Cascade so each window is offset instead of stacking exactly on top of
        // the previous one (which made multiple windows look like one). Wraps after 6.
        let offset = (n % 6) as f64 * 36.0;
        // Attach the same handler to the child so links opened from it also work.
        let child_app = app.clone();
        match WebviewWindowBuilder::new(app, format!("child-{n}"), WebviewUrl::External(url.clone()))
            .initialization_script(env_init_script(app))
            .title(url.as_str())
            .inner_size(1100.0, 800.0)
            .position(140.0 + offset, 120.0 + offset)
            .background_color(WINDOW_BG)
            .visible(false)
            .on_new_window(move |u, f| handle_new_window(&child_app, u, f))
            .on_page_load(show_when_loaded)
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

fn open_connect_window(app: &AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(CONNECT_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, CONNECT_LABEL, WebviewUrl::App("connect.html".into()))
        .title("Connect to OpenFrame")
        .inner_size(520.0, 600.0)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn show_primary_window(app: &AppHandle) {
    let label = if app.get_webview_window(MAIN_LABEL).is_some() {
        MAIN_LABEL
    } else {
        CONNECT_LABEL
    };
    if let Some(win) = app.get_webview_window(label) {
        #[cfg(target_os = "macos")]
        let _ = app.set_activation_policy(ActivationPolicy::Regular);
        let _ = win.show();
        let _ = win.set_focus();
    }
}

#[tauri::command]
fn get_tenant_host(app: AppHandle) -> Option<String> {
    load_config(&app).host
}

#[tauri::command]
fn set_tenant_host(app: AppHandle, host: String) -> Result<(), String> {
    log::info!("set_tenant_host: received {host:?}");
    let normalized = normalize_host(&host).inspect_err(|e| log::error!("normalize: {e}"))?;
    let mut cfg = load_config(&app);
    cfg.host = Some(normalized.clone());
    save_config(&app, &cfg).inspect_err(|e| log::error!("save_config: {e}"))?;
    // The env init script is baked at window creation, so a live main window
    // still carries the previous tenant — replace it. destroy(), not close():
    // close() emits CloseRequested, which the tray handler turns into a hide.
    if let Some(main) = app.get_webview_window(MAIN_LABEL) {
        let _ = main.destroy();
        reopen_main_window(&app);
    } else {
        open_main_window(&app).inspect_err(|e| log::error!("open_main_window: {e}"))?;
    }
    if let Some(connect) = app.get_webview_window(CONNECT_LABEL) {
        let _ = connect.destroy();
    }
    log::info!("set_tenant_host: opened {normalized}");
    Ok(())
}

#[tauri::command]
fn switch_instance(app: AppHandle) -> Result<(), String> {
    let mut cfg = load_config(&app);
    cfg.host = None;
    cfg.learned_host = None;
    save_config(&app, &cfg)?;
    // The stored tokens belong to the tenant being left behind.
    native_auth_clear_tokens(app.clone())?;
    let had_main = match app.get_webview_window(MAIN_LABEL) {
        Some(main) => {
            let _ = main.destroy();
            true
        }
        None => false,
    };
    if is_saas_tenant(&cfg) {
        // No host picker in saas-tenant mode — a fresh main window boots to
        // the sign-in screen (tokens are gone), which rediscovers the tenant.
        if had_main {
            reopen_main_window(&app);
        } else {
            open_main_window(&app)?;
        }
    } else {
        open_connect_window(&app)?;
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
    tokens::save_tokens(&app, &stored)
}

#[tauri::command]
fn native_auth_clear_tokens(app: AppHandle) -> Result<(), String> {
    // Who cleared the session matters in post-mortems: this path is the
    // webview (force-logout / sign-out) or switch_instance — the shell's own
    // refresh logs its 401-clear separately in tokens.rs.
    log::info!("[tokens] tokens cleared (webview logout or instance switch)");
    tokens::clear_tokens(&app)
}

/// Persist the tenant host the frontend learned from the OAuth callback
/// (saas-tenant dynamic mode), so shell-side networking (token refresh, future
/// NATS) has a gateway without depending on webview localStorage.
#[tauri::command]
fn native_auth_set_tenant_host(app: AppHandle, origin: String) -> Result<(), String> {
    let normalized = normalize_host(&origin)?;
    let mut cfg = load_config(&app);
    if cfg.learned_host.as_deref() != Some(normalized.as_str()) {
        log::info!("learned tenant host: {normalized}");
        cfg.learned_host = Some(normalized);
        save_config(&app, &cfg)?;
    }
    Ok(())
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
    let switch_i = MenuItem::with_id(app, "switch", "Switch Instance…", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &switch_i, &quit_i])?;

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
            "switch" => {
                if let Err(e) = switch_instance(app.clone()) {
                    log::error!("switch_instance failed: {e}");
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
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_primary_window(app);
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
            get_tenant_host,
            set_tenant_host,
            switch_instance,
            native_auth_start,
            native_auth_exchange_ticket,
            native_auth_get_tokens,
            native_auth_refresh_tokens,
            native_auth_set_tokens,
            native_auth_clear_tokens,
            native_auth_set_tenant_host,
            webview_log
        ])
        .setup(|app| {
            build_tray(app)?;
            tokens::spawn_refresh_loop(app.handle().clone());
            notifications::init(app.handle());

            let handle = app.handle().clone();
            let cfg = load_config(&handle);
            if cfg.host.is_some() || is_saas_tenant(&cfg) {
                open_main_window(&handle)
            } else {
                open_connect_window(&handle)
            }
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Close the primary window to tray; let child (new-tab) windows close normally.
            WindowEvent::CloseRequested { api, .. } => {
                let label = window.label();
                if label == MAIN_LABEL || label == CONNECT_LABEL {
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
            // Badge clear + pending toast-click handoff.
            WindowEvent::Focused(true) if window.label() == MAIN_LABEL => {
                notifications::on_main_focused(window.app_handle());
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
