mod autostart;
// The background notification actions and the REST calls behind them exist on
// both desktop platforms; Linux has no notification backend at all, so neither
// module is built there.
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod chat_api;
#[cfg(target_os = "macos")]
mod macos_un;
#[cfg(target_os = "macos")]
mod macos_wake;
mod nats;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod notification_actions;
mod notifications;
mod tokens;
mod updater;
#[cfg(target_os = "windows")]
mod windows_activator;
// Also compiled for a macOS test run: the toast XML and the button-argument
// codec fail inside the notification platform rather than in a stack trace, so
// they are worth testing on the host the rest of CI already uses. Not on Linux,
// where `notification_actions` — which this depends on — does not exist.
#[cfg(any(target_os = "windows", all(target_os = "macos", test)))]
mod windows_toast;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::{Color, NewWindowFeatures, NewWindowResponse, PageLoadEvent, PageLoadPayload},
    AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent,
};
use tokens::NativeAuthTokens;

/// App background applied to every window so no white frame shows before the
/// page paints (matches the dark OpenFrame UI).
const WINDOW_BG: Color = Color(22, 22, 22, 255);

/// Title every window opens with, before its page has one.
/// [`mirror_document_title`] replaces it as soon as the page reports one.
/// A window must not fall back to its URL here: a child window's is a
/// `tauri://localhost/...` bundle URL, which is an internal detail and reads as
/// a broken window.
const WINDOW_TITLE: &str = "OpenFrame";

/// Floor for every window the app opens, main and child alike. The UI's
/// narrowest layout still assumes this much room, and without it a child window
/// can be dragged down to a few pixels. There is deliberately no ceiling —
/// neither window has one.
const MIN_WINDOW_WIDTH: f64 = 1024.0;
const MIN_WINDOW_HEIGHT: f64 = 700.0;

/// Cap on a title mirrored in from a page, in characters. Long enough for any
/// real title; short enough that a window's titlebar entry stays a label.
const MAX_WINDOW_TITLE_CHARS: usize = 200;

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

pub(crate) const MAIN_LABEL: &str = "main";

/// The one URI scheme this app claims with the OS. Two hosts ride on it, and the
/// host is what tells them apart:
///
/// - `openframe-console://auth?devTicket=…` — the OAuth callback. The login runs
///   in the user's **default browser**, where their SSO session already is, so
///   the ticket cannot be read out of a webview we control; it has to be handed
///   back by the OS.
/// - `openframe-console://notify?context=…` — what a Windows toast activates
///   (`notifications::CLICK_URI_PREFIX`).
///
/// Registered on macOS by `Info.plist` (`CFBundleURLTypes`) and on Windows by
/// [`register_url_scheme`] (HKCU) — one scheme, one registration, both uses.
///
/// It is also the value pushed to the frontend as
/// `NEXT_PUBLIC_MOBILE_APP_SCHEME` (see [`env_init_script`]), which is what makes
/// it build `<scheme>://auth` as the gateway `redirectTo`. Those two uses must
/// stay the same string: the frontend names the callback, the OS delivers it.
///
/// The SaaS gateway only honours a redirect that appears verbatim in
/// `openframe.gateway.redirect.allowed-uris`, so that exact URI has to be in
/// that list or the callback is rewritten to the tenant root and the browser
/// keeps the ticket. The OSS gateway honours any requested redirect.
pub(crate) const URI_SCHEME: &str = "openframe-console";

/// Host that marks a URL on [`URI_SCHEME`] as the OAuth callback.
const AUTH_CALLBACK_HOST: &str = "auth";

/// How long a browser login may take before the parked `native_auth_start`
/// gives up. Generous on purpose: the person is in an SSO flow with a password
/// manager and possibly MFA, and unlike the old login window there is no close
/// event to cancel on — this timeout is the only thing that ends a login the
/// user walked away from.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Baked-in shared auth host, set at compile time
/// (`OPENFRAME_SHARED_HOST_URL=https://… cargo build`). Overridable per install
/// by `shared_host` in config.json, which is how a dev build points at a stage
/// gateway without a rebuild.
const DEFAULT_SHARED_HOST: Option<&str> = option_env!("OPENFRAME_SHARED_HOST_URL");

static WINDOW_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Emitted to the main window each time it comes back in front of the user
/// after having been away — revealed from the tray, or unminimized — with how
/// long the absence lasted. The first reveal after launch announces nothing:
/// the page is loading its data anyway, and a resync would only race it.
///
/// Everything the UI keeps live resyncs off this, chiefly the Mingo chat tail,
/// which the user can talk to from a notification while the window is away.
///
/// Deliberately independent of the page's own `visibilitychange`, which
/// [`set_webview_visible`] separately tries to make trustworthy on Windows.
/// That mirroring rests on a reading of WebView2's visibility model; this event
/// rests on nothing but the window events the shell already handles, so a wrong
/// reading costs a redundant refetch rather than a frozen conversation. The two
/// are coalesced page-side.
///
/// Addressed to the main window, though a `listen()` that names no target
/// receives it in any window — the same reach `notification:click` and
/// `native-auth:token-update` already have, and harmless here: a child window
/// running this bundle was away for the same absence.
///
/// Consumed by `onNativeShellResumed` in the frontend's native-shell.ts.
const RESUME_EVENT: &str = "shell:resumed";

/// Whether the main window is in front of the user, since when it has not been,
/// and what the webview was last told. Guarded because window events arrive on
/// the event loop while `show_primary_window` can be called from a
/// notification's async task.
struct MainPresence {
    present: bool,
    /// Wall clock, not `Instant`. `Instant` is `CLOCK_UPTIME_RAW` on macOS and
    /// stops while the machine sleeps (`tokens::spawn_wake_watch` measures the
    /// gap: 398.7h of wall time against 140.2h monotonic on one machine), so a
    /// window that sat in the tray overnight would report only the minutes the
    /// machine was awake — and could fall under the page's threshold in exactly
    /// the case the event exists for. The page judges this against `Date.now()`,
    /// so the two have to be on the same clock.
    hidden_since: Option<std::time::SystemTime>,
    /// What the webview was last told, `None` until it has been told anything.
    /// Purely to avoid repeating the call: `Resized` arrives per frame of a
    /// drag, and a WebView2 visibility call is not something to spend per
    /// frame. It is not a correctness signal — `mirror_webview` re-reads
    /// `present` and converges on that.
    webview_visible: Option<bool>,
}

