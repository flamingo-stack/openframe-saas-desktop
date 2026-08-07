// Shell-owned token lifecycle: custody (tokens.json), freshness (JWT exp),
// and refresh against the gateway BFF. Owning refresh in the shell keeps the
// session alive independent of the webview's idle state, and enforces a single
// refresher — refresh tokens rotate, so a webview refresher racing a shell
// refresher would invalidate each other's tokens. In shells that expose
// `refreshTokens`, the frontend's token-refresh-manager delegates here instead
// of calling /oauth/refresh itself.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use futures::FutureExt;
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
/// Background freshness poll.
///
/// Note what this does *not* do: self-heal across sleep. `tokio::time::sleep`
/// runs on `Instant`, which on macOS is `CLOCK_UPTIME_RAW` and stops while the
/// machine is out — so on a laptop cycling through maintenance DarkWake (see
/// [`LONGEST_DARKWAKE_SECS`]) one tick of this can take over an hour of wall
/// clock, and [`poll_delay`] multiplies that. Freshness is still decided on the
/// wall clock in [`needs_refresh`], so a late tick refreshes correctly; it is
/// only the cadence that stretches. Anything that must happen while the machine
/// sleeps cannot be hung off this loop.
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
/// Total budget for one refresh attempt, enforced by
/// [`under_wall_clock_deadline`] rather than by reqwest — the client's own timer
/// is monotonic and so is stopped by the sleep that makes a refresh go wrong.
const REFRESH_WALL_CLOCK_BUDGET: Duration = Duration::from_secs(10);
/// Bound on the connect alone, so a TCP connect left hanging by a half-up
/// network resolves as [`Outcome::Unreached`] — which is retryable — instead of
/// running out the total budget and being called [`Outcome::Unknown`], which is
/// not, and which needlessly puts the refresh token in doubt.
const REFRESH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the machine must have been continuously awake before a rotation may
/// go out.
///
/// This is the guard the session was lost for the want of. A laptop idling
/// overnight on battery is not "asleep": it cycles through maintenance DarkWake
/// (see [`LONGEST_DARKWAKE_SECS`]). The access token dies inside one of those
/// sleeps, so at the next resume something immediately wants a bearer, and the
/// rotation POST goes out in a window that ends before the answer can come back,
/// with Wi-Fi still reassociating. That is unrecoverable: the gateway retires the
/// presented refresh token as it issues the next one, so a rotation whose
/// response is lost costs the session.
/// Observed 2026-08-01: POST at 01:33:30 UTC, machine re-entered sleep the same
/// second, 401 at 02:05:20, session cleared.
///
/// Sized above [`LONGEST_DARKWAKE_SECS`] so no maintenance wake can clear it,
/// and small enough that a real wake — which is followed by minutes of use —
/// pays it once and never notices. Refresh tokens are good for a week
/// (`security.oauth2.token.refresh.expiration-seconds: 604800`), so declining to
/// rotate through a night of DarkWake costs nothing.
const WAKE_SETTLE_SECS: u64 = 15;
/// Longest maintenance DarkWake window measured against `pmset -g log` on the
/// night of 2026-08-01: they ran 2-9 seconds, roughly every 15 minutes.
const LONGEST_DARKWAKE_SECS: u64 = 9;
/// Wall-clock budget a caller spends waiting for the machine to settle before
/// giving up and failing the refresh — a few settle windows' worth. Bounded, and
/// bounded on the wall clock, because in a night of DarkWake the machine never
/// settles and the waits themselves are monotonic: an unbounded wait would park
/// one task per resume until morning.
const SETTLE_WAIT_BUDGET_SECS: u64 = 3 * WAKE_SETTLE_SECS;
/// How often [`spawn_wake_watch`] samples the two clocks. A timer this often for
/// the life of a tray app is a real if small cost, and it buys the one thing the
/// gate depends on: a mark fresh enough that a caller arriving seconds after a
/// resume reads it rather than the one before it. Coarsening the tick widens
/// exactly that gap.
const WAKE_TICK: Duration = Duration::from_secs(1);
/// How often [`under_wall_clock_deadline`] re-checks the wall clock.
const DEADLINE_TICK: Duration = Duration::from_secs(1);
/// Divergence between the wall and monotonic clocks across one [`WAKE_TICK`]
/// that counts as the machine having slept.
///
/// A late tick is not the hazard — coalescing advances both clocks alike and so
/// shows no divergence at all. Truncation is: the two are read one after the
/// other and each floors to a whole second, so a pair can differ by two with the
/// machine fully awake. Everything above that is real suspend.
const SLEEP_DETECT_SECS: u64 = 3;
/// After a rotation whose answer was lost, how long before the refresh token may
/// be presented again.
///
/// Without this the single-flight lock serializes callers but does not coalesce
/// them: the losing caller finds the tokens unchanged, `needs_refresh` still
/// true, and POSTs the same refresh token the moment the lock frees — which is
/// how 02:05:20 turned an ambiguous outcome into a cleared session inside one
/// second. Waiting cannot save a rotation that really did land, but it is the
/// difference between "we asked again once the network was back" and "we
/// re-presented it 200ms later on the same dead link", and it gives the case
/// where the request died before reaching the auth service a chance to recover.
const LOST_ROTATION_COOLDOWN_SECS: u64 = 60;
/// Why a refresh was refused without a request going out. Shared by the settle
/// gates so a post-mortem sees one reason, not a spelling of it per site.
const UNSETTLED: &str = "the machine has not been awake long enough to rotate";

