use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;

use crate::{autostart, load_config, tokens, AppConfig, MAIN_LABEL};

const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/flamingo-stack/openframe-saas-desktop/releases/latest/download/updater.json";
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const RUNTIME_POLL_INTERVAL: Duration = Duration::from_secs(45 * 60);

/// Reported over NATS on `user.<id>.installed-agent` and persisted server-side per
/// install (`InstalledAgentListener` → `addInstalledAgent`). Renamed with the app;
/// rows written by pre-rename builds stay filed under `openframe-desktop` and are
/// not migrated, so a query spanning both eras has to accept either.
const AGENT_TYPE: &str = "openframe-console";
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
    /// Set when this install cannot apply the update where it is, so the UI can
    /// say what to do instead of offering a button that is bound to fail.
    blocked: Option<UpdateError>,
}

impl UpdateAvailability {
    fn none() -> Self {
        Self {
            available: false,
            version: None,
            notes: None,
            release_notes_url: None,
            blocked: None,
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
            blocked: in_place_blocker(),
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
/// copy and an action, and the raw message — shown verbatim as the UI's detail
/// line and logged, never matched on.
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

    /// The running bundle cannot be replaced where it is — see
    /// [`in_place_blocker`]. Moving the app is the fix, not a retry.
    fn location(message: impl Into<String>) -> Self {
        Self {
            kind: "location",
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
/// user: retry, free up disk, get approval, move the app, or give up. Variants
/// that are platform-gated or that no bucket would change the advice for are
/// left to the catch-all, which the UI words as a plain retry.
fn classify(error: &tauri_plugin_updater::Error) -> &'static str {
    use tauri_plugin_updater::Error as E;
    match error {
        E::Reqwest(_) | E::Network(_) | E::ReleaseNotFound => "network",
        E::Minisign(_) | E::Base64(_) | E::SignatureUtf8(_) => "signature",
        E::Io(io) => classify_io(io),
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

/// Windows' codes for "security software took the file". std has no
/// `ErrorKind` for them, so they arrive as `Uncategorized`.
const ERROR_VIRUS_INFECTED: i32 = 225;
const ERROR_VIRUS_DELETED: i32 = 226;

/// The download is held in memory, so an IO error comes from the install: on
/// macOS unpacking into `$TMPDIR` and swapping the `.app` bundle (the plugin
/// asks for an administrator password when the swap is refused, and reports a
/// cancelled prompt as `PermissionDenied`); on Windows writing the installer
/// into `%TEMP%`. A full disk is the rarest of these, so only an actual
/// out-of-space error may say so.
fn classify_io(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind as K;
    if cfg!(target_os = "windows")
        && matches!(
            error.raw_os_error(),
            Some(ERROR_VIRUS_INFECTED | ERROR_VIRUS_DELETED)
        )
    {
        return "security";
    }
    match error.kind() {
        K::StorageFull | K::QuotaExceeded => "disk",
        // Most often security software; an unwritable %TEMP% reads the same.
        // An installer blocked at launch never gets here: the plugin ignores
        // ShellExecuteW's result and exits.
        K::PermissionDenied if cfg!(target_os = "windows") => "security",
        K::PermissionDenied => "permission",
        // The bundle lives where it can't be swapped in place: the mounted DMG,
        // Gatekeeper's read-only App Translocation copy, or another volume.
        K::ReadOnlyFilesystem | K::CrossesDevices if cfg!(target_os = "macos") => "location",
        _ => "unknown",
    }
}

/// Why this install cannot update itself where it runs, decided before anything
/// is downloaded. The plugin swaps the `.app` by renaming it, which a read-only
/// mount refuses (`EROFS` from a translocated copy, `EXDEV` from a disk image)
/// — after the whole download, and again at every launch, since the silent
/// startup pass would retry it each time. The paths are resolved the way the
/// plugin resolves the bundle it will swap.
fn in_place_blocker() -> Option<UpdateError> {
    let exe = tauri::utils::platform::current_exe().ok()?;
    if !crate::on_read_only_mount(&exe) {
        return None;
    }
    let bundle = tauri_plugin_updater::extract_path_from_executable(&exe).unwrap_or(exe);
    Some(UpdateError::location(format!(
        "{} is on a read-only volume",
        bundle.display()
    )))
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

    if let Some(error) = in_place_blocker() {
        log::warn!(
            "[updater] not applying {}: {}",
            update.version,
            error.message
        );
        return Err(error);
    }

    update.timeout = Some(DOWNLOAD_TIMEOUT);
    log::info!("[updater] applying update to {}", update.version);

    let progress_app = app.clone();
    let mut downloaded = 0usize;
    let mut last_emit: Option<std::time::Instant> = None;

    let installing_app = app.clone();
    // Two calls rather than `download_and_install`, which is exactly these two:
    // the log has to say which one failed.
    let result = match update
        .download(
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
                // Whether the restart should come back with a window is a
                // question about now, not about which path reached here: a
                // login-started session stays in the tray, but the user may
                // have opened one from the tray or a notification — including
                // during this download, which can run for minutes. Sampled here
                // because it is the last point that runs on every platform:
                // on Windows the install hands the argv to the installer and
                // exits the process itself.
                autostart::show_window_after_restart(
                    &installing_app,
                    installing_app.get_webview_window(MAIN_LABEL).is_some(),
                );
                // The bar is throttled, so it never quite reaches the end — and
                // install is its own wait (on Windows, a whole NSIS run). Say
                // which one the user is in rather than leaving a bar stuck a
                // hair short of full.
                let _ = installing_app.emit_to(MAIN_LABEL, EVENT_INSTALLING, ());
            },
        )
        .await
    {
        Ok(bytes) => update.install(bytes).map_err(|e| ("install", e)),
        Err(e) => Err(("download", e)),
    };

    if let Err((phase, e)) = result {
        // Reached only when the attempt failed and this process is still alive,
        // so no restart is coming and the request must not outlive the attempt.
        // A signature failure lands here too, as a download failure: the
        // signature is verified after the download-finish callback already
        // recorded one.
        autostart::show_window_after_restart(app, false);
        let io_detail = match &e {
            tauri_plugin_updater::Error::Io(io) => Some((io.kind(), io.raw_os_error())),
            _ => None,
        };
        let error = UpdateError::from_plugin(e);
        log::error!(
            "[updater] {phase} failed ({}): {}",
            error.kind,
            error.message
        );
        // The plugin's text is the OS message alone. The kind and code pin the
        // cause; the executable path shows a launch from the DMG (`/Volumes/…`)
        // or from Gatekeeper's translocated copy (`…/AppTranslocation/…`).
        if let Some((kind, os_error)) = io_detail {
            log::error!(
                "[updater] io error: kind={kind:?} os_error={os_error:?} exe={:?} temp_dir={:?}",
                tauri::utils::platform::current_exe().ok(),
                std::env::temp_dir(),
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn only_a_full_disk_is_reported_as_disk_space() {
        assert_eq!(classify_io(&Error::from(ErrorKind::StorageFull)), "disk");
        assert_eq!(classify_io(&Error::from(ErrorKind::QuotaExceeded)), "disk");
        assert_eq!(classify_io(&Error::from(ErrorKind::NotFound)), "unknown");
    }

    #[test]
    fn a_plugin_io_error_is_classified_by_its_kind() {
        let error = tauri_plugin_updater::Error::Io(Error::from(ErrorKind::StorageFull));
        assert_eq!(classify(&error), "disk");
    }

    #[test]
    fn a_missing_temp_dir_is_not_reported_as_disk_space() {
        assert_eq!(
            classify(&tauri_plugin_updater::Error::TempDirNotFound),
            "unknown"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_refused_admin_prompt_asks_for_approval() {
        // What the plugin returns when the administrator prompt is cancelled.
        let error = Error::new(
            ErrorKind::PermissionDenied,
            "Failed to move the new app into place",
        );
        assert_eq!(classify_io(&error), "permission");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_bundle_that_cannot_be_swapped_in_place_asks_to_move_the_app() {
        assert_eq!(
            classify_io(&Error::from_raw_os_error(libc::EROFS)),
            "location"
        );
        assert_eq!(
            classify_io(&Error::from_raw_os_error(libc::EXDEV)),
            "location"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn a_refused_installer_write_names_security_software() {
        assert_eq!(
            classify_io(&Error::from_raw_os_error(ERROR_VIRUS_INFECTED)),
            "security"
        );
        assert_eq!(
            classify_io(&Error::from_raw_os_error(ERROR_VIRUS_DELETED)),
            "security"
        );
        // ERROR_ACCESS_DENIED.
        assert_eq!(classify_io(&Error::from_raw_os_error(5)), "security");
    }
}
