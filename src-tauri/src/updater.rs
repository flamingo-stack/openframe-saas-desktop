use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;

use crate::{load_config, tokens, AppConfig, MAIN_LABEL};

const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/flamingo-stack/openframe-saas-desktop/releases/latest/download/updater.json";
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const RUNTIME_POLL_INTERVAL: Duration = Duration::from_secs(45 * 60);

const AGENT_TYPE: &str = "openframe-desktop";
const REPORT_RETRIES: u32 = 5;

const EVENT_AVAILABLE: &str = "update:available";
const EVENT_PROGRESS: &str = "update:progress";
const EVENT_ERROR: &str = "update:error";

#[derive(Default)]
pub struct UpdateManager {
    apply_lock: Mutex<()>,
}

fn self_update_enabled(cfg: &AppConfig) -> bool {
    cfg.self_update_enabled.unwrap_or(true)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InstalledAgentReport {
    agent_type: &'static str,
    version: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAvailability {
    available: bool,
    version: Option<String>,
    notes: Option<String>,
}

impl UpdateAvailability {
    fn none() -> Self {
        Self {
            available: false,
            version: None,
            notes: None,
        }
    }

    fn from_update(update: &Update) -> Self {
        Self {
            available: true,
            version: Some(update.version.clone()),
            notes: update.body.clone(),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    downloaded: usize,
    total: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorPayload {
    message: String,
}

fn resolve_manifest_url(app: &AppHandle) -> String {
    if let Some(url) = load_config(app)
        .update_manifest_url
        .filter(|s| !s.is_empty())
    {
        return url;
    }
    if let Some(url) = option_env!("OPENFRAME_UPDATE_MANIFEST_URL").filter(|s| !s.is_empty()) {
        return url.to_string();
    }
    DEFAULT_MANIFEST_URL.to_string()
}

async fn check_for_update(app: &AppHandle) -> Result<Option<Update>, String> {
    let manifest_url = resolve_manifest_url(app);
    let endpoint = url::Url::parse(&manifest_url)
        .map_err(|e| format!("invalid manifest URL '{manifest_url}': {e}"))?;

    let updater = app
        .updater_builder()
        .timeout(CHECK_TIMEOUT)
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;

    updater.check().await.map_err(|e| e.to_string())
}

async fn apply(app: &AppHandle, manager: &UpdateManager, mut update: Update) -> Result<(), String> {
    let _guard = match manager.apply_lock.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            log::info!("[updater] apply already in progress — ignoring");
            return Ok(());
        }
    };

    update.timeout = Some(DOWNLOAD_TIMEOUT);
    log::info!("[updater] applying update to {}", update.version);
    let progress_app = app.clone();
    let result = update
        .download_and_install(
            move |downloaded, total| {
                let _ = progress_app.emit_to(
                    MAIN_LABEL,
                    EVENT_PROGRESS,
                    Progress { downloaded, total },
                );
            },
            || log::info!("[updater] download complete, installing"),
        )
        .await;

    if let Err(e) = result {
        let message = e.to_string();
        log::error!("[updater] install failed: {message}");
        let _ = app.emit_to(
            MAIN_LABEL,
            EVENT_ERROR,
            ErrorPayload {
                message: message.clone(),
            },
        );
        return Err(message);
    }

    log::info!("[updater] installed {} — restarting", update.version);
    app.restart();
    #[allow(unreachable_code)]
    Ok(())
}

pub async fn run_startup_update(app: &AppHandle) {
    if !self_update_enabled(&load_config(app)) {
        log::info!("[updater] self-update disabled — skipping startup check");
        return;
    }
    let manager = app.state::<UpdateManager>();
    match check_for_update(app).await {
        Ok(Some(update)) => {
            log::info!(
                "[updater] startup: {} available, applying before window",
                update.version
            );
            if let Err(e) = apply(app, &manager, update).await {
                log::error!("[updater] startup apply failed, continuing on current version: {e}");
            }
        }
        Ok(None) => log::info!("[updater] startup: up to date"),
        Err(e) => log::warn!("[updater] startup check failed: {e}"),
    }
}

pub fn spawn_poll_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(RUNTIME_POLL_INTERVAL).await;
            if !self_update_enabled(&load_config(&app)) {
                continue;
            }
            match check_for_update(&app).await {
                Ok(Some(update)) => {
                    log::info!(
                        "[updater] runtime: {} available — surfacing",
                        update.version
                    );
                    let _ = app.emit_to(
                        MAIN_LABEL,
                        EVENT_AVAILABLE,
                        UpdateAvailability::from_update(&update),
                    );
                }
                Ok(None) => {}
                Err(e) => log::warn!("[updater] runtime check failed: {e}"),
            }
        }
    });
}

#[tauri::command]
pub async fn update_check(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<UpdateAvailability, String> {
    if window.label() != MAIN_LABEL {
        return Ok(UpdateAvailability::none());
    }
    match check_for_update(&app).await? {
        Some(update) => {
            let availability = UpdateAvailability::from_update(&update);
            let _ = app.emit_to(MAIN_LABEL, EVENT_AVAILABLE, availability.clone());
            Ok(availability)
        }
        None => Ok(UpdateAvailability::none()),
    }
}

#[tauri::command]
pub async fn update_apply_now(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    if window.label() != MAIN_LABEL {
        return Err("updates can only be applied from the main window".into());
    }
    let manager = app.state::<UpdateManager>();
    match check_for_update(&app).await? {
        Some(update) => apply(&app, &manager, update).await,
        None => Ok(()),
    }
}

pub(crate) async fn publish_version_report(app: &AppHandle, client: &async_nats::Client) {
    let Some(user_id) = tokens::load_tokens(app)
        .access_token
        .as_deref()
        .and_then(|token| tokens::jwt_claim_str(token, "userId"))
    else {
        log::debug!("[updater] no userId in token — skipping version report");
        return;
    };
    let version = app.package_info().version.to_string();
    let subject = format!("user.{user_id}.installed-agent");
    let payload = match serde_json::to_vec(&InstalledAgentReport {
        agent_type: AGENT_TYPE,
        version: version.clone(),
    }) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("[updater] failed to serialize version report: {e}");
            return;
        }
    };

    for attempt in 1..=REPORT_RETRIES {
        let result = match client
            .publish(subject.clone(), payload.clone().into())
            .await
        {
            Ok(()) => client.flush().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(()) => {
                log::info!("[updater] reported version {version} on {subject}");
                return;
            }
            Err(e) => {
                log::warn!(
                    "[updater] version report attempt {attempt}/{REPORT_RETRIES} failed: {e}"
                )
            }
        }
        if attempt < REPORT_RETRIES {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    log::warn!("[updater] version report gave up after {REPORT_RETRIES} attempts");
}