/// The two bounds the wake gate is only useful between: the settle window has to
/// outlast every maintenance DarkWake or a rotation still goes out inside one,
/// and a suspend has to look longer than a tick plus the two clocks' truncation
/// error, or an ordinary coalesced timer reads as sleep.
const _: () = {
    assert!(WAKE_SETTLE_SECS > LONGEST_DARKWAKE_SECS);
    assert!(SLEEP_DETECT_SECS >= WAKE_TICK.as_secs() + 2);
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NativeAuthTokens {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

impl NativeAuthTokens {
    /// Take only the fields that arrived. A rotation response may carry one
    /// token or both (matching the mobile Keychain plugin), and an arrival
    /// carrying one must not blank the other. Whether an arrival also settles a
    /// rotation in doubt is a separate question, answered at each call site —
    /// the two differ, and merging is the only part they share.
    fn merge_from(&mut self, incoming: NativeAuthTokens) {
        if incoming.access_token.is_some() {
            self.access_token = incoming.access_token;
        }
        if incoming.refresh_token.is_some() {
            self.refresh_token = incoming.refresh_token;
        }
    }
}

/// Managed state: serializes refreshes (single-flight).
#[derive(Default)]
pub struct TokenLifecycle {
    refresh_lock: tokio::sync::Mutex<()>,
    /// `now_secs` of the last refresh POST that went out and got no answer back,
    /// so whether the server rotated is unknown; zero once nothing is in doubt.
    ///
    /// Gates how soon the token may be presented again, and tells the next
    /// rejection that it is very likely that lost rotation surfacing rather than
    /// a revoked session — a distinction no status code carries, and the
    /// difference between a bug and a known hazard when someone reads the log
    /// later. One field rather than a flag beside a timestamp, because the two
    /// can then never disagree about whether anything is in doubt.
    lost_rotation_at: AtomicU64,
    /// Refresh attempts that settled without an answer, in a row. Drives one
    /// backoff for both things that ask for a refresh — the poll and
    /// [`refresh_soon`] — so a nudging caller cannot outrun it.
    consecutive_failures: AtomicU32,
    /// `now_secs` of the last nudge let through.
    last_nudge: AtomicU64,
    /// Bumped every time custody is dropped, so a rotation that was in flight
    /// at the time can tell that the answer it is holding is no longer wanted.
    ///
    /// A lock would be the obvious guard and is the wrong one here: both
    /// sign-out paths are synchronous (a Tauri command and a tray callback),
    /// and the refresh lock is held across the whole POST — so taking it would
    /// mean either restructuring both callers or leaving a person who clicked
    /// Sign Out watching a dead menu for the length of a network timeout.
    custody_epoch: AtomicU64,
    /// [`mono_secs`] when the machine was last seen to resume from sleep,
    /// maintained by [`spawn_wake_watch`] and by the full-wake observer.
    ///
    /// Monotonic, not wall clock, and that is the whole safety property — see
    /// [`settle_left_secs`]. Zero means no resume has been recorded yet, handled
    /// there as its own case, so a watcher that is not running gates nothing.
    awake_since: AtomicU64,
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
    // Read before the wait, not after. `refresh` holds this lock across a whole
    // POST, so a sign-out can land entirely inside the wait — and writing these
    // tokens afterwards would undo it, then hand the signed-out user's
    // notification subject straight back on the resubscribe that follows. A
    // sign-in issued *after* that sign-out reads the bumped value here, so it is
    // untouched by this.
    let custody = lifecycle.custody_epoch.load(Ordering::Acquire);
    let _guard = lifecycle.refresh_lock.lock().await;
    if lifecycle.custody_epoch.load(Ordering::Acquire) != custody {
        log::warn!("[tokens] tokens arrived for a session that was signed out while they waited — dropping them");
        return Ok(());
    }

    let mut stored = load_tokens(app);
    let incoming = NativeAuthTokens {
        access_token,
        refresh_token,
    };
    // A refresh token this side has never presented settles any doubt exactly as
    // a rotation does. Without this, a sign-in within the cooldown of an earlier
    // session's lost rotation is held back and its first rejection is blamed on
    // that old loss. Conditional on the value actually changing, because the
    // webview re-saving the set it just read must not clear a doubt that stands.
    if incoming.refresh_token.is_some() && incoming.refresh_token != stored.refresh_token {
        lifecycle.lost_rotation_at.store(0, Ordering::Relaxed);
    }
    stored.merge_from(incoming);
    save_tokens(app, &stored)
}

pub fn clear_tokens(app: &AppHandle) -> Result<(), String> {
    let lifecycle = app.state::<TokenLifecycle>();
    // The doubt belonged to the token being deleted; whatever arrives next is
    // not the one that was in question.
    lifecycle.lost_rotation_at.store(0, Ordering::Relaxed);
    let removed = match std::fs::remove_file(tokens_path(app)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    };
    // After the delete, never before. A rotation already past its own snapshot
    // would otherwise see an unchanged epoch and write the file straight back.
    lifecycle.custody_epoch.fetch_add(1, Ordering::Release);
    removed
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

/// Seconds on the monotonic clock, which — unlike [`now_secs`] — does not
/// advance while the machine is asleep. The gap between the two is the whole
/// sleep signal; see [`spawn_wake_watch`].
fn mono_secs() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs()
}

/// Tracks when the machine last resumed from sleep, by watching the wall clock
/// run ahead of the monotonic one.
///
/// On macOS `Instant` is `CLOCK_UPTIME_RAW`, which stops during sleep, while
/// `SystemTime` keeps counting (measured on a developer machine: 398.7h since
/// boot against 140.2h of `CLOCK_UPTIME_RAW`). So a tick that finds more wall
/// time elapsed than monotonic time has just come back from a suspend, and the
/// size of the discrepancy is how long the machine was out.
///
/// This is the only sleep signal that covers maintenance DarkWake, and the only
/// one that exists on both platforms at all: `NSWorkspaceDidWakeNotification`
/// (macos_wake.rs) is posted for full wakes only — five times in six
/// days of logs, against roughly thirty DarkWake cycles in a single night — and
/// Windows has no wake observer in this shell.
///
/// The tick is itself a monotonic timer and so is stalled by the same sleep it
/// is looking for. That is what makes it work: it cannot run *during* sleep,
/// only immediately after, which is exactly when the mark needs updating.
pub fn spawn_wake_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let lifecycle = app.state::<TokenLifecycle>();
        // `awake_since` is deliberately left at its zero default rather than
        // stamped with the start time: a process that is starting is running on
        // a scheduled CPU, so there is nothing to settle, and stamping it would
        // make every launch defer its first rotation by the settle window —
        // straight into the webview's opening `native_auth_get_tokens`.
        let mut prev = (now_secs(), mono_secs());
        loop {
            tokio::time::sleep(WAKE_TICK).await;
            let now = (now_secs(), mono_secs());
            let slept = slept_between(prev, now);
            prev = now;
            if slept >= SLEEP_DETECT_SECS {
                let (_wall, mono) = now;
                lifecycle.awake_since.store(mono, Ordering::Relaxed);
                log::info!("[tokens] the machine resumed after {slept}s asleep");
            }
        }
    });
}