/// Presence is tracked as a state machine rather than read off each event
/// because the events are not edges: `Resized` fires for every pixel of a frame
/// drag, and neither a resume announced on each one nor a WebView2 visibility
/// call issued on each one is something the user should pay for.
static MAIN_PRESENCE: Mutex<MainPresence> = Mutex::new(MainPresence {
    present: false,
    hidden_since: None,
    webview_visible: None,
});

/// Serializes deciding the webview's visibility, making the call, and recording
/// what was decided — three steps that must not interleave with another
/// thread's, or a suppressed call pairs with an executed one.
///
/// Separate from [`MAIN_PRESENCE`] because the presence lock is taken from
/// window-event handlers that must not be blocked behind an OS call. Always
/// acquired FIRST, with `MAIN_PRESENCE` only in short spans inside it — one
/// ordering, so the two cannot deadlock.
static MIRROR: Mutex<()> = Mutex::new(());

/// macOS only: the dock icon tracks whether the APP has a window on screen, not
/// whether the main one does. Close-to-tray used to drop straight to Accessory,
/// which with a child window still open read as "the app quit" while its windows
/// sat there — and an Accessory app has no dock icon left to click to get back.
/// Call this whenever a window goes away, naming the one that is leaving:
/// `is_visible` is not reliably false yet for the window currently being closed.
#[cfg(target_os = "macos")]
fn sync_activation_policy(app: &AppHandle, leaving: &str) {
    let still_on_screen = app
        .webview_windows()
        .iter()
        .any(|(label, w)| label != leaving && w.is_visible().unwrap_or(false));
    let policy = if still_on_screen {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    };
    let _ = app.set_activation_policy(policy);
}

/// Record whether the main window is in front of the user, mirror that onto the
/// webview, and — on the way back — say how long it was away so the UI can
/// catch up on what it missed.
fn set_main_presence(app: &AppHandle, present: bool) {
    let away = {
        let mut presence = notifications::lock(&MAIN_PRESENCE);
        if presence.present == present {
            None
        } else {
            presence.present = present;
            if present {
                // A clock stepped backwards mid-absence yields Err; report
                // nothing rather than a nonsense duration — the page treats a
                // missing figure as "not a resume", which is the safe read.
                presence
                    .hidden_since
                    .take()
                    .and_then(|since| since.elapsed().ok())
            } else {
                presence.hidden_since = Some(std::time::SystemTime::now());
                None
            }
        }
    };

    mirror_webview(app);
    // `None` is either no transition at all or the first reveal after launch.
    let Some(away) = away else {
        return;
    };
    // The absence is reported rather than judged: the page applies its own
    // "long enough to have missed something" threshold, the same one it applies
    // to a browser tab.
    let payload = serde_json::json!({ "hiddenMs": away.as_millis() as u64 });
    if let Err(e) = app.emit_to(MAIN_LABEL, RESUME_EVENT, payload) {
        log::warn!("emit {RESUME_EVENT}: {e}");
    }
}

/// Forget the main window entirely — it is gone, and the one that replaces it
/// starts from nothing. Distinct from `set_main_presence(app, false)`, which
/// starts the hidden-since clock a destroyed window has no use for.
fn clear_main_presence() {
    let mut presence = notifications::lock(&MAIN_PRESENCE);
    presence.present = false;
    presence.hidden_since = None;
    presence.webview_visible = None;
}

/// Bring the webview's visibility into line with the presence recorded for the
/// window, whatever that has become by now.
///
/// Converges rather than applies each caller's own decision: two threads can
/// decide opposite things before either acts, and a caller acting on a stale
/// decision could suppress its `show` against a memo the other thread was about
/// to invalidate — leaving a blank window the memo believes is fine. Re-reading
/// the intent inside [`MIRROR`] makes the last decision the one that runs.
///
/// One gap remains, and it is not closed here: `Webview::show`/`hide` run inline
/// on the event-loop thread but are queued from any other, so a call issued
/// earlier from a background thread can still land later. Every hide comes from
/// a window event (inline), and only shows are ever issued off-thread, so the
/// reachable skew is a webview left VISIBLE behind a hidden window — the state
/// this mechanism is trying to improve on, not a regression from it, and the
/// next presence edge corrects it.
fn mirror_webview(app: &AppHandle) {
    let _serialized = notifications::lock(&MIRROR);
    let want = {
        let presence = notifications::lock(&MAIN_PRESENCE);
        if presence.webview_visible == Some(presence.present) {
            return;
        }
        presence.present
    };
    // Resolved inside the lock, not handed in: waiting for `MIRROR` can outlast
    // the window itself (sign-out destroys and rebuilds it), and telling a dead
    // window anything would still record the live one as told. A destroyed
    // window also leaves nothing to mirror — `clear_main_presence` forgets what
    // it was told, so the replacement is mirrored on its first reveal.
    let Some(window) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    if set_webview_visible(&window, want) {
        notifications::lock(&MAIN_PRESENCE).webview_visible = Some(want);
    }
}

