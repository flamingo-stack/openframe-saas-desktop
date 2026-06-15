use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::{Color, NewWindowFeatures, NewWindowResponse, PageLoadEvent, PageLoadPayload},
    Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

/// App background applied to every window so no white frame shows before the
/// page paints (matches the dark OpenFrame UI).
const WINDOW_BG: Color = Color(22, 22, 22, 255);

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

const MAIN_LABEL: &str = "main";
const CONNECT_LABEL: &str = "connect";

static WINDOW_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Default, Serialize, Deserialize)]
struct AppConfig {
    /// Fully-qualified tenant origin, e.g. "https://acme.openframe.ai" (no trailing slash).
    host: Option<String>,
}

fn config_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_config_dir()
        .expect("resolve app config dir")
        .join("config.json")
}

fn load_config(app: &tauri::AppHandle) -> AppConfig {
    std::fs::read_to_string(config_path(app))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_config(app: &tauri::AppHandle, cfg: &AppConfig) -> Result<(), String> {
    let path = config_path(app);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, data).map_err(|e| e.to_string())
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

fn open_main_window(app: &tauri::AppHandle, host: &str) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(MAIN_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    let base = url::Url::parse(host).map_err(|e| e.to_string())?;
    let handler_app = app.clone();
    let window = WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::External(base))
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
/// app pages open in a new in-app window (sharing the logged-in session),
/// external http(s) links open in the system browser, other schemes are blocked.
///
/// "In-app" is decided against the *live* origin of an open window — not the
/// configured host — so app links stay in-app even when the login flow redirects
/// across hosts (configured host -> shared auth host -> the tenant you end up on).
fn handle_new_window(
    app: &tauri::AppHandle,
    url: url::Url,
    features: NewWindowFeatures,
) -> NewWindowResponse<tauri::Wry> {
    if !matches!(url.scheme(), "http" | "https") {
        log::warn!("blocked new window for scheme '{}': {url}", url.scheme());
        return NewWindowResponse::Deny;
    }

    let target_origin = url.origin();
    let in_app = app
        .webview_windows()
        .values()
        .any(|w| w.url().map(|u| u.origin() == target_origin).unwrap_or(false));

    if in_app {
        let n = WINDOW_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Cascade so each window is offset instead of stacking exactly on top of
        // the previous one (which made multiple windows look like one). Wraps after 6.
        let offset = (n % 6) as f64 * 36.0;
        // Attach the same handler to the child so links opened from it also work.
        let child_app = app.clone();
        match WebviewWindowBuilder::new(app, format!("child-{n}"), WebviewUrl::External(url.clone()))
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

fn open_connect_window(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(CONNECT_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(app, CONNECT_LABEL, WebviewUrl::App("index.html".into()))
        .title("Connect to OpenFrame")
        .inner_size(520.0, 600.0)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn show_primary_window(app: &tauri::AppHandle) {
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
fn get_tenant_host(app: tauri::AppHandle) -> Option<String> {
    load_config(&app).host
}

#[tauri::command]
fn set_tenant_host(app: tauri::AppHandle, host: String) -> Result<(), String> {
    log::info!("set_tenant_host: received {host:?}");
    let normalized = normalize_host(&host).inspect_err(|e| log::error!("normalize: {e}"))?;
    save_config(&app, &AppConfig { host: Some(normalized.clone()) })
        .inspect_err(|e| log::error!("save_config: {e}"))?;
    open_main_window(&app, &normalized).inspect_err(|e| log::error!("open_main_window: {e}"))?;
    if let Some(connect) = app.get_webview_window(CONNECT_LABEL) {
        let _ = connect.close();
    }
    log::info!("set_tenant_host: opened {normalized}");
    Ok(())
}

#[tauri::command]
fn switch_instance(app: tauri::AppHandle) -> Result<(), String> {
    save_config(&app, &AppConfig { host: None })?;
    open_connect_window(&app)?;
    if let Some(main) = app.get_webview_window(MAIN_LABEL) {
        let _ = main.close();
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
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_tenant_host,
            set_tenant_host,
            switch_instance
        ])
        .setup(|app| {
            build_tray(app)?;

            let handle = app.handle().clone();
            match load_config(&handle).host {
                Some(host) => open_main_window(&handle, &host),
                None => open_connect_window(&handle),
            }
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Close the primary window to tray; let child (new-tab) windows close normally.
            if let WindowEvent::CloseRequested { api, .. } = event {
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
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