/// Record a resume the OS told us about directly, rather than waiting for
/// [`spawn_wake_watch`] to infer it from the clocks.
///
/// Without this the full-wake path is a race the gate loses: the observer fires
/// its nudge at resume+0, while the watch is a 1-second timer that is merely
/// *overdue* then, and a machine that was up for hours before the lid closed
/// carries a monotonic mark that reads as hours awake. Both gates would open and
/// the rotation would go out at the instant of resume — on the one path where a
/// person is waiting and the network is least ready.
#[cfg(target_os = "macos")]
pub(crate) fn mark_resume(app: &AppHandle) {
    app.state::<TokenLifecycle>()
        .awake_since
        .store(mono_secs(), Ordering::Relaxed);
}

/// Seconds the machine spent asleep between two `(wall, monotonic)` samples:
/// the wall clock counts sleep, the monotonic clock does not, so the difference
/// between how much each advanced is the suspend.
fn slept_between(prev: (u64, u64), now: (u64, u64)) -> u64 {
    now.0
        .saturating_sub(prev.0)
        .saturating_sub(now.1.saturating_sub(prev.1))
}

/// How much longer until the machine counts as settled after a resume, or None
/// if it already does.
///
/// Both arguments are [`mono_secs`], and that is what makes the gate fail safe.
/// The mark is only as fresh as [`spawn_wake_watch`]'s last tick, so a caller can
/// read one taken before the resume it cares about — and on the wall clock, which
/// counts sleep, that stale mark would read as however long the machine was out.
/// Hundreds of seconds "awake", gate open, POST into the DarkWake window this
/// exists to keep it out of. Monotonic elapsed is bounded by time actually spent
/// running, so the same stale mark still reads as a few seconds.
fn settle_left_secs(awake_since_mono: u64, mono_now: u64) -> Option<u64> {
    // Zero is "no resume has ever been recorded" — not "resumed at process
    // start", which the monotonic clock's own zero would otherwise mean and
    // which would gate every launch. Nothing to settle, so nothing is held; a
    // missing wake watch lands here too.
    if awake_since_mono == 0 {
        return None;
    }
    WAKE_SETTLE_SECS
        .checked_sub(mono_now.saturating_sub(awake_since_mono))
        .filter(|left| *left > 0)
}

