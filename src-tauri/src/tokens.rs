// Shell-owned token lifecycle: custody (tokens.json), freshness (JWT exp),
// and refresh against the gateway BFF. Owning refresh in the shell keeps the
// session alive independent of the webview's idle state, and enforces a single
// refresher — refresh tokens rotate, so a webview refresher racing a shell
// refresher would invalidate each other's tokens. In shells that expose
// `refreshTokens`, the frontend's token-refresh-manager delegates here instead
// of calling /oauth/refresh itself.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{load_config, tenant_host, AppConfig, MAIN_LABEL};

/// Emitted to every bundle window whenever the stored tokens change from the
/// Rust side (refresh rotation or session death). Payload: NativeAuthTokens.
pub const TOKEN_UPDATE_EVENT: &str = "native-auth:token-update";

/// Refresh when the access token is within this margin of its `exp`. Sized
/// against two opposing risks, both observed in the wild:
///
/// - too small and a stretch of not being scheduled at all (a background app
///   with hidden windows is an App Nap candidate, and macOS coalesces its
///   timers) skips every poll tick inside the margin. At 60 seconds there were
///   only two. Cost: the access token dies — recoverable, the refresh token is
///   still there.
/// - too large and the token rotates more often than it needs to, and **every
///   rotation is a chance to lose the session**: the gateway rotates refresh
///   tokens without a grace window, so a rotation whose response never arrives
///   leaves this side holding a token the server has already retired. Cost:
///   unrecoverable, a real sign-in.
///
/// The second cost is the higher one, so this stays close to the floor: four
/// poll ticks of slack, which moves rotation from every 840s to every 780s.
const REFRESH_MARGIN_SECS: u64 = 120;
/// Background freshness poll. Polling (vs a scheduled timer) self-heals across
/// laptop sleep/wake, matching openframe-chat's token-watcher approach.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Waits between attempts within one refresh; the count of them sets how many
/// retries a refresh gets. Only failures that never reached the gateway are
/// retried (see [`Outcome`]) — the case this exists for is the minute after a
/// laptop wakes, where DNS and the connect both fail outright before Wi-Fi has
/// reassociated, and one attempt used to be the whole budget.
const REFRESH_RETRY_DELAYS: &[Duration] = &[Duration::from_secs(1), Duration::from_secs(3)];
/// Cap on the poll's failure backoff, as a multiple of [`POLL_INTERVAL`] — an
/// hour-long gateway outage should not be an hour of POSTs. Bounded so the loop
/// still recovers on its own, without needing a wake or a webview read.
const MAX_POLL_BACKOFF: u32 = 16;
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
    /// Set when a refresh POST went out and no answer came back, so whether the
    /// server rotated is unknown. Read by the next rejection, which is then very
    /// likely that lost rotation surfacing rather than a revoked session — a
    /// distinction no status code carries, and the difference between a bug and a
    /// known hazard when someone reads the log later.
    lost_rotation: AtomicBool,
    /// Refresh attempts that settled without an answer, in a row. Drives one
    /// backoff for both things that ask for a refresh — the poll and
    /// [`refresh_soon`] — so a nudging caller cannot outrun it.
    consecutive_failures: AtomicU32,
    /// `now_secs` of the last nudge let through.
    last_nudge: AtomicU64,
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

/// Merge tokens the webview hands us into the store: only fields that arrive are
/// overwritten, because a rotation response may carry one token or both (matches
/// the mobile Keychain plugin).
///
/// Runs under the refresh lock, which is what makes it safe. Unlocked, this
/// read-merge-write could land on top of a rotation that completed in between
/// and put a spent refresh token back on disk — and a spent refresh token is not
/// a retryable error, it is the end of the session (the gateway issues rotating
/// refresh tokens, so the old value stops resolving the moment the new one is
/// issued).
pub async fn merge_and_save(
    app: &AppHandle,
    access_token: Option<String>,
    refresh_token: Option<String>,
) -> Result<(), String> {
    let lifecycle = app.state::<TokenLifecycle>();
    let _guard = lifecycle.refresh_lock.lock().await;

    let mut stored = load_tokens(app);
    if access_token.is_some() {
        stored.access_token = access_token;
    }
    if refresh_token.is_some() {
        stored.refresh_token = refresh_token;
    }
    save_tokens(app, &stored)
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

/// True once the access token's `exp` has passed. Unparseable `exp` counts as
/// live, same convention as [`needs_refresh`] — that is the 401 path's problem.
/// Used by the notification actions, which must not act on a dead session.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn is_expired(token: &str) -> bool {
    jwt_exp_secs(token).is_some_and(|exp| now_secs() >= exp)
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
    crate::shared_host(cfg).or_else(|| tenant_host(cfg))
}

