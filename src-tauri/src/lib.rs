use serde::{Deserialize, Serialize};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

const MAIN_LABEL: &str = "main";
const CONNECT_LABEL: &str = "connect";

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
    let url = url::Url::parse(host).map_err(|e| e.to_string())?;
    WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::External(url))
        .title("OpenFrame")
        .inner_size(1280.0, 860.0)
        .min_inner_size(1024.0, 700.0)
        .resizable(true)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
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
    let normalized = normalize_host(&host)?;
    save_config(&app, &AppConfig { host: Some(normalized.clone()) })?;
    open_main_window(&app, &normalized)?;
    if let Some(connect) = app.get_webview_window(CONNECT_LABEL) {
        let _ = connect.close();
    }
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
        .plugin(tauri_plugin_log::Builder::new().build())
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
            // Close to tray instead of quitting.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                #[cfg(target_os = "macos")]
                let _ = window
                    .app_handle()
                    .set_activation_policy(ActivationPolicy::Accessory);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