/// Tell the webview whether it is visible.
///
/// `Window::hide`/`show` move the OS window and stop there, which on Windows
/// leaves `CoreWebView2Controller.IsVisible` true and the page reporting itself
/// visible to nobody; Microsoft's guidance is that the host drives `IsVisible`
/// itself for exactly this reason. wry hides its own container child HWND, not
/// the top-level window, so this does not disturb the taskbar entry.
///
/// macOS is deliberately left alone: WKWebView already tracks its window's
/// occlusion, so `document.visibilityState` is correct there without help.
/// Reports whether the call went out, so a failure is not memoized as applied.
/// `Ok` means dispatched rather than applied — tauri-runtime-wry logs and
/// swallows the wry error on the event-loop side — so the only failure reaching
/// here is a dead event loop; leaving that unrecorded costs one redundant call
/// on the next presence edge and nothing else.
fn set_webview_visible(window: &WebviewWindow, visible: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        let webview: &tauri::Webview<_> = window.as_ref();
        let result = if visible {
            webview.show()
        } else {
            webview.hide()
        };
        if let Err(e) = result {
            log::warn!("set webview visible={visible}: {e}");
            return false;
        }
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, visible);
        true
    }
}

/// The shell knows exactly one configured URL: the **shared auth host**. There
/// is no tenant host to configure — the bundle's /auth pages discover the
/// tenant from the user's email (`/sas/tenant/discover` on the shared host) and
/// the frontend pushes that tenant's origin down once login succeeds. Same model as
/// openframe-mobile, minus mobile's optional build-time single-tenant pin.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AppConfig {
    /// Shared auth host, e.g. "https://auth.openframe.example" — tenant discovery,
    /// /oauth/login, /oauth/dev-exchange and /oauth/refresh all live there.
    /// Empty/absent falls back to [`DEFAULT_SHARED_HOST`]. Read through
    /// [`shared_host`], never directly.
    shared_host: Option<String>,
    /// Tenant origin learned at login — the host discovery resolved, since the
    /// scheme callback carries none — pushed by the frontend via
    /// NativeAuth.setTenantHost. Shell-side networking (token refresh, NATS)
    /// uses it. Written by the shell, not by hand.
    pub(crate) learned_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) self_update_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) update_manifest_url: Option<String>,
    /// Managed-install policy for start-at-login: `Some` pins it in that
    /// direction and takes the toggle away from the user. Written by fleet
    /// tooling, never by the shell. Absent leaves the choice to the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) autostart_enforced: Option<bool>,
    /// Whether the start-at-login default has already been applied on this
    /// machine. Shell-written, and the reason the default is applied once
    /// rather than re-asserted at every launch over a user who turned it off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) autostart_configured: Option<bool>,
    /// Set by an update that restarts while the user has a window open, so the
    /// process that comes back opens one even in a login session. Shell-written
    /// and cleared on the next launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) autostart_show_window_next_start: Option<bool>,
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

/// The tenant origin login learned, if there is one. Everything the shell talks
/// to on the tenant — NATS, the chat API, refresh's fallback base — resolves it
/// through here rather than reading `learned_host` directly, so "unset" and
/// "empty string" cannot mean different things in different callers.
pub(crate) fn tenant_host(cfg: &AppConfig) -> Option<String> {
    cfg.learned_host
        .as_deref()
        .filter(|host| !host.is_empty())
        .map(str::to_string)
}

/// RFC 3986 unreserved set. Anything the shell interpolates into a URL — an id
/// into a path, a bearer into a query — is escaped against this, so a value can
/// never smuggle in a path or a parameter of its own.
pub(crate) const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

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

