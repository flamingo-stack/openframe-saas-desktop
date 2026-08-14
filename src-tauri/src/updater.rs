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
const EVENT_INSTALLING: &str = "update:installing";
const EVENT_ERROR: &str = "update:error";

/// Custom key in the updater manifest carrying the public release-notes page for
/// that version, stamped by the release workflow. Not part of tauri's manifest
/// schema — the plugin ignores fields it doesn't know and hands us the response
/// body verbatim as [`Update::raw_json`], so this rides along for free.
///
/// Absent on manifests published before the release page existed; the UI hides
/// the link rather than pointing at a 404.
const RELEASE_NOTES_URL_KEY: &str = "releaseNotesUrl";

/// A progress frame costs an IPC round trip, and reqwest yields chunks far
/// faster than a progress bar can be read — a 100MB download would emit
/// thousands. One frame per interval is more than the eye resolves.
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(200);

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
    release_notes_url: Option<String>,
}

impl UpdateAvailability {
    fn none() -> Self {
        Self {
            available: false,
            version: None,
            notes: None,
            release_notes_url: None,
        }
    }

    fn from_update(update: &Update) -> Self {
        Self {
            available: true,
            version: Some(update.version.clone()),
            notes: update.body.clone(),
            release_notes_url: update
                .raw_json
                .get(RELEASE_NOTES_URL_KEY)
                .and_then(serde_json::Value::as_str)
                .filter(|url| !url.is_empty())
                .map(str::to_string),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    /// Bytes downloaded SO FAR. The plugin's callback reports the length of the
    /// chunk it just read, not a running total — this accumulates it, because a
    /// bar fed the raw callback argument sits near zero for the whole download.
    downloaded: usize,
    /// `None` when the download response carried no `Content-Length`, which the
    /// UI renders as an indeterminate bar rather than a fraction of nothing.
    total: Option<u64>,
}

/// What went wrong, in the two terms the UI needs: a `kind` it can turn into
/// copy and an action, and the raw message for the log.
///
/// The kind is decided here rather than by matching on the message text in
/// TypeScript. `tauri_plugin_updater::Error`'s Display strings are not an
/// interface — they change between plugin releases, and half of them wrap an
/// upstream error whose text we don't control at all.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateError {
    kind: &'static str,
    message: String,
}

impl UpdateError {
    /// Another apply is already running — the previous call owns the restart, so
    /// this one has nothing to report and nothing to retry.
    fn busy() -> Self {
        Self {
            kind: "busy",
            message: "an update is already being applied".into(),
        }
    }

    /// The update went away between being offered and being accepted: the
    /// silent startup path took it, or the manifest no longer lists it.
    fn gone() -> Self {
        Self {
            kind: "gone",
            message: "no update is available anymore".into(),
        }
    }

    /// Our own precondition failed, not the updater's — a bad manifest URL in
    /// config, a call from the wrong window. Nothing the user can retry.
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: "unavailable",
            message: message.into(),
        }
    }

    fn from_plugin(error: tauri_plugin_updater::Error) -> Self {
        Self {
            kind: classify(&error),
            message: error.to_string(),
        }
    }
}

/// Coarse buckets, chosen so each one maps to a DIFFERENT thing to tell the
/// user: retry, free up disk, or give up and reinstall. Variants that are
/// platform-gated or that no bucket would change the advice for are left to the
/// catch-all, which the UI words as a plain retry.
fn classify(error: &tauri_plugin_updater::Error) -> &'static str {
    use tauri_plugin_updater::Error as E;
    match error {
        E::Reqwest(_) | E::Network(_) | E::ReleaseNotFound => "network",
        E::Minisign(_) | E::Base64(_) | E::SignatureUtf8(_) => "signature",
        E::Io(_) | E::TempDirNotFound | E::FailedToDetermineExtractPath => "io",
        // Nothing the user did and nothing a retry fixes: this build has no
        // artifact for their platform, or the manifest we published is wrong.
        E::UnsupportedArch
        | E::UnsupportedOs
        | E::TargetNotFound(_)
        | E::TargetsNotFound(_)
        | E::EmptyEndpoints
        | E::UrlParse(_)
        | E::InsecureTransportProtocol
        | E::InvalidUpdaterFormat
        | E::Semver(_)
        | E::Serialization(_) => "unavailable",
        _ => "unknown",
    }
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

