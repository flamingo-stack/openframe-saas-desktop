// Shell-owned token lifecycle: custody (tokens.json), freshness (JWT exp),
// and refresh against the gateway BFF. Owning refresh in the shell keeps the
// session alive independent of the webview's idle state, and enforces a single
// refresher — refresh tokens rotate, so a webview refresher racing a shell
// refresher would invalidate each other's tokens. In shells that expose
// `refreshTokens`, the frontend's token-refresh-manager delegates here instead
// of calling /oauth/refresh itself.

use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{load_config, AppConfig, MAIN_LABEL};

/// Emitted to every bundle window whenever the stored tokens change from the
/// Rust side (refresh rotation or session death). Payload: NativeAuthTokens.
pub const TOKEN_UPDATE_EVENT: &str = "native-auth:token-update";

/// Refresh when the access token is within this margin of its `exp`.
const REFRESH_MARGIN_SECS: u64 = 60;
/// Background freshness poll. Polling (vs a scheduled timer) self-heals across
/// laptop sleep/wake, matching openframe-chat's token-watcher approach.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NativeAuthTokens {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

/// Managed state: serializes refreshes (single-flight).
#[derive(Default)]
pub struct TokenLifecycle {
    refresh_lock: tokio::sync::Mutex<()>,
}

fn tokens_path(app: &AppHandle) -> std::path::PathBuf {
    app.path()
        .app_config_dir()
        .expect("resolve app config dir")
        .join("tokens.json")
}