/// How much of the lost-rotation cooldown is left, or None once it has run out —
/// which is also the answer for the zero mark that means nothing is in doubt.
///
/// On the wall clock, because unlike a resume the doubt is about a token rather
/// than about this process's uptime. The zero mark carries the opposite meaning
/// to the one in [`settle_left_secs`] — there it means "no resume recorded, so
/// gate nothing", here "nothing in doubt, so hold nothing back" — which is why
/// the two windows stay separate functions despite the shared arithmetic.
fn cooldown_left_secs(lost_at: u64, now: u64) -> Option<u64> {
    LOST_ROTATION_COOLDOWN_SECS
        .checked_sub(now.saturating_sub(lost_at))
        .filter(|left| *left > 0)
}

/// Waits for the machine to have been continuously awake for
/// [`WAKE_SETTLE_SECS`], so a rotation cannot go out into a DarkWake window that
/// closes before the answer arrives. Returns whether it got there.
///
/// Waits rather than refusing outright, because the caller that matters most — a
/// person who just opened the lid — must still get a rotation, only a few
/// seconds later and on a network that has finished reassociating. Held outside
/// the refresh lock so a settling caller does not stall sign-in, which takes the
/// same lock through [`merge_and_save`].
///
/// In a maintenance DarkWake it never settles — the process is descheduled
/// before the timer fires and the next resume restarts the count — so the wait
/// gives up on a **wall-clock** budget and reports failure. Wall clock because
/// the sleeps are monotonic and would otherwise stretch across a whole night,
/// parking one task per resume until morning: the accumulation the bound exists
/// to prevent.
///
/// The gate is only as prompt as [`spawn_wake_watch`], so a rotation asked for
/// in the first [`WAKE_TICK`] after a resume can still read a stale mark and go
/// out. That is the pre-existing behaviour rather than a new hole, and the
/// margin is comfortable in practice: the reconnect that asks for a bearer takes
/// seconds to notice the link died at all (measured 2026-08-01: DarkWake at
/// 01:33:25 UTC, the refresh it provoked at 01:33:30).
async fn wait_until_settled(app: &AppHandle) -> bool {
    let give_up_at = now_secs() + SETTLE_WAIT_BUDGET_SECS;
    loop {
        if settle_left(app).is_none() {
            return true;
        }
        if now_secs() >= give_up_at {
            return false;
        }
        // A tick at a time rather than the whole remainder: the sleep is
        // monotonic, so one long enough to cover the settle window would stretch
        // across a DarkWake cycle and blow the wall-clock budget by tens of
        // minutes. Short sleeps do complete inside a maintenance window.
        tokio::time::sleep(WAKE_TICK).await;
    }
}