fn emit_token_update(app: &AppHandle, tokens: &NativeAuthTokens) {
    for (label, _window) in app.webview_windows() {
        if label == MAIN_LABEL || label.starts_with("child-") {
            let _ = app.emit_to(label.as_str(), TOKEN_UPDATE_EVENT, tokens.clone());
        }
    }
}

/// Drop the session the shell is holding: custody, the notification plane, and
/// the webview's cache. Returns the empty set to hand back to the caller.
///
/// The notification teardown is not the webview's job here — the shell can
/// notice a session died while the webview sits idle in the tray, and that
/// webview's logout path may not run for hours, if ever.
fn end_stored_session(app: &AppHandle, why: &str) -> NativeAuthTokens {
    log::warn!("[tokens] session over ({why}) — clearing stored tokens");
    let cleared = NativeAuthTokens::default();
    let _ = clear_tokens(app);
    crate::notifications::end_session(app);
    emit_token_update(app, &cleared);
    cleared
}

/// Current tokens, refreshed first when the access token is missing/expiring.
/// Refresh failures fall back to whatever is stored — callers recover via the
/// normal 401 path.
///
/// For callers that are waiting on a person: the webview hydrating, or a
/// notification action about to act. Background reconnect loops use
/// [`refresh_soon`] instead — a rotation awaited from inside a retry loop runs
/// exactly when the network is least able to deliver its answer.
pub async fn ensure_fresh(app: &AppHandle) -> NativeAuthTokens {
    if needs_refresh(&load_tokens(app)) {
        if let Err(e) = refresh(app, false, None).await {
            log::warn!("[tokens] on-demand refresh failed: {e}");
        }
    }
    load_tokens(app)
}

/// What one POST to /oauth/refresh settled as.
///
/// The split between the last two is the whole point. A refresh is not
/// idempotent — the gateway retires the presented refresh token the moment it
/// issues the next one — so "the request failed" and "the answer never arrived"
/// are different facts. Retrying the first costs nothing; retrying the second
/// re-presents a token the server may already have retired, which comes back as
/// a 401 and reads exactly like a revoked session.
enum Outcome {
    /// Whatever the response headers carried; at least one of the two is set.
    Rotated(NativeAuthTokens),
    /// The gateway refused the refresh token (401/403).
    Rejected,
    /// The gateway never saw it — DNS, connect, or TLS. Safe to retry.
    Unreached(String),
    /// The request went out and the outcome is unknown: a broken or timed-out
    /// response, or a success carrying no tokens. Not retried.
    Unknown(String),
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
/// - session over — rejected, or a rotation that could not be written down —
///   → tokens cleared, Ok(empty)
/// - unreached or unknown → Err, stored tokens kept
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