pub(crate) fn save_config(app: &AppHandle, cfg: &AppConfig) -> Result<(), String> {
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
///     build omits `<PublicEnvScript />`). `NEXT_PUBLIC_TENANT_HOST_URL` is
///     deliberately absent so `runtimeEnv.tenantHostUrl()` falls through to the
///     host login learned (`getStoredTenantHost`), which is what lets one binary
///     serve any tenant. `NEXT_PUBLIC_MOBILE_APP_SCHEME` is the override
///     `runtimeEnv.appScheme()` reads for the OAuth callback: without it the
///     frontend defaults to mobile's `com.openframe.app`, which this app does
///     not claim with the OS — so the browser would hand the ticket to whatever
///     did (or to nothing), and the login would hang until it timed out.
/// (2) `window.__OPENFRAME_SHELL__.nativeAuth` — the login + token-custody
///     bridge, backed here by the native_auth_* Tauri commands. Load-bearing:
///     drop it and the frontend's `nativeAuthPlugin()` is null, which kills
///     desktop sign-in.
///
///   This used to be injected as a fake `window.Capacitor` with an
///   `isNativePlatform()` returning true, so the frontend's one "is this
///   native?" check covered desktop too. That impersonation is gone: the
///   frontend detects us from Tauri's own IPC globals (`lib/platform.ts`) and
///   reads this namespace for the bridge. Nothing here claims to be mobile, so
///   phone-only features (FCM push, biometrics, safe-area insets, Android
///   back) cannot switch on by accident. Method names still match
///   openframe-mobile's NativeAuthPlugin — one frontend interface, two
///   implementations — but only the methods desktop actually implements.
fn env_init_script(app: &AppHandle) -> String {
    let env = serde_json::json!({
        "NEXT_PUBLIC_SHARED_HOST_URL": shared_host(&load_config(app)).unwrap_or_default(),
        "NEXT_PUBLIC_APP_MODE": "saas-tenant",
        "NEXT_PUBLIC_ENABLE_DEV_TICKET_OBSERVER": "true",
        "NEXT_PUBLIC_MOBILE_APP_SCHEME": URI_SCHEME,
    });
    format!(
        r#"window.__ENV = {env};
window.__OPENFRAME_SHELL__ = {{
  nativeAuth: {{
    start: function (o) {{
      return window.__TAURI_INTERNALS__.invoke('native_auth_start', {{
        url: o.url, callbackScheme: o.callbackScheme
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

/// Mirror the page's title onto the native window title, the way a browser
/// titles a tab. Wired with `on_document_title_changed`, so the *webview*
/// reports its own title (KVO on WKWebView, equivalents on WebView2/WebKitGTK)
/// and it lands on the window that actually changed.
///
/// Deliberately NOT injected script calling a command: a child window has to be
/// built with the opener's webview configuration — macOS requires that for
/// `window.open` to work at all (`NewWindowResponse::Create`) — and that
/// configuration carries the user-content controller that hosts Tauri's IPC
/// handler. A child's `invoke` therefore arrives under the OPENER's identity, so
/// a script-driven sync left every child stuck on its opening title and retitled
/// the MAIN window whenever a child navigated.
fn mirror_document_title(window: WebviewWindow, title: String) {
    let title: String = title.trim().chars().take(MAX_WINDOW_TITLE_CHARS).collect();
    // A document with no title of its own must not blank the window's; it keeps
    // whatever it opened with.
    if title.is_empty() {
        return;
    }
    if let Err(e) = window.set_title(&title) {
        log::warn!(
            "[webview:{}] could not set window title: {e}",
            window.label()
        );
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
        .title(WINDOW_TITLE)
        .on_document_title_changed(mirror_document_title)
        .inner_size(1280.0, 860.0)
        .min_inner_size(MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)
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
                raise_or_open_main_window(&app);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Either the destroyed window never released the label, or something
        // else (the tray, a notification) already rebuilt it — both leave the
        // label taken, and only the first is a problem.
        log::warn!("reopen main window: label still taken after destroy");
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
            if window.label() == MAIN_LABEL {
                set_main_presence(window.app_handle(), true);
            }
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
        if window.label() == MAIN_LABEL {
            set_main_presence(window.app_handle(), true);
        }
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
        .title(WINDOW_TITLE)
        .on_document_title_changed(mirror_document_title)
        .inner_size(1100.0, 800.0)
        .min_inner_size(MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)
        .resizable(true)
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
///
/// The launcher is reaped on a thread of its own. On Unix it exits as soon as it
/// has handed the URL over, but `Child` does not reap on drop and this process
/// lives in the tray for days — so without this every link opened and every
/// sign-in would leave a zombie behind for the rest of the session. The exit
/// status is worth a line because sign-in hangs on it: a launcher that starts
/// and then fails to hand the URL over otherwise looks exactly like a gateway
/// that never redirected, five minutes later.
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

    let mut child = cmd.arg(url).spawn()?;
    std::thread::spawn(move || match child.wait() {
        Ok(status) if !status.success() => log::warn!("url launcher exited with {status}"),
        Err(e) => log::warn!("url launcher could not be waited on: {e}"),
        Ok(_) => {}
    });
    Ok(())
}

/// Raise the main window. Deliberately a no-op when there is none — except in a
/// headless session, where setup never builds one and the no-op would otherwise
/// last the whole process. On Windows a click delivered during setup would
/// otherwise build the window itself, and setup's own `open_main_window` would
/// then find it and reveal it unpainted.
/// Leaving the payload stashed keeps the reveal with `handle_page_load`.
/// Callers acting on a direct user request want [`raise_or_open_main_window`].
pub(crate) fn show_primary_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window(MAIN_LABEL) else {
        // The refusal above rests on setup building a window at all. A headless
        // session never does — so there it protects nothing and would instead
        // make every notification click do nothing for the life of the process.
        // Keyed on the decision setup actually reached, not on the login flag:
        // a login start that an update asked to show a window *does* build one,
        // and racing it is the hazard.
        if autostart::is_headless() {
            raise_or_open_main_window(app);
        }
        return;
    };
    {
        #[cfg(target_os = "macos")]
        let _ = app.set_activation_policy(ActivationPolicy::Regular);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        // After the reveal is requested, not after it lands: off the main
        // thread these are queued messages, so the resume can reach the page a
        // frame before the window does. Harmless — the page's work is a
        // refetch, not a paint.
        set_main_presence(app, true);
        // The window may have sat in the tray for hours. Get the session current
        // before the page's first request 401s into the refresh-or-logout path.
        tokens::refresh_soon(app, "window shown");
    }
}

/// The user asked for the app — from the tray, a relaunch, or a notification
/// button that turned out to need a window. Unlike [`show_primary_window`] this
/// builds one when the process has none, which a `-ToastActivated` launch does:
/// it opens no window in setup, so without this the tray would be inert for the
/// life of that process.
pub(crate) fn raise_or_open_main_window(app: &AppHandle) {
    if let Err(e) = open_main_window(app) {
        log::error!("open main window: {e}");
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
// NativeAuth bridge — backs window.__OPENFRAME_SHELL__.nativeAuth (see
// env_init_script). Default browser -> openframe-console://auth?devTicket=
// capture -> native token exchange -> local token store. Mirrors
// openframe-mobile's NativeAuthPlugin.swift, with the browser in place of
// mobile's ASWebAuthenticationSession.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AuthState {
    /// The login `native_auth_start` is waiting on. May hold a sender whose
    /// receiver is already gone — a start that timed out leaves its own behind
    /// rather than clearing the slot, because a clear would drop the sender of
    /// whichever login had replaced it and cancel that one instead.
    /// [`finish_login`] handles the dead sender it may find.
    ///
    /// What nothing here can do is tie a callback to the attempt that asked for
    /// it: the callback carries nothing to match on, since the gateway
    /// allow-lists the redirect URI by exact string equality and no nonce of
    /// ours can ride on it (the same gap that leaves the flow without CSRF
    /// protection — see docs/auth-and-notifications.md). So a first browser tab
    /// finished after the user has already started a second attempt resolves
    /// that second attempt, with a ticket old enough that the exchange fails.
    pending_login: Mutex<Option<tokio::sync::oneshot::Sender<Result<String, String>>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeAuthStartResult {
    callback_url: String,
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn url_carries_ticket(url: &url::Url) -> bool {
    url.query_pairs().any(|(k, _)| k == "devTicket")
}

/// Whether a URL the OS handed us is the OAuth callback — our scheme, our
/// `auth` host. Anything else on the scheme (a stray link, a future route) is
/// not a login result and must not resolve one.
///
/// The host is compared case-insensitively because `Url::parse` does not
/// normalise it: for a non-special scheme like ours the host is opaque and kept
/// verbatim, so `//AUTH` would otherwise read as "not a callback" and leave the
/// login to run out its whole timeout.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn is_auth_callback(url: &url::Url) -> bool {
    url.scheme() == URI_SCHEME
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(AUTH_CALLBACK_HOST))
}

/// Route a URL the OS delivered on our registered scheme. Returns `true` only
/// when it was the OAuth callback **and** a login was waiting for it — an
/// unsolicited or long-stale callback is consumed silently, because acting on
/// one would let any page that can reach the scheme drive this app's window.
///
/// A callback that *cold-starts* the app therefore does nothing: the login it
/// belonged to died with the process that asked for it, and its ticket has a
/// two-minute life. Stashing it for the page about to load would mean completing
/// a sign-in nobody in this process asked for, which is the property this
/// nonce-less callback is least able to justify.
///
/// A callback **without** a ticket fails the login rather than being ignored:
/// the gateway appends one to every `authMobile` callback, so its absence means
/// the flow never reached the BFF callback, and waiting out the full timeout
/// would tell the user nothing.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn handle_scheme_url(app: &AppHandle, url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !is_auth_callback(&parsed) {
        return false;
    }
    let result = if url_carries_ticket(&parsed) {
        Ok(url.to_string())
    } else {
        Err("callback carried no ticket".into())
    };
    finish_login(app, result)
}

/// Resolve the pending native_auth_start(), reporting whether there was one.
/// Only the first resolution wins; a callback arriving with nothing parked — a
/// stale link, or a login that already timed out — is a no-op beyond the log.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn finish_login(app: &AppHandle, result: Result<String, String>) -> bool {
    let state: State<'_, AuthState> = app.state();
    let Some(tx) = notifications::lock(&state.pending_login).take() else {
        log::info!("[auth] scheme callback arrived with no login waiting — ignored");
        return false;
    };
    match &result {
        // Scheme and host only — the devTicket rides in the query string.
        Ok(callback) => log::info!(
            "[auth] devTicket callback captured (from: {})",
            url::Url::parse(callback)
                .map(|u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default()))
                .unwrap_or_else(|_| "<unparsed>".into())
        ),
        Err(e) => log::info!("[auth] login ended without a ticket: {e}"),
    }
    // A send that fails means the waiter gave up between the take above and here
    // — say so rather than let the capture line stand as the last word.
    if tx.send(result).is_err() {
        log::info!("[auth] the login it belonged to had already ended — callback dropped");
        return false;
    }
    true
}

/// NativeAuth.start: run the gateway BFF OAuth flow in the user's **default
/// browser** and resolve with the callback URL carrying `?devTicket=`.
///
/// The browser is the point. A login window of our own is a fresh cookie jar, so
/// an identity provider the user is already signed into on this machine cannot
/// see that session and asks for credentials again; the default browser holds
/// it. It also keeps us out of the embedded-webview user agents Google rejects
/// with `disallowed_useragent`.
///
/// The price is that nothing about the flow is observable from here any more —
/// no navigation callbacks, no window to close. The ticket comes back only if
/// the gateway redirects to [`URI_SCHEME`] and the OS hands us that URL
/// ([`handle_scheme_url`]), which is why `callback_scheme` is checked against
/// what we actually registered instead of being taken on faith. The shell hands
/// the frontend this very value as `NEXT_PUBLIC_MOBILE_APP_SCHEME`, so any
/// mismatch is a misconfiguration, and against a gateway that allow-lists the
/// redirect by exact string equality it ends in the timeout below — better to
/// fail immediately, naming the env var, than to hang for five minutes.
///
/// The old "accept ANY url carrying a ticket" fallback is gone with the window.
/// A gateway that rewrites the redirect to the tenant root now leaves the ticket
/// in the browser where this process cannot reach it, so on the SaaS side this
/// flow *requires* the callback in `openframe.gateway.redirect.allowed-uris`.
#[tauri::command]
async fn native_auth_start(
    state: State<'_, AuthState>,
    url: String,
    callback_scheme: Option<String>,
) -> Result<NativeAuthStartResult, String> {
    let login_url = url::Url::parse(&url).map_err(|e| format!("Invalid login URL: {e}"))?;
    // This URL is about to be handed to the OS launcher, which will open
    // whatever it names — a `file:` or shell-handled scheme would be an
    // application launch, not a login. Same gate `handle_new_window` puts in
    // front of the same launcher for the same reason.
    if !matches!(login_url.scheme(), "http" | "https") {
        log::error!("[auth] refusing to open a {} login URL", login_url.scheme());
        return Err("Login URL must be http(s)".into());
    }
    let scheme = callback_scheme.as_deref().unwrap_or("<none>");
    if scheme != URI_SCHEME {
        log::error!(
            "[auth] frontend asked to complete on {scheme}://, which this app does not claim \
             — check NEXT_PUBLIC_MOBILE_APP_SCHEME"
        );
        return Err(match callback_scheme {
            Some(_) => format!("Login callback scheme {scheme} is not the registered {URI_SCHEME}"),
            None => "Login started without a callback scheme".into(),
        });
    }
    log::info!(
        "[auth] opening login in the default browser at {}",
        login_url.origin().ascii_serialization()
    );

    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Some(stale) = notifications::lock(&state.pending_login).replace(tx) {
        let _ = stale.send(Err("USER_CANCELED".into()));
    }

    // `spawn`, so nothing here waits on the browser, and no shell, so the URL's
    // own characters can't be read as flags or commands.
    if let Err(e) = open_external(login_url.as_str()) {
        log::error!("[auth] could not open the default browser: {e}");
        return Err(format!("Could not open the browser: {e}"));
    }

    match tokio::time::timeout(LOGIN_TIMEOUT, rx).await {
        Ok(Ok(result)) => result.map(|callback_url| NativeAuthStartResult { callback_url }),
        // Sender dropped without sending. A replacement sends USER_CANCELED
        // rather than dropping, so in practice this is process teardown.
        Ok(Err(_)) => Err("USER_CANCELED".into()),
        Err(_) => {
            log::warn!(
                "[auth] no callback on {URI_SCHEME}://{AUTH_CALLBACK_HOST} within {}s — the \
                 browser flow never came back (is the callback in the gateway's \
                 redirect.allowed-uris?)",
                LOGIN_TIMEOUT.as_secs()
            );
            Err("LOGIN_TIMEOUT".into())
        }
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
///
/// A transient failure resolves with the tokens we still hold rather than
/// rejecting. The webview cannot act on the difference: it maps every refresh
/// that doesn't yield an access token to `forceLogout`, which calls back into
/// `native_auth_clear_tokens` and deletes a refresh token that is good for days
/// — so a rejection here turns one failed request into a lost session. Resolving
/// with the stored set makes the caller retry that request (and fail it, which
/// is not terminal) while the shell keeps retrying the rotation on its own.
/// Deciding a session is over stays where custody is: `tokens::refresh` clears
/// the store itself and then resolves empty, which the webview does act on.
#[tauri::command]
async fn native_auth_refresh_tokens(app: AppHandle) -> Result<NativeAuthTokens, String> {
    log::info!("[tokens] webview requested a refresh (upstream 401)");
    let prev_access = tokens::load_tokens(&app).access_token;
    match tokens::refresh(&app, true, prev_access, "webview upstream 401").await {
        Ok(tokens) => Ok(tokens),
        Err(e) => {
            let stored = tokens::load_tokens(&app);
            if stored.access_token.is_some() && stored.refresh_token.is_some() {
                log::warn!(
                    "[tokens] delegated refresh failed transiently ({e}) — keeping the session"
                );
                return Ok(stored);
            }
            Err(e)
        }
    }
}

/// Merge semantics and the lock they need both live in `tokens::merge_and_save`:
/// a write from the webview must not land on top of a rotation the shell
/// completed in between.
#[tauri::command]
async fn native_auth_set_tokens(
    app: AppHandle,
    access_token: Option<String>,
    refresh_token: Option<String>,
) -> Result<(), String> {
    log::info!(
        "[tokens] webview stored tokens (access: {}, refresh: {})",
        access_token.is_some(),
        refresh_token.is_some()
    );
    tokens::merge_and_save(&app, access_token, refresh_token).await?;
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

/// Deliver a scheme URI carried in a process's arguments — how Windows delivers
/// both an OAuth callback and a toast click, either as our own argv (cold start)
/// or forwarded by the single-instance plugin (warm). macOS delivers neither
/// this way: URL activation there is an Apple Event, which reaches us as
/// `RunEvent::Opened`.
///
/// Notification delivery raises the window only when there already is one, so
/// callers still have to decide whether this process should have one at all.
#[cfg(target_os = "windows")]
fn handle_scheme_argv(app: &AppHandle, args: &[String]) {
    for arg in args {
        if handle_scheme_url(app, arg) || notifications::handle_notification_uri(app, arg) {
            return;
        }
    }
}

/// Register [`URI_SCHEME`] for this executable (HKCU, no elevation). Re-written
/// on every launch so the command always points at the current exe path.
///
/// Windows has no equivalent of the macOS bundle's `CFBundleURLTypes`, so the
/// one scheme both uses ride on — the OAuth callback and toast clicks — is
/// written here.
#[cfg(target_os = "windows")]
fn register_url_scheme() {
    use winreg::{enums::*, RegKey};

    let Ok(exe) = std::env::current_exe() else {
        log::warn!("[scheme] current_exe unavailable — sign-in and toast clicks will not work");
        return;
    };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = format!(r"Software\Classes\{URI_SCHEME}");
    // `URL Protocol` is what marks the key as a protocol handler and the command
    // under it is what the shell dispatches to; with either missing, an
    // activation resolves to nothing and is lost in the shell, not here. So they
    // are written as one fallible sequence rather than four discarded results —
    // same reason `windows_activator::register` is. The default value is only the
    // friendly name the shell shows the user (in "How do you want to open this?",
    // say), so it is a product name and its failure is not worth reporting.
    let written = hkcu
        .create_subkey(&path)
        .and_then(|(key, _)| {
            let _ = key.set_value("", &"URL:OpenFrame Console");
            key.set_value("URL Protocol", &"")?;
            key.create_subkey(r"shell\open\command")
        })
        .and_then(|(command, _)| command.set_value("", &format!("\"{}\" \"%1\"", exe.display())));
    match written {
        Ok(()) => log::info!("[scheme] {URI_SCHEME} registered"),
        Err(err) => log::warn!(
            "[scheme] {URI_SCHEME} not registered ({err}) — sign-in and toast clicks will not \
             work"
        ),
    }
}

/// Delete the HKCU keys a pre-rename build wrote under its own identity: the
/// `openframe-desktop` scheme, whose `shell\open\command` still names the old
/// exe, and the AppUserModelId key registering that build's toast activator.
///
/// The scheme one is not inert. A toast left in the Action Center from before the
/// rename activates `openframe-desktop://notify`, which the shell resolves to the
/// old binary — so an update lands, and a stale notification still starts the
/// build it replaced. Nothing writes these keys any more, and the old build
/// rewrites them if it does get launched, so this only converges over launches of
/// the renamed build; it is still the difference between a stale toast opening
/// the wrong app once and doing it forever.
///
/// Deletable once no machine still has a pre-rename build on it, alongside
/// `autostart::remove_legacy_registration`.
#[cfg(target_os = "windows")]
fn remove_legacy_url_scheme() {
    use winreg::{enums::*, RegKey};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for path in [
        r"Software\Classes\openframe-desktop",
        r"Software\Classes\AppUserModelId\com.openframe.desktop",
    ] {
        match hkcu.delete_subkey_all(path) {
            Ok(()) => log::info!("[scheme] removed the legacy key {path}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("[scheme] could not remove the legacy key {path}: {e}"),
        }
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
    // Built with placeholder flags and then synced, so how a `Status` maps onto
    // the item — checked when enabled, greyed when a policy pins it — is
    // decided in exactly one place. The sync below is what makes it true, and
    // is not redundant with the startup reconcile: that one does not run at all
    // in a debug build.
    let autostart_i = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start at Login",
        true,
        false,
        None::<&str>,
    )?;
    let signout_i = MenuItem::with_id(app, "signout", "Sign Out", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &autostart_i, &signout_i, &quit_i])?;
    // Held so the webview's toggle moves the same check mark the tray does.
    app.manage(autostart::TrayCheck(autostart_i));
    autostart::sync_tray(app.handle());

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
                raise_or_open_main_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => raise_or_open_main_window(app),
            "autostart" => autostart::toggle_from_tray(app),
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
            // A browser handing back the OAuth callback, or a toast click,
            // reaches the running instance on Windows as a second launch
            // carrying the URI in argv. Anything else is the user re-launching
            // the app — except a COM server launch, which only happens if this
            // instance failed to register the activator, and must not put a
            // window on screen for a press meant to run in the background.
            #[cfg(target_os = "windows")]
            let handled = {
                // Deliver first, decide about the window after: a click only
                // stashes its payload when there is no window to emit at, and
                // this process may well have none — a COM server launch opens
                // none, and it is the one case that must stay windowless. An
                // OAuth callback deliberately does not count as handled: the
                // browser has the focus at that moment, and the app the user is
                // being returned to should come forward.
                handle_scheme_argv(app, &argv);
                windows_activator::is_activation_launch(&argv)
            };
            #[cfg(not(target_os = "windows"))]
            let handled = {
                let _ = argv;
                false
            };
            if !handled {
                raise_or_open_main_window(app);
            }
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("openframe-console".into()),
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AuthState::default())
        .manage(tokens::TokenLifecycle::default())
        .manage(updater::UpdateManager::default())
        .invoke_handler(tauri::generate_handler![
            native_auth_start,
            native_auth_exchange_ticket,
            native_auth_get_tokens,
            native_auth_refresh_tokens,
            native_auth_set_tokens,
            native_auth_clear_tokens,
            native_auth_set_tenant_host,
            take_pending_notification_click,
            webview_log,
            updater::update_check,
            updater::update_apply_now,
            autostart::autostart_status,
            autostart::autostart_set
        ])
        .setup(|app| {
            build_tray(app)?;
            #[cfg(target_os = "windows")]
            {
                register_url_scheme();
                remove_legacy_url_scheme();
            }
            tokens::spawn_wake_watch(app.handle().clone());
            tokens::spawn_refresh_loop(app.handle().clone());
            #[cfg(target_os = "macos")]
            macos_wake::observe(app.handle().clone());
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
            // Read before the toast-activation return below, so a request left
            // behind by an update that never landed is cleared rather than
            // sitting in config.json until some later boot honours it. Costs
            // nothing on that path, which opens no window either way.
            let asked_for_a_window = autostart::take_show_window_request(&handle);

            // Cold start from a Windows protocol activation — a toast click, or
            // an OAuth callback that arrived after the app was quit — put the
            // URI in our own argv. Runs before the window exists so a click
            // payload is stashed (the webview pulls it via
            // take_pending_notification_click) instead of being shown early —
            // handle_page_load still owns the reveal, so no unpainted flash.
            // `args_os`, not `args`: the latter panics on non-UTF-8 arguments.
            #[cfg(target_os = "windows")]
            {
                let argv: Vec<String> = std::env::args_os()
                    .filter_map(|arg| arg.into_string().ok())
                    .collect();
                handle_scheme_argv(&handle, &argv);
                // COM started this process only to serve a button press, and a
                // press that completes in the background must not put a window
                // on screen for it. Which press it is arrives after setup, so
                // the activator raises the window itself for the ones that need
                // one. The process stays resident afterwards rather than exiting
                // — the same outcome as a macOS relaunch, and it means a press
                // on a stale toast after the user chose Quit brings the app back
                // to the tray.
                if windows_activator::is_activation_launch(&argv) {
                    log::info!("started to serve a toast activation — not opening a window");
                    return Ok(());
                }
            }

            // After the toast-activation return above: a COM server launch is
            // transient and has no business rewriting a login registration.
            autostart::reconcile_on_startup(&handle);

            // Started by the OS at login, not by the user. Everything the app
            // is useful for while closed — notifications, a live session — is
            // already running by this point; the window is the one thing a
            // login start must not put on screen.
            let headless = autostart::launched_at_login() && !asked_for_a_window;
            // Recorded so `show_primary_window` can tell "no window yet" from
            // "no window ever", which argv alone cannot answer.
            autostart::set_headless(headless);
            if headless {
                log::info!("[autostart] started at login — staying in the tray");
                #[cfg(target_os = "macos")]
                let _ = handle.set_activation_policy(ActivationPolicy::Accessory);
            }

            updater::spawn_poll_loop(handle.clone());
            let startup_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                // Runs headless too: a login is the best moment to take an
                // update, and `restart()` re-spawns with our own argv, so the
                // relaunch is still a login start.
                updater::run_startup_update(&startup_handle).await;
                if headless {
                    return;
                }
                if let Err(e) = open_main_window(&startup_handle) {
                    log::error!("failed to open main window, exiting: {e}");
                    startup_handle.exit(1);
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Close the main window to tray; let child (new-tab) windows close normally.
            WindowEvent::CloseRequested { api, .. } => {
                if window.label() == MAIN_LABEL {
                    api.prevent_close();
                    let _ = window.hide();
                    set_main_presence(window.app_handle(), false);
                    #[cfg(target_os = "macos")]
                    sync_activation_policy(window.app_handle(), MAIN_LABEL);
                }
            }
            // Minimize and restore reach the shell as a resize and nothing else
            // — there is no minimize event on desktop — so presence is read back
            // off the window rather than inferred from the payload. Cheap: both
            // reads are inline Win32 calls on the event-loop thread, and the
            // presence gate drops every resize that is not an edge.
            WindowEvent::Resized(_) if window.label() == MAIN_LABEL => {
                let present =
                    !window.is_minimized().unwrap_or(false) && window.is_visible().unwrap_or(true);
                set_main_presence(window.app_handle(), present);
            }
            // Sign-out destroys and recreates the main window; park clicks from
            // here until the replacement's listener mounts, rather than emitting
            // them at a label that currently has no window.
            WindowEvent::Destroyed if window.label() == MAIN_LABEL => {
                notifications::reset_click_gate(window.app_handle());
                // A new window is a new page, which loads its own data — so it
                // must reveal as a first reveal, not as a return from however
                // long the destroyed one had been in the tray. Clearing the
                // clock is the part `set_main_presence` cannot do for us: for
                // an already-hidden window this is no transition at all.
                clear_main_presence();
            }
            // The main window is already hidden by the time a child is closed,
            // so nothing else would ever take the app back out of the dock.
            #[cfg(target_os = "macos")]
            WindowEvent::Destroyed if window.label() != MAIN_LABEL => {
                sync_activation_policy(window.app_handle(), window.label());
            }
            // The user is looking at the app — clear the unread badge.
            WindowEvent::Focused(true) if window.label() == MAIN_LABEL => {
                notifications::on_main_focused(window.app_handle());
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            #[cfg(not(target_os = "macos"))]
            let _ = app;
            // A tray app outlives its windows. Tauri asks to exit as soon as the
            // last one is DESTROYED (not hidden), and tray "Sign Out" destroys
            // the main window on purpose to rebuild it signed-out — so the
            // process died there before `reopen_main_window` could put the
            // window back, and the tray went with it. Close-to-tray never hit
            // this: it hides the window, which leaves it in the window list.
            //
            // `code` is what separates the two: only "every window is gone"
            // arrives as None. The tray's Quit (`app.exit(0)`) and the updater's
            // restart both carry one and still go through — and `prevent_exit`
            // is ignored for a restart regardless.
            if let tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } = &event
            {
                api.prevent_exit();
            }
            // macOS delivers a URL activation as an Apple Event, not in argv —
            // this is the only path an OAuth callback can take to reach a
            // running app there. Raise the window when a login was actually
            // resolved, success or failure: the browser has the focus and the
            // user is being handed back to the app to see the result. A callback
            // nobody was waiting on raises nothing here — Windows is necessarily
            // laxer, since there the same URL arrives as a second process launch
            // and the single-instance handler raises for any relaunch.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = &event {
                for url in urls {
                    if handle_scheme_url(app, url.as_str()) {
                        raise_or_open_main_window(app);
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> url::Url {
        url::Url::parse(url).unwrap()
    }

    // The callback the gateway sends when it honours the requested redirect
    // (`openframe.gateway.redirect.allowed-uris`): a non-special URL still
    // parses, the host is `auth`, and the ticket is in its query.
    #[test]
    fn scheme_callback_is_recognised_and_carries_the_ticket() {
        let url = parse("openframe-console://auth?devTicket=abc123");
        assert!(is_auth_callback(&url));
        assert!(url_carries_ticket(&url));
    }

    // The gateway appends the ticket with `&` when the redirect already has a
    // query (OAuthBffController), so the parse must survive both separators.
    #[test]
    fn ticket_is_found_after_an_existing_query() {
        assert!(url_carries_ticket(&parse(
            "openframe-console://auth?state=x&devTicket=abc123"
        )));
    }

    // Ends the login as a failure rather than hanging: the gateway appends a
    // ticket to every authMobile callback, so one without means the flow never
    // reached the BFF callback.
    #[test]
    fn scheme_callback_without_a_ticket_is_still_a_callback() {
        let url = parse("openframe-console://auth");
        assert!(is_auth_callback(&url));
        assert!(!url_carries_ticket(&url));
    }

    // What a gateway that does not allow-list our redirect does with it: the
    // ticket lands on the tenant root, in the user's browser, where this process
    // cannot see it. Not a callback — the old login window could read this, the
    // browser flow cannot, and pretending otherwise would be a lie.
    #[test]
    fn https_tenant_landing_is_not_a_callback() {
        assert!(!is_auth_callback(&parse(
            "https://acme.openframe.example/?devTicket=abc123"
        )));
    }

    // Our scheme, but not the login route — a toast URI shape, or anything else
    // we may answer on later.
    #[test]
    fn other_hosts_on_our_scheme_are_not_callbacks() {
        assert!(!is_auth_callback(&parse(
            "openframe-console://notify?context=%7B%7D"
        )));
    }

    // `Url::parse` normalises the scheme but not an opaque host, so the host
    // needs its own case-insensitive compare to survive a callback that comes
    // back shouting.
    #[test]
    fn the_callback_host_is_matched_regardless_of_case() {
        assert!(is_auth_callback(&parse(
            "openframe-console://AUTH?devTicket=abc123"
        )));
    }

    // The macOS half of the scheme registration lives in a plist the compiler
    // never reads, so nothing but this test would notice it drifting away from
    // URI_SCHEME — and the way it fails is silent: the OS simply hands the
    // callback to nobody and sign-in hangs until it times out.
    #[test]
    fn the_bundle_plist_registers_the_scheme_we_answer_on() {
        let plist = include_str!("../Info.plist");
        assert!(
            plist.contains(&format!("<string>{URI_SCHEME}</string>")),
            "Info.plist does not register {URI_SCHEME} in CFBundleURLSchemes"
        );
    }
}