fn awake_since(app: &AppHandle) -> u64 {
    app.state::<TokenLifecycle>()
        .awake_since
        .load(Ordering::Relaxed)
}

/// [`settle_left_secs`] against the live marks. The pure two-argument form stays
/// for the tests, which pass literals.
fn settle_left(app: &AppHandle) -> Option<u64> {
    settle_left_secs(awake_since(app), mono_secs())
}

/// Runs `request` under a deadline measured on the wall clock, returning `None`
/// if it blows.
///
/// reqwest's own timeout is a tokio timer, so it is stalled by exactly the sleep
/// that makes a refresh go wrong: on 2026-08-01 a POST with a 10-second budget
/// took 15 minutes to settle, holding the single-flight lock the whole time and
/// blocking every other refresher behind it. The one-second tick is stalled too,
/// which is fine — it fires the moment the process is scheduled again, and the
/// decision is then made from [`now_secs`].
async fn under_wall_clock_deadline<F, T>(budget: Duration, request: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    let deadline = now_secs() + budget.as_secs();
    tokio::pin!(request);
    loop {
        tokio::select! {
            // Biased, and the request first. At a resume both arms are ready at
            // once — the tick is overdue by however long the machine was out,
            // and the response may have landed just before the suspend — and an
            // unbiased select picks between them at random. Losing that coin
            // flip would discard a rotation that actually completed, which is
            // the one outcome that cannot be recovered from: the token on disk
            // is already retired, so the session is gone at the next attempt.
            biased;
            settled = &mut request => return Some(settled),
            _ = tokio::time::sleep(DEADLINE_TICK) => {
                if now_secs() >= deadline {
                    // hyper drives the connection on its own task, so at a
                    // resume this arm can win before that task has been
                    // scheduled even though the answer is already on the
                    // socket. Give it one turn before writing the rotation off.
                    tokio::task::yield_now().await;
                    return request.as_mut().now_or_never();
                }
            }
        }
    }
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
fn end_stored_session(app: &AppHandle, why: &str, trigger: &str) -> NativeAuthTokens {
    log::warn!(
        "[tokens] session over ({why}) on a refresh (trigger: {trigger}) — clearing stored tokens"
    );
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
        if let Err(e) = refresh(app, false, None, "an on-demand read").await {
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
/// - signed out while the rotation was on the wire → the rotation is dropped
///   unused, Ok(empty). The frontend acts on this the same way it acts on any
///   empty result, which is right: the session really is over.
/// - unreached or unknown → Err, stored tokens kept
/// - held back, because the machine has not been awake long enough or an
///   earlier rotation's fate is still unknown → Err, stored tokens kept, no
///   request made. Indistinguishable from a transient failure by design: every
///   caller already treats one as "keep the session and try later", which is
///   exactly right here.
///
/// `trigger` names what asked, for the log. Worth the parameter because the
/// rejection that ends a session is the one event with no other way to attribute
/// it — it resolves to `Ok`, so no caller's own error path reports it.
pub async fn refresh(
    app: &AppHandle,
    force: bool,
    prev_access: Option<String>,
    trigger: &str,
) -> Result<NativeAuthTokens, String> {
    // A lock-free look before paying the settle wait, which is otherwise pure
    // latency for a caller with nothing to rotate — a force refresh whose access
    // token already moved would wait out the whole budget only to be told so
    // below. The authoritative decision is still the one made under the lock.
    let stored = load_tokens(app);
    let wanted = if force {
        stored.access_token == prev_access
    } else {
        needs_refresh(&stored)
    };
    // Before the lock, so a settling caller does not stall sign-in behind it.
    if wanted && !wait_until_settled(app).await {
        log::warn!("[tokens] {UNSETTLED} — gave up waiting (trigger: {trigger})");
        return Err(UNSETTLED.into());
    }

    let lifecycle = app.state::<TokenLifecycle>();
    let _guard = lifecycle.refresh_lock.lock().await;

    // Read before the tokens, so a sign-out that lands between the two reads is
    // caught by the epoch rather than slipping past both.
    let custody = lifecycle.custody_epoch.load(Ordering::Acquire);
    let current = load_tokens(app);
    if force {
        if current.access_token != prev_access {
            return Ok(current);
        }
    } else if !needs_refresh(&current) {
        return Ok(current);
    }

    // Settled again, now that the lock is ours and a rotation is genuinely
    // wanted. Waiting for the lock can take as long as the holder's whole retry
    // budget, and that wait is a monotonic timer — so a suspend can land
    // entirely inside it and put this POST right back in the window the gate
    // above exists to keep it out of. Below the checks that short-circuit,
    // because a caller whose work the holder already did wants those tokens
    // rather than an error.
    if let Some(left) = settle_left(app) {
        log::warn!("[tokens] {UNSETTLED} — {left}s of the settle window left (trigger: {trigger})");
        return Err(UNSETTLED.into());
    }

    // A rotation is already out there with an unknown fate, so the token about
    // to be presented may have been retired by it. Presenting it now is how an
    // ambiguous outcome becomes a cleared session; wait instead.
    let lost_at = lifecycle.lost_rotation_at.load(Ordering::Relaxed);
    if let Some(left) = cooldown_left_secs(lost_at, now_secs()) {
        log::warn!(
            "[tokens] refresh held back for {left}s — a rotation's fate is still unknown (trigger: {trigger})"
        );
        return Err("a rotation with an unknown outcome is still cooling down".into());
    }

    let Some(refresh_token) = current.refresh_token.clone() else {
        return Err("no refresh token stored".into());
    };
    let cfg = load_config(app);
    let base = refresh_base(&cfg).ok_or("no host configured for token refresh")?;

    // Connect timeout only. The total budget is enforced by
    // `under_wall_clock_deadline` instead, which reqwest's own timer cannot do
    // — and setting both would leave two mechanisms racing to report the same
    // condition under different names. Bounding the connect separately still
    // matters: a connect that hangs while Wi-Fi reassociates would otherwise
    // blow the total budget and be classed `Unknown`, arming the cooldown and
    // mislabelling the next rejection, when nothing ever left the machine. Only
    // for an awake machine, mind — this timer is monotonic too, so a suspend
    // still lands on the wall-clock deadline and the conservative `Unknown`.
    let client = reqwest::Client::builder()
        .connect_timeout(REFRESH_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;

    let outcome = attempt_refresh(app, &client, &base, &refresh_token).await;
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
            // Someone signed out while this was on the wire. The rotation
            // succeeded, but persisting it now would write the file back out
            // after custody was deliberately dropped and push a live session to
            // a webview that has already logged out. Let the tokens die with the
            // session; they are the signed-out user's.
            if lifecycle.custody_epoch.load(Ordering::Acquire) != custody {
                log::warn!(
                    "[tokens] a rotation landed after the session was signed out — dropping it (trigger: {trigger})"
                );
                return Ok(NativeAuthTokens::default());
            }
            let mut merged = current;
            merged.merge_from(rotated);
            if let Err(e) = save_tokens(app, &merged) {
                // The gateway has already rotated, so the refresh token still on
                // disk is spent: every later refresh would be rejected until the
                // retry budget gave up and cleared it anyway. End it here, where
                // the cause is still known, rather than through a doomed loop.
                log::error!("[tokens] a completed rotation could not be persisted: {e}");
                return Ok(end_stored_session(
                    app,
                    "rotation could not be persisted",
                    trigger,
                ));
            }
            // Again, because the check above and the write are not one step: a
            // delete landing between them would leave the file back on disk with
            // nothing left to notice. Whichever order the two actually ran in,
            // taking it out here means custody ends up dropped either way.
            if lifecycle.custody_epoch.load(Ordering::Acquire) != custody {
                log::warn!(
                    "[tokens] a rotation was written as the session was signed out — taking it back out (trigger: {trigger})"
                );
                let _ = clear_tokens(app);
                return Ok(NativeAuthTokens::default());
            }
            // Whatever an earlier lost rotation did or didn't do, the token now
            // on disk is one the gateway just issued. Nothing is in doubt any
            // more, so a later rejection is a real revocation and must not be
            // reported — or held back — as that old loss resurfacing.
            lifecycle.lost_rotation_at.store(0, Ordering::Relaxed);
            log::info!(
                "[tokens] refreshed (trigger: {trigger}, access exp: {:?})",
                merged.access_token.as_deref().and_then(jwt_exp_secs)
            );
            emit_token_update(app, &merged);
            Ok(merged)
        }
        Outcome::Rejected => {
            let why = if lifecycle.lost_rotation_at.swap(0, Ordering::Relaxed) != 0 {
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
            Ok(end_stored_session(app, why, trigger))
        }
        Outcome::Unreached(e) => Err(e),
        Outcome::Unknown(e) => {
            // Stamped on every loss, not just the first: each unanswered POST is
            // itself newly in doubt, and pacing from the original one would let
            // the cooldown lapse permanently after a single window while the
            // token was still being re-presented.
            lifecycle
                .lost_rotation_at
                .store(now_secs(), Ordering::Relaxed);
            Err(e)
        }
    }
}

/// One refresh, retried only while the gateway has not seen it. A rejection is
/// final on the first occurrence — re-presenting the same token cannot change
/// that answer — and an unknown outcome is never retried, because the retry is
/// what would turn it into a rejection.
async fn attempt_refresh(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    refresh_token: &str,
) -> Outcome {
    let mut delays = REFRESH_RETRY_DELAYS.iter();
    loop {
        let attempt = post_refresh(client, base, refresh_token);
        // Blowing the wall-clock budget means the request went out and its fate
        // is unknown — the same standing as a broken response, and for the same
        // reason: the gateway may already have rotated.
        let Some(outcome) = under_wall_clock_deadline(REFRESH_WALL_CLOCK_BUDGET, attempt).await
        else {
            log::warn!(
                "[tokens] refresh to {base} outlived its {}s wall-clock budget — treating the outcome as unknown",
                REFRESH_WALL_CLOCK_BUDGET.as_secs()
            );
            return Outcome::Unknown("refresh outlived its wall-clock budget".into());
        };
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
        // Same reason as the gate in `refresh`, for the same monotonic delay:
        // gating only the first of three attempts would leave the other two
        // free to fire at the instant of a resume.
        if let Some(left) = settle_left(app) {
            log::warn!(
                "[tokens] not retrying yet — {left}s of the settle window left ({UNSETTLED})"
            );
            return outcome;
        }
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
            if let Err(e) = refresh(&app, false, None, "the background poll").await {
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
        if let Err(e) = refresh(&app, false, None, why).await {
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

    /// The wall clock counts sleep and the monotonic clock does not, so the gap
    /// between how far each moved is the suspend. Equal movement means the
    /// machine was up the whole time.
    #[test]
    fn sleep_is_the_gap_between_the_two_clocks() {
        assert_eq!(slept_between((1_000, 500), (1_060, 560)), 0);
        assert_eq!(slept_between((1_000, 500), (1_900, 560)), 840);
        // Neither clock running backwards is representable, but a sample pair
        // that arrives out of order must not underflow into a fake suspend.
        assert_eq!(slept_between((1_000, 500), (900, 400)), 0);
    }

    /// A resume starts the settle window; a machine that has been up for a while
    /// is already past it.
    #[test]
    fn a_rotation_waits_out_the_settle_window_after_a_resume() {
        let woke_at = 10_000;
        assert_eq!(settle_left_secs(woke_at, woke_at), Some(WAKE_SETTLE_SECS));
        assert_eq!(
            settle_left_secs(woke_at, woke_at + 5),
            Some(WAKE_SETTLE_SECS - 5)
        );
        assert_eq!(settle_left_secs(woke_at, woke_at + WAKE_SETTLE_SECS), None);
        assert_eq!(settle_left_secs(woke_at, woke_at + 3_600), None);
    }

    /// No recorded resume must not gate anything: if the wake watch is not
    /// running, refreshes behave exactly as they did before it existed. The
    /// monotonic clock starts near zero, so this also covers process start,
    /// where gating would delay the webview's opening token read.
    #[test]
    fn an_unrecorded_resume_gates_nothing() {
        assert_eq!(settle_left_secs(0, 0), None);
        assert_eq!(settle_left_secs(0, 5), None);
    }

    /// The window that stops a lost rotation's ambiguity from being cashed in
    /// as a cleared session by whoever holds the lock next.
    #[test]
    fn a_lost_rotation_holds_the_token_back_until_it_cools_down() {
        let lost_at = 10_000;
        assert_eq!(
            cooldown_left_secs(lost_at, lost_at + 1),
            Some(LOST_ROTATION_COOLDOWN_SECS - 1)
        );
        assert_eq!(
            cooldown_left_secs(lost_at, lost_at + LOST_ROTATION_COOLDOWN_SECS),
            None
        );
    }

    /// A rotation response may carry one token or both, and the half that did
    /// not arrive must survive the merge — blanking it would drop custody of a
    /// live session.
    #[test]
    fn merging_keeps_the_half_that_did_not_arrive() {
        let mut stored = tokens(Some("old-access".into()), Some("old-refresh"));
        stored.merge_from(tokens(Some("new-access".into()), None));
        assert_eq!(stored.access_token.as_deref(), Some("new-access"));
        assert_eq!(stored.refresh_token.as_deref(), Some("old-refresh"));

        stored.merge_from(tokens(None, Some("new-refresh")));
        assert_eq!(stored.access_token.as_deref(), Some("new-access"));
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"));
    }

    /// The zero mark means nothing is in doubt, so it must never gate — which is
    /// what lets the flag and the timestamp be one field.
    #[test]
    fn nothing_in_doubt_holds_nothing_back() {
        assert_eq!(cooldown_left_secs(0, now_secs()), None);
    }

    /// The cooldown errs the other way from the settle gate on an unreadable
    /// age: holding a token back costs a delayed refresh, presenting a retired
    /// one costs the session.
    #[test]
    fn a_cooldown_mark_ahead_of_the_clock_still_holds() {
        let now = 10_000;
        assert_eq!(
            cooldown_left_secs(now + 3_600, now),
            Some(LOST_ROTATION_COOLDOWN_SECS)
        );
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