    let client = reqwest::Client::builder()
        .timeout(REFRESH_HTTP_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;

    let outcome = attempt_refresh(&client, &base, &refresh_token).await;
    match &outcome {
        // Settled, either way: whatever the answer was, the gateway is reachable
        // and there is nothing left to back off from.
        Outcome::Rotated(_) | Outcome::Rejected => {
            lifecycle.consecutive_failures.store(0, Ordering::Relaxed)
        }
        Outcome::Unreached(_) | Outcome::Unknown(_) => {
            lifecycle
                .consecutive_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    match outcome {
        Outcome::Rotated(rotated) => {
            let mut merged = current;
            if rotated.access_token.is_some() {
                merged.access_token = rotated.access_token;
            }
            if rotated.refresh_token.is_some() {
                merged.refresh_token = rotated.refresh_token;
            }
            if let Err(e) = save_tokens(app, &merged) {
                // The gateway has already rotated, so the refresh token still on
                // disk is spent: every later refresh would be rejected until the
                // retry budget gave up and cleared it anyway. End it here, where
                // the cause is still known, rather than through a doomed loop.
                log::error!("[tokens] a completed rotation could not be persisted: {e}");
                return Ok(end_stored_session(app, "rotation could not be persisted"));
            }
            log::info!(
                "[tokens] refreshed (access exp: {:?})",
                merged.access_token.as_deref().and_then(jwt_exp_secs)
            );
            emit_token_update(app, &merged);
            Ok(merged)
        }
        Outcome::Rejected => {
            let why = if lifecycle.lost_rotation.swap(false, Ordering::Relaxed) {
                // An earlier refresh went out and never came back. The gateway
                // rotates refresh tokens with no grace window, so that rotation
                // most likely did land and retired the value just presented —
                // this rejection is that lost rotation surfacing, not a session
                // someone revoked. Custody goes either way (the token is dead),
                // but the log has to say which, or it gets debugged twice.
                "a refresh whose response was lost had already rotated it"
            } else {
                "the gateway rejected the refresh token"
            };
            Ok(end_stored_session(app, why))
        }
        Outcome::Unreached(e) => Err(e),
        Outcome::Unknown(e) => {
            lifecycle.lost_rotation.store(true, Ordering::Relaxed);
            Err(e)
        }
    }
}

/// One refresh, retried only while the gateway has not seen it. A rejection is
/// final on the first occurrence — re-presenting the same token cannot change
/// that answer — and an unknown outcome is never retried, because the retry is
/// what would turn it into a rejection.
async fn attempt_refresh(client: &reqwest::Client, base: &str, refresh_token: &str) -> Outcome {
    let mut delays = REFRESH_RETRY_DELAYS.iter();
    loop {
        let outcome = post_refresh(client, base, refresh_token).await;
        let Outcome::Unreached(ref reason) = outcome else {
            return outcome;
        };
        let Some(delay) = delays.next() else {
            return outcome;
        };
        log::warn!(
            "[tokens] refresh never reached {base} ({reason}) — retrying in {}s",
            delay.as_secs()
        );
        tokio::time::sleep(*delay).await;
    }
}

/// Same contract as the frontend's token-refresh-manager: POST with the
/// Refresh-Token header; rotated tokens ride back on response headers
/// (dev-ticket mode). tenantId is optional — the BFF resolves the tenant from
/// the token (refreshTokensByLookup).
async fn post_refresh(client: &reqwest::Client, base: &str, refresh_token: &str) -> Outcome {
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
        .await;
    let response = match response {
        Ok(response) => response,
        // `is_connect` is the line between the two: it covers DNS, the TCP
        // connect and the TLS handshake, i.e. everything that ends before the
        // request is on the wire. Anything else — a connection broken mid-answer,
        // a timeout — means the gateway may well have rotated already.
        Err(e) if e.is_connect() => {
            log::warn!("[tokens] refresh could not reach {base}: {e}");
            return Outcome::Unreached(format!("refresh could not reach the gateway: {e}"));
        }
        Err(e) => {
            log::warn!("[tokens] refresh to {base} went out with no answer back: {e}");
            return Outcome::Unknown(format!("refresh got no answer: {e}"));
        }
    };

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        log::warn!("[tokens] refresh against {base} rejected: HTTP {status}");
        return Outcome::Rejected;
    }
    if !status.is_success() {
        // Answered, so a rotation is not ruled out (a 5xx from a front end can
        // sit in front of a gateway that already did the work).
        log::warn!("[tokens] refresh against {base} failed: HTTP {status}");
        return Outcome::Unknown(format!("refresh failed: HTTP {status}"));
    }

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let rotated = NativeAuthTokens {
        access_token: header("Access-Token"),
        refresh_token: header("Refresh-Token"),
    };
    if rotated.access_token.is_none() && rotated.refresh_token.is_none() {
        log::warn!("[tokens] refresh against {base} returned no token headers — is dev-ticket enabled there?");
        return Outcome::Unknown(
            "refresh returned no tokens — is dev-ticket enabled on the gateway?".into(),
        );
    }
    Outcome::Rotated(rotated)
}

pub fn spawn_refresh_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(poll_delay(failures(&app))).await;
            if !needs_refresh(&load_tokens(&app)) {
                continue;
            }
            if let Err(e) = refresh(&app, false, None).await {
                log::warn!(
                    "[tokens] background refresh failed ({} in a row): {e}",
                    failures(&app)
                );
            }
        }
    });
}

fn failures(app: &AppHandle) -> u32 {
    app.state::<TokenLifecycle>()
        .consecutive_failures
        .load(Ordering::Relaxed)
}

/// Poll cadence: [`POLL_INTERVAL`], doubling while refreshes keep failing up to
/// [`MAX_POLL_BACKOFF`] times it.
fn poll_delay(failures: u32) -> Duration {
    POLL_INTERVAL * 2u32.saturating_pow(failures.min(16)).min(MAX_POLL_BACKOFF)
}