async fn check_for_update(app: &AppHandle) -> Result<Option<Update>, UpdateError> {
    let manifest_url = resolve_manifest_url(app);
    let endpoint = url::Url::parse(&manifest_url).map_err(|e| {
        UpdateError::unavailable(format!("invalid manifest URL '{manifest_url}': {e}"))
    })?;

    let updater = app
        .updater_builder()
        .timeout(CHECK_TIMEOUT)
        .endpoints(vec![endpoint])
        .map_err(UpdateError::from_plugin)?
        .build()
        .map_err(UpdateError::from_plugin)?;

    updater.check().await.map_err(UpdateError::from_plugin)
}

async fn apply(
    app: &AppHandle,
    manager: &UpdateManager,
    mut update: Update,
) -> Result<(), UpdateError> {
    let _guard = match manager.apply_lock.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            log::info!("[updater] apply already in progress — ignoring");
            // Told to the caller rather than swallowed as success: an `Ok` here
            // promises a restart that the OTHER apply owns, and the window that
            // asked would sit on a progress bar fed by a download it never
            // started, forever.
            return Err(UpdateError::busy());
        }
    };

    update.timeout = Some(DOWNLOAD_TIMEOUT);
    log::info!("[updater] applying update to {}", update.version);

    let progress_app = app.clone();
    let mut downloaded = 0usize;
    let mut last_emit: Option<std::time::Instant> = None;

    let installing_app = app.clone();
    let result = update
        .download_and_install(
            move |chunk_len, total| {
                downloaded += chunk_len;
                // Throttled on the way OUT, not on the way in: every chunk is
                // counted, only the reporting is rationed.
                let now = std::time::Instant::now();
                if last_emit.is_some_and(|last| now.duration_since(last) < PROGRESS_EMIT_INTERVAL) {
                    return;
                }
                last_emit = Some(now);
                let _ = progress_app.emit_to(
                    MAIN_LABEL,
                    EVENT_PROGRESS,
                    Progress { downloaded, total },
                );
            },
            move || {
                log::info!("[updater] download complete, installing");
                // The bar is throttled, so it never quite reaches the end — and
                // install is its own wait (on Windows, a whole NSIS run). Say
                // which one the user is in rather than leaving a bar stuck a
                // hair short of full.
                let _ = installing_app.emit_to(MAIN_LABEL, EVENT_INSTALLING, ());
            },
        )
        .await;

    if let Err(e) = result {
        let error = UpdateError::from_plugin(e);
        log::error!(
            "[updater] install failed ({}): {}",
            error.kind,
            error.message
        );
        let _ = app.emit_to(MAIN_LABEL, EVENT_ERROR, error.clone());
        return Err(error);
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
                log::error!(
                    "[updater] startup apply failed, continuing on current version: {}",
                    e.message
                );
            }
        }
        Ok(None) => log::info!("[updater] startup: up to date"),
        Err(e) => log::warn!("[updater] startup check failed: {}", e.message),
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
                Err(e) => log::warn!("[updater] runtime check failed: {}", e.message),
            }
        }
    });
}

#[tauri::command]
pub async fn update_check(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<UpdateAvailability, UpdateError> {
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
pub async fn update_apply_now(app: AppHandle, window: WebviewWindow) -> Result<(), UpdateError> {
    if window.label() != MAIN_LABEL {
        return Err(UpdateError::unavailable(
            "updates can only be applied from the main window",
        ));
    }
    let manager = app.state::<UpdateManager>();
    match check_for_update(&app).await? {
        Some(update) => apply(&app, &manager, update).await,
        // Re-checked rather than trusted, so the update can legitimately be gone
        // by now — the silent startup path took it, or it was pulled. Success
        // would leave the caller waiting on a restart that is never coming.
        None => Err(UpdateError::gone()),
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