pub fn load_tokens(app: &AppHandle) -> NativeAuthTokens {
    std::fs::read_to_string(tokens_path(app))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_tokens(app: &AppHandle, tokens: &NativeAuthTokens) -> Result<(), String> {
    let data = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
    // write_atomic keeps it owner-only; OS-keychain storage is tracked in the docs.
    crate::write_atomic(&tokens_path(app), &data)
}

pub fn clear_tokens(app: &AppHandle) -> Result<(), String> {
    match std::fs::remove_file(tokens_path(app)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Decoded JWT payload — no signature check; the shell only reads scheduling
/// hints (`exp`) and identity claims (`userId`) from its own tokens.
fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_exp_secs(token: &str) -> Option<u64> {
    jwt_payload(token)?.get("exp")?.as_u64()
}

/// String claim from a JWT, e.g. `userId` (gateway access tokens carry the
/// user UUID there; `sub` is the email).
pub(crate) fn jwt_claim_str(token: &str, claim: &str) -> Option<String> {
    jwt_payload(token)?
        .get(claim)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn needs_refresh(tokens: &NativeAuthTokens) -> bool {
    if tokens.refresh_token.is_none() {
        return false;
    }
    match &tokens.access_token {
        None => true,
        Some(access) => match jwt_exp_secs(access) {
            // Unparseable exp: can't schedule — leave it to the 401 path.
            None => false,
            Some(exp) => now_secs() + REFRESH_MARGIN_SECS >= exp,
        },
    }
}

/// Where /oauth/refresh lives. Mirrors the frontend's resolution (shared auth
/// host first), falling back to the tenant host learned at login — the BFF
/// resolves the tenant from the token either way (refreshTokensByLookup).
fn refresh_base(cfg: &AppConfig) -> Option<String> {
    crate::shared_host(cfg).or_else(|| {
        cfg.learned_host
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn emit_token_update(app: &AppHandle, tokens: &NativeAuthTokens) {
    for (label, _window) in app.webview_windows() {
        if label == MAIN_LABEL || label.starts_with("child-") {
            let _ = app.emit_to(label.as_str(), TOKEN_UPDATE_EVENT, tokens.clone());
        }
    }
}

/// Current tokens, refreshed first when the access token is missing/expiring.
/// Refresh failures fall back to whatever is stored — callers recover via the
/// normal 401 path.
pub async fn ensure_fresh(app: &AppHandle) -> NativeAuthTokens {
    if needs_refresh(&load_tokens(app)) {
        if let Err(e) = refresh(app, false, None).await {
            log::warn!("[tokens] on-demand refresh failed: {e}");
        }
    }
    load_tokens(app)
}

/// Single-flight refresh.
///
/// `force` skips the freshness check — used when the gateway already rejected
/// the current access token (a 401 upstream), which `exp` can't predict.
/// `prev_access` dampens force stampedes: if the stored access token changed
/// while we waited for the lock, a parallel refresh already rotated it.
///
/// Outcome mapping (consumed by the frontend's refresh delegation):
/// - rotated → Ok(tokens)
/// - session over (BFF 401/403) → tokens cleared, Ok(empty)
/// - transient failure (network, 5xx, no headers) → Err, stored tokens kept
pub async fn refresh(
    app: &AppHandle,
    force: bool,
    prev_access: Option<String>,
) -> Result<NativeAuthTokens, String> {
    let lifecycle = app.state::<TokenLifecycle>();
    let _guard = lifecycle.refresh_lock.lock().await;

    let current = load_tokens(app);
    if force {
        if current.access_token != prev_access {
            return Ok(current);
        }
    } else if !needs_refresh(&current) {
        return Ok(current);
    }

    let Some(refresh_token) = current.refresh_token.clone() else {
        return Err("no refresh token stored".into());
    };
    let cfg = load_config(app);
    let base = refresh_base(&cfg).ok_or("no host configured for token refresh")?;

    // Same contract as the frontend's token-refresh-manager: POST with the
    // Refresh-Token header; rotated tokens ride back on response headers
    // (dev-ticket mode). tenantId is optional — the BFF resolves the tenant
    // from the token (refreshTokensByLookup).
    let client = reqwest::Client::builder()
        .timeout(REFRESH_HTTP_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("{base}/oauth/refresh"))
        .header("Refresh-Token", refresh_token)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        // Non-empty body, deliberately: reqwest/hyper omit Content-Length even
        // for an explicit zero-length body, and over HTTP/1.1 GCP's front end
        // rejects Content-Length-less POSTs with 411 before they reach the
        // gateway (verified empirically — `.body("")` still 411s). The BFF
        // takes no request body, so an empty JSON object is ignored. Browsers
        // always send Content-Length: 0, hence the webview never hit this.
        .body("{}")
        .send()
        .await
        .inspect_err(|e| log::warn!("[tokens] refresh request to {base} failed: {e}"))
        .map_err(|e| format!("refresh request failed: {e}"))?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        log::warn!("[tokens] refresh rejected ({status}) — session is over, clearing tokens");
        let cleared = NativeAuthTokens::default();
        let _ = clear_tokens(app);
        // The shell can notice the session died while the webview sits idle in
        // the tray, so it has to tear the notification plane down itself — the
        // webview's logout path may not run for hours, if ever.
        crate::notifications::end_session(app);
        emit_token_update(app, &cleared);
        return Ok(cleared);
    }
    if !status.is_success() {
        log::warn!("[tokens] refresh against {base} failed: HTTP {status}");
        return Err(format!("refresh failed: HTTP {status}"));
    }

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let access = header("Access-Token");
    let refreshed = header("Refresh-Token");
    if access.is_none() && refreshed.is_none() {
        log::warn!("[tokens] refresh against {base} returned no token headers — is dev-ticket enabled there?");
        return Err("refresh returned no tokens — is dev-ticket enabled on the gateway?".into());
    }

    let mut merged = current;
    if access.is_some() {
        merged.access_token = access;
    }
    if refreshed.is_some() {
        merged.refresh_token = refreshed;
    }
    save_tokens(app, &merged)?;
    log::info!(
        "[tokens] refreshed (access exp: {:?})",
        merged.access_token.as_deref().and_then(jwt_exp_secs)
    );
    emit_token_update(app, &merged);
    Ok(merged)
}

pub fn spawn_refresh_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if needs_refresh(&load_tokens(&app)) {
                if let Err(e) = refresh(&app, false, None).await {
                    log::warn!("[tokens] background refresh failed: {e}");
                }
            }
        }
    });
}