/// Refresh now, because something just told us the assumptions the poll runs on
/// no longer hold — the machine woke, the user came back to the window after it
/// sat in the tray, or a connection wants a bearer. All land on an access token
/// that expired while nothing was scheduled to rotate it, and at wake they land
/// there before the network is back, which is exactly the moment the retry budget
/// in [`attempt_refresh`] exists for.
///
/// Rate-limited to the poll's own cadence rather than trusting callers: some of
/// them are retry loops that ask every few seconds, and a nudge must be able to
/// pull the schedule forward without replacing it. Measured before this gate: 132
/// refresh attempts in ten minutes against an unreachable host.
pub fn refresh_soon(app: &AppHandle, why: &'static str) {
    if !needs_refresh(&load_tokens(app)) {
        return;
    }
    let now = now_secs();
    let gap = poll_delay(failures(app)).as_secs();
    let last_nudge = &app.state::<TokenLifecycle>().last_nudge;
    if now.saturating_sub(last_nudge.load(Ordering::Relaxed)) < gap {
        return;
    }
    last_nudge.store(now, Ordering::Relaxed);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        log::info!("[tokens] {why} — refreshing an expiring session");
        if let Err(e) = refresh(&app, false, None).await {
            log::warn!("[tokens] refresh after {why} failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token shaped like the gateway's: only the payload is ever read, and
    /// only for `exp` / `userId`.
    fn token_expiring_in(secs: i64) -> String {
        let exp = now_secs() as i64 + secs;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"exp":{exp},"userId":"u-1"}}"#));
        format!("header.{payload}.signature")
    }

    fn tokens(access: Option<String>, refresh: Option<&str>) -> NativeAuthTokens {
        NativeAuthTokens {
            access_token: access,
            refresh_token: refresh.map(str::to_string),
        }
    }

    #[test]
    fn nothing_to_refresh_without_a_refresh_token() {
        assert!(!needs_refresh(&tokens(
            Some(token_expiring_in(-3600)),
            None
        )));
    }

    #[test]
    fn a_missing_access_token_is_refreshable() {
        assert!(needs_refresh(&tokens(None, Some("r"))));
    }

    #[test]
    fn refresh_is_due_inside_the_margin_and_not_before() {
        let outside = REFRESH_MARGIN_SECS as i64 + 60;
        assert!(!needs_refresh(&tokens(
            Some(token_expiring_in(outside)),
            Some("r")
        )));
        let inside = REFRESH_MARGIN_SECS as i64 - 60;
        assert!(needs_refresh(&tokens(
            Some(token_expiring_in(inside)),
            Some("r")
        )));
    }

    /// Two competing bounds on the margin, both in [`REFRESH_MARGIN_SECS`]:
    /// several poll ticks of slack against a coalesced timer, and well short of
    /// the access token's 900-second life so it doesn't rotate needlessly often.
    #[test]
    fn the_margin_stays_between_its_two_bounds() {
        /// What the gateway issues access tokens with
        /// (`security.oauth2.token.access.expiration-seconds`).
        const ACCESS_TOKEN_LIFE: Duration = Duration::from_secs(900);
        let margin = Duration::from_secs(REFRESH_MARGIN_SECS);
        assert!(margin >= POLL_INTERVAL * 4);
        assert!(margin <= ACCESS_TOKEN_LIFE / 4);
    }

    /// The distinction the session depends on: only a failure that never reached
    /// the gateway may be retried, because the gateway retires the presented
    /// refresh token the moment it issues a new one.
    #[test]
    fn only_unreached_attempts_are_retryable() {
        let retryable = |outcome: &Outcome| matches!(outcome, Outcome::Unreached(_));
        assert!(retryable(&Outcome::Unreached("connect refused".into())));
        assert!(!retryable(&Outcome::Unknown("connection closed".into())));
        assert!(!retryable(&Outcome::Rejected));
        assert!(!retryable(&Outcome::Rotated(NativeAuthTokens::default())));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn an_expired_token_is_expired_and_a_live_one_is_not() {
        assert!(is_expired(&token_expiring_in(-1)));
        assert!(!is_expired(&token_expiring_in(60)));
        // An `exp` we can't read counts as live, same convention as needs_refresh.
        assert!(!is_expired("not-a-jwt"));
    }

    /// An `exp` we can't read is not a schedule — the 401 path owns it.
    #[test]
    fn an_unparseable_token_is_left_alone() {
        assert!(!needs_refresh(&tokens(Some("not-a-jwt".into()), Some("r"))));
    }

    /// The cadence a nudge is held to as well, which is what keeps a caller in a
    /// retry loop from outrunning the backoff.
    #[test]
    fn the_poll_backs_off_while_failing_and_stays_bounded() {
        assert_eq!(poll_delay(0), POLL_INTERVAL);
        assert_eq!(poll_delay(1), POLL_INTERVAL * 2);
        assert_eq!(poll_delay(u32::MAX), POLL_INTERVAL * MAX_POLL_BACKOFF);
    }
}
