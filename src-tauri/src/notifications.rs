// Rust-owned background notification plane: subscribes to
// `user.<userId>.notification` on the shell's own NATS connection (nats.rs; the
// webview keeps a separate one for interactive streams) and dispatches OS
// notifications + badge while the window sits hidden in the tray.
//
// Click delivery per platform (aligned with openframe-chat's nats_bridge):
//   - macOS bundled builds: `UNUserNotificationCenter` (`macos_un`) reports
//     every click — live banner, Notification Center hours later, or a click
//     that launches the app cold — with the payload in the notification's
//     userInfo.
//   - Windows: toasts activate an `openframe-desktop://notify` URI, which
//     reaches a running instance through single-instance argv forwarding or
//     launches the app cold (`handle_notification_uri`). Their action buttons
//     take the other route, through the COM activator (`windows_activator`).
//   - macOS dev builds (unbundled — UN APIs abort there) and other platforms
//     have no OS notification backend.
//
// On both desktop platforms a notification that carries an actionable context
// gets buttons that complete the work with no window and no webview; what they
// do, and the session gate they do it under, is `notification_actions`.
//
// Unlike chat, the shell does not map notifications to routes: the payload
// forwarded on `notification:click` is the envelope's `context` in wire shape,
// which the webview resolves with the same mapping the in-app drawer uses.
// Every click path funnels through `deliver_click`, which stashes the payload
// until the webview signals readiness (`take_startup_click`), so a cold-start
// click is not emitted into a listener that has not mounted yet.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard};

use futures::StreamExt;
use tauri::{AppHandle, Emitter, Manager};

use crate::{show_primary_window, tokens, MAIN_LABEL};

/// Consumed by `onNativeNotificationClick` in the frontend's native-shell.ts —
/// a rename here silently stops routing there.
const CLICK_EVENT: &str = "notification:click";
/// Length limit for text the shell did not write: an incoming envelope's
/// `description` and the gateway's error text. A body the shell composes itself
/// is deliberately exempt — a failed reply's banner carries the user's own
/// words, and trimming those would discard the only copy left.
pub(crate) const BODY_CHARS: usize = 140;
/// The envelope `eventType` that supersedes an earlier push rather than being a
/// new thing to tell the user about. See [`delivery_of`].
const UPDATED_EVENT: &str = "UPDATED";

/// Whether a notification is the first the user hears of something, or
/// supersedes one they were already told about. An update must not interrupt
/// again: the user has already been alerted once for this, and what changed is
/// the outcome of a decision, not a new one to make.
///
/// Lives here, with the rest of the envelope's vocabulary, rather than beside
/// the backends that honour it — `notification_actions` is not built on Linux,
/// and this module is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Delivery {
    New,
    Update,
}

/// Custom URI scheme Windows toasts activate through, registered under HKCU at
/// startup by `crate::register_url_scheme`. Windows-only: every other platform
/// delivers clicks in-process (macOS) or not at all.
#[cfg(any(target_os = "windows", test))]
pub(crate) const URI_SCHEME: &str = "openframe-desktop";
#[cfg(any(target_os = "windows", test))]
const CLICK_URI_PREFIX: &str = "openframe-desktop://notify";

pub(crate) struct NotificationsPlane {
    subscription: tokio::sync::Mutex<Option<SubscriptionSlot>>,
    unread: AtomicU32,
    /// `true` once the webview pulled `take_startup_click`, i.e. its
    /// `notification:click` listener is mounted. Until then click payloads are
    /// parked in `stashed_click`. Only touched while holding the stash lock,
    /// which serializes the ready-flip + drain against a concurrent
    /// emit-or-stash decision.
    webview_click_ready: AtomicBool,
    stashed_click: StdMutex<Option<serde_json::Value>>,
}

struct SubscriptionSlot {
    subject: String,
    task: tauri::async_runtime::JoinHandle<()>,
}

/// Register the plane's state and the platform's click/action backend — the
/// notification-center delegate on macOS, the COM activator on Windows. Must run
/// before `crate::nats::spawn`, whose Connected handler subscribes through this
/// module.
pub(crate) fn init(app: &AppHandle) {
    app.manage(NotificationsPlane {
        subscription: tokio::sync::Mutex::new(None),
        unread: AtomicU32::new(0),
        webview_click_ready: AtomicBool::new(false),
        stashed_click: StdMutex::new(None),
    });
    #[cfg(target_os = "macos")]
    crate::macos_un::init(app);
    #[cfg(target_os = "windows")]
    crate::windows_activator::init(app);
}

/// Runs on every NATS Connected event. async-nats replays plain SUBs across
/// reconnects, so an existing router for the same subject is kept; the router
/// is replaced only when the signed-in user changed.
pub(crate) async fn ensure_subscription(app: &AppHandle, client: async_nats::Client) {
    let plane = app.state::<NotificationsPlane>();
    let Some(user_id) = tokens::load_tokens(app)
        .access_token
        .as_deref()
        .and_then(|token| tokens::jwt_claim_str(token, "userId"))
    else {
        // No session: drop any surviving router so a signed-out user's
        // notifications stop here rather than at the next reconnect.
        drop_subscription(app, "no session").await;
        return;
    };
    let subject = format!("user.{user_id}.notification");

    let mut slot = plane.subscription.lock().await;
    if let Some(existing) = slot.as_ref() {
        if existing.subject == subject {
            return;
        }
        log::info!(
            "[notifications] user changed ({} -> {subject}) — replacing router",
            existing.subject
        );
        existing.task.abort();
        *slot = None;
    }

    let subscriber = match client.subscribe(subject.clone()).await {
        Ok(subscriber) => subscriber,
        Err(err) => {
            log::warn!("[notifications] subscribe to {subject} failed: {err}");
            return;
        }
    };
    log::info!("[notifications] subscribed to {subject}");
    // Prompt for OS permission only once we know someone is signed in, rather
    // than on the very first launch of a shell that has no session yet.
    #[cfg(target_os = "macos")]
    crate::macos_un::ensure_authorized();

    let task_app = app.clone();
    let task_subject = subject.clone();
    let task = tauri::async_runtime::spawn(async move {
        notification_router(task_app, task_subject, user_id, subscriber).await;
    });
    *slot = Some(SubscriptionSlot { subject, task });
}

async fn notification_router(
    app: AppHandle,
    subject: String,
    user_id: String,
    mut subscriber: async_nats::Subscriber,
) {
    while let Some(message) = subscriber.next().await {
        // Payload carries user-facing content — full dump only at debug.
        log::info!(
            "[notifications] received on '{}' ({} bytes)",
            message.subject,
            message.payload.len()
        );
        log::debug!(
            "[notifications] payload: {}",
            String::from_utf8_lossy(&message.payload)
        );
        let payload: serde_json::Value = match serde_json::from_slice(&message.payload) {
            Ok(value) => value,
            Err(err) => {
                log::warn!("[notifications] dropping non-JSON notification: {err}");
                continue;
            }
        };
        maybe_notify(&app, &payload, &user_id);
    }
    // The stream only closes when the client is torn down — clear the slot
    // (only if it is still ours; a user switch may have replaced it) so the
    // next Connected re-subscribes instead of seeing a dead router as alive.
    if let Some(plane) = app.try_state::<NotificationsPlane>() {
        let mut slot = plane.subscription.lock().await;
        if slot.as_ref().is_some_and(|s| s.subject == subject) {
            *slot = None;
        }
    }
    log::info!("[notifications] router exited (stream closed) — will re-subscribe on next connect");
}

/// Every envelope on this subject is user-displayable by contract — anything
/// with a `title` fires unless the user is already looking at the app (the
/// webview's own subscription drives the in-app drawer either way).
///
/// `user_id` is the subject's user — whoever the notification was delivered to.
/// It rides along to the platform backend so an action taken hours later can
/// refuse to run under a different session (see
/// `notification_actions::run_action`).
fn maybe_notify(app: &AppHandle, envelope: &serde_json::Value, user_id: &str) {
    // Bookkeeping first, ahead of both display gates: an envelope carrying a
    // verdict is the gateway telling every consumer the decision is made, and
    // that is true whether or not this envelope is one the user gets to see.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    crate::notification_actions::note_resolution(envelope.get("context"));

    let Some(title) = string_field(envelope, "title") else {
        log::debug!("[notifications] ignoring envelope without title");
        return;
    };
    let delivery = delivery_of(envelope);
    let click = click_payload(envelope);
    // An update that lands on the banner it supersedes is posted silently, so
    // the usual "don't interrupt someone already looking at the app" gate would
    // cost the replacement and buy nothing — the stale banner would keep the
    // buttons of a decision already made. All three conditions are load-bearing:
    // Windows is the only platform that can post without alerting, and an update
    // with nothing to land on is not a correction but a second notification,
    // which is exactly what the gate is for.
    let silent_update = delivery == Delivery::Update
        && cfg!(target_os = "windows")
        && supersedes_a_banner(click.as_ref());
    if !silent_update && !should_notify(app) {
        log::debug!("[notifications] window visible+focused — skipping notification");
        return;
    }
    let body = string_field(envelope, "description")
        .map(|d| truncate_for_notification(&d, BODY_CHARS))
        .unwrap_or_default();

    let plane = app.state::<NotificationsPlane>();
    // An update supersedes something the user already owes attention to, so it
    // must not be counted twice — the frontend's drawer leaves its own count
    // alone on an update for the same reason.
    if delivery == Delivery::New {
        let unread = plane.unread.fetch_add(1, Ordering::Relaxed) + 1;
        set_badge(app, unread);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        use crate::notification_actions::{live_kind_for, post};
        // `live_kind_for`, not `kind_for`: a settled request keeps its payload
        // but must not keep its buttons.
        let kind = live_kind_for(click.as_ref());
        post(
            app,
            title,
            body,
            click,
            Some(user_id.to_string()),
            kind,
            delivery,
        );
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (title, body, click, user_id);
        log::debug!("[notifications] no OS notification backend on this platform");
    }
}

/// Whether a payload names something a later notification takes the place of.
/// The one caller is the silent-update decision above, and it is the same fact
/// the Windows backend tags a toast by — an update that supersedes nothing has
/// no banner to correct, and posting it quietly would only hide it.
fn supersedes_a_banner(click: Option<&serde_json::Value>) -> bool {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        crate::notification_actions::approval_request_id(click).is_some()
    }
    // Linux has no notification backend, so nothing is ever on screen to
    // supersede — and `notification_actions` is not built here to ask.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = click;
        false
    }
}

/// `eventType` on the envelope: `CREATED` for the first push of a notification,
/// `UPDATED` for one that supersedes it under the same notification id. Absent
/// means the first push.
///
/// Matched exactly, where `note_resolution` reads its verdict case-insensitively
/// — deliberately, because each mirrors how the frontend reads the same field
/// (`payload.eventType === 'UPDATED'` against `resolutionToStatus`'s
/// `toUpperCase()`), and the gateway feeds both consumers the same envelope.
fn delivery_of(envelope: &serde_json::Value) -> Delivery {
    match string_field(envelope, "eventType").as_deref() {
        Some(UPDATED_EVENT) => Delivery::Update,
        _ => Delivery::New,
    }
}

// ---------------------------------------------------------------------------
// Click payload + delivery
// ---------------------------------------------------------------------------

/// The webview's `notification:click` payload: the envelope's routing context
/// in wire shape, which the frontend's `resolveNatsNotificationRoute` maps to a
/// route. Only the fields that mapping reads — plus `approvalRequestId`, which
/// the Approve/Reject buttons resolve against the chat API — survive. The rest
/// of `context` can be arbitrarily large (an approval request carries the whole
/// `toolCalls` array), and it has to fit both in a Windows activation URI and in
/// a toast payload that repeats it once per button; every id kept here is a
/// UUID. `None` when the envelope points at nothing openable; the click then
/// only raises the window.
fn click_payload(envelope: &serde_json::Value) -> Option<serde_json::Value> {
    let context = envelope.get("context")?;
    let routing: serde_json::Map<_, _> = ["type", "ticketId", "dialogId", "approvalRequestId"]
        .into_iter()
        .filter_map(|key| Some((key.to_string(), context.get(key)?.clone())))
        .collect();
    (!routing.is_empty()).then(|| serde_json::json!({ "context": routing }))
}

/// `openframe-desktop://notify?context=<percent-encoded JSON>`.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn click_uri(click: Option<&serde_json::Value>) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    match click.and_then(|c| c.get("context")) {
        Some(context) => format!(
            "{CLICK_URI_PREFIX}?context={}",
            utf8_percent_encode(&context.to_string(), NON_ALPHANUMERIC)
        ),
        None => CLICK_URI_PREFIX.to_string(),
    }
}

/// Inverse of [`click_uri`]. `None` for a bare or malformed URI — the click
/// still raises the window, it just doesn't navigate.
#[cfg(any(target_os = "windows", test))]
fn parse_click_uri(uri: &str) -> Option<serde_json::Value> {
    let rest = uri.strip_prefix(CLICK_URI_PREFIX)?;
    let query = rest.strip_prefix('/').unwrap_or(rest).strip_prefix('?')?;
    let encoded = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("context="))?;
    let json = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?;
    payload_from_context_json(&json)
}

/// Wrap a decoded `context` object back into the shape every click path carries
/// it in. Both Windows activation transports land here — the URI a toast body
/// activates and the arguments a toast button carries — so the envelope shape
/// the webview and `kind_for` depend on is asserted once.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn payload_from_context_json(json: &str) -> Option<serde_json::Value> {
    let context: serde_json::Value = serde_json::from_str(json).ok()?;
    context
        .is_object()
        .then(|| serde_json::json!({ "context": context }))
}

/// Handles an `openframe-desktop://notify` URI from a Windows toast click,
/// arriving either as our own argv (cold start) or forwarded by the
/// single-instance plugin (warm click). Returns `false` for foreign URIs.
#[cfg(target_os = "windows")]
pub(crate) fn handle_notification_uri(app: &AppHandle, uri: &str) -> bool {
    if !uri.starts_with(CLICK_URI_PREFIX) {
        return false;
    }
    let payload = parse_click_uri(uri);
    log::info!(
        "[notifications] click URI received (target: {})",
        payload.is_some()
    );
    deliver_click(app, payload);
    true
}

/// The OS identified the exact notification that was clicked: hand the payload
/// to the webview and raise the window.
pub(crate) fn deliver_click(app: &AppHandle, payload: Option<serde_json::Value>) {
    match payload {
        Some(payload) => {
            // Context can carry user-facing content — identify it at info, dump
            // it only at debug, same policy as notification_router.
            log::info!(
                "[notifications] activated — forwarding context.type={}",
                payload
                    .pointer("/context/type")
                    .unwrap_or(&serde_json::Value::Null)
            );
            log::debug!("[notifications] click payload: {payload}");
            emit_or_stash(app, payload);
        }
        None => log::info!("[notifications] activated — opening window"),
    }
    show_primary_window(app);
}

/// Emit `notification:click`, or stash it if the webview has not signalled
/// readiness yet (cold start: the click that launched the app happens before
/// React mounts the listener).
fn emit_or_stash(app: &AppHandle, payload: serde_json::Value) {
    let Some(plane) = app.try_state::<NotificationsPlane>() else {
        return;
    };
    // The readiness check and the stash write share the stash lock: checked
    // outside it, a concurrent take_startup_click could flip ready and drain
    // between our check and store, stranding the payload until the next
    // webview reload.
    let mut stash = lock(&plane.stashed_click);
    if plane.webview_click_ready.load(Ordering::Acquire) {
        let _ = app.emit_to(MAIN_LABEL, CLICK_EVENT, payload);
    } else {
        log::info!("[notifications] webview not ready — stashing notification click");
        *stash = Some(payload);
    }
}

/// Invoked once per webview document, when its `notification:click` listener
/// mounts. Opens the gate and drains a click that happened before the listener
/// existed (a notification click that cold-started the app).
pub(crate) fn take_startup_click(app: &AppHandle) -> Option<serde_json::Value> {
    let plane = app.state::<NotificationsPlane>();
    // Flip ready and drain under the stash lock — see emit_or_stash.
    let mut stash = lock(&plane.stashed_click);
    plane.webview_click_ready.store(true, Ordering::Release);
    stash.take()
}

/// Close the gate again for a fresh webview generation. The listener does not
/// survive a page load, so without this the one-way latch would let clicks be
/// emitted at a window whose listener has not mounted yet — the exact case the
/// stash exists for. Driven by the main window's PageLoadEvent::Started.
pub(crate) fn reset_click_gate(app: &AppHandle) {
    let Some(plane) = app.try_state::<NotificationsPlane>() else {
        return;
    };
    // Load-bearing guard, not a discard: it serializes the flip against
    // emit_or_stash's check-then-store — see emit_or_stash.
    let _stash = lock(&plane.stashed_click);
    plane.webview_click_ready.store(false, Ordering::Release);
}

/// Session over (tray Sign Out, in-app logout, or a force-logout): stop the
/// previous user's notifications immediately. The NATS connection outlives the
/// session, so waiting for a reconnect would leak their notification content to
/// whoever signs in next. Re-subscribing is `nats::resubscribe`'s job once new
/// tokens land.
///
/// Deliberately does NOT close the click gate: only the webview reopens it, and
/// it does so once per document — closing it here would strand every later
/// click on the logout paths that don't reload the page.
pub(crate) fn end_session(app: &AppHandle) {
    // Before the spawn, not inside it: the decisions belong to the session that
    // made them, and a fast sign-out/sign-in could otherwise land this clear on
    // top of what the next session has already learned from the wire.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    crate::notification_actions::forget_resolved();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        drop_subscription(&app, "session ended").await;
        // Detached: this can land after the app has torn its state down.
        let Some(plane) = app.try_state::<NotificationsPlane>() else {
            return;
        };
        plane.unread.store(0, Ordering::Relaxed);
        set_badge(&app, 0);
        *lock(&plane.stashed_click) = None;
    });
}

/// Abort the router task and vacate the slot, so the next `ensure_subscription`
/// installs a fresh one instead of seeing a dead router as live. Dropping the
/// router also drops its `Subscriber`, which holds a clone of the client's
/// command channel — the connection cannot close while one is alive.
pub(crate) async fn drop_subscription(app: &AppHandle, reason: &str) {
    let Some(plane) = app.try_state::<NotificationsPlane>() else {
        return;
    };
    // Bound separately: the guard temporary must drop before `plane` does.
    let slot = plane.subscription.lock().await.take();
    if let Some(slot) = slot {
        log::info!("[notifications] {reason} — dropping {}", slot.subject);
        slot.task.abort();
    }
}

/// Called from the main window's Focused(true) event: the user is looking at
/// the app, so the unread badge is stale.
pub(crate) fn on_main_focused(app: &AppHandle) {
    let Some(plane) = app.try_state::<NotificationsPlane>() else {
        return;
    };
    plane.unread.store(0, Ordering::Relaxed);
    // Unconditional: sign-out zeroes the counter while the window is being
    // destroyed, so its set_badge(0) no-ops and only this call can clear the
    // badge the signed-out user left behind.
    set_badge(app, 0);
}

fn should_notify(app: &AppHandle) -> bool {
    // No window = the user can't be looking at the app — notify. Unknown
    // state counts as not engaged.
    let Some(main) = app.get_webview_window(MAIN_LABEL) else {
        return true;
    };
    let visible = main.is_visible().unwrap_or(false);
    let focused = main.is_focused().unwrap_or(false);
    !(visible && focused)
}

/// macOS/Linux only in practice: tauri-runtime-wry has no Windows arm for
/// SetBadgeCount, so this is a silent no-op there (not even an Err to log).
fn set_badge(app: &AppHandle, count: u32) {
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        let badge = if count == 0 { None } else { Some(count as i64) };
        if let Err(err) = window.set_badge_count(badge) {
            log::debug!("[notifications] set_badge_count failed: {err}");
        }
    }
}

pub(crate) fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn truncate_for_notification(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A poisoned lock is not a reason to take the process down: every mutex behind
/// this guards display state, and a panic that poisoned one has already been
/// reported. Shared with `notification_actions`, which guards its own.
pub(crate) fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_uri_roundtrip() {
        let envelope = serde_json::json!({
            "title": "Ticket updated",
            "context": { "type": "TICKET_STATUS_CHANGED", "ticketId": "6a4fda9ba8b65c28c4dbf6ba" }
        });
        let payload = click_payload(&envelope).unwrap();
        assert_eq!(
            parse_click_uri(&click_uri(Some(&payload))).unwrap(),
            payload
        );
    }

    #[test]
    fn click_uri_roundtrip_survives_reserved_chars() {
        let envelope = serde_json::json!({
            "context": { "type": "CLIENT_AI_MESSAGE", "dialogId": "abc/д ф&x=1" }
        });
        let payload = click_payload(&envelope).unwrap();
        assert_eq!(
            parse_click_uri(&click_uri(Some(&payload))).unwrap(),
            payload
        );
    }

    #[test]
    fn envelopes_without_context_have_no_payload() {
        assert!(click_payload(&serde_json::json!({ "title": "Hi" })).is_none());
        assert!(click_payload(&serde_json::json!({ "context": "not-an-object" })).is_none());
        assert!(click_payload(&serde_json::json!({ "context": { "ticketId": "" } })).is_some());
        assert_eq!(click_uri(None), CLICK_URI_PREFIX);
    }

    /// The bulk a context can carry (an approval request's toolCalls) must not
    /// reach the activation URI — Windows truncates it at ~2 KB. The ids the
    /// macOS action buttons act on must survive it.
    #[test]
    fn click_payload_keeps_only_routing_fields() {
        let envelope = serde_json::json!({
            "context": {
                "type": "ADMIN_APPROVAL_REQUEST",
                "ticketId": "abc",
                "approvalRequestId": "0a2a0b3c-9d1e-4f5a-8b7c-6d5e4f3a2b1c",
                "toolCalls": [{ "toolExplanation": "x".repeat(4096) }],
            }
        });
        let payload = click_payload(&envelope).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({ "context": {
                "type": "ADMIN_APPROVAL_REQUEST",
                "ticketId": "abc",
                "approvalRequestId": "0a2a0b3c-9d1e-4f5a-8b7c-6d5e4f3a2b1c",
            } })
        );
        assert!(click_uri(Some(&payload)).len() < 2048);
    }

    /// The scheme half of the activation URI is what `register_url_scheme`
    /// writes to HKCU; if they drift, toasts activate a scheme nothing claims.
    #[test]
    fn click_uri_prefix_uses_the_registered_scheme() {
        assert_eq!(CLICK_URI_PREFIX.split_once("://").unwrap().0, URI_SCHEME);
    }

    /// `UPDATED` is the gateway saying "you already know about this one" —
    /// anything else, including an envelope from a build that sends no
    /// `eventType` at all, is something the user has not seen yet.
    #[test]
    fn only_an_updated_envelope_supersedes() {
        let updated = serde_json::json!({ "title": "Approval", "eventType": "UPDATED" });
        assert_eq!(delivery_of(&updated), Delivery::Update);
        for envelope in [
            serde_json::json!({ "title": "Approval", "eventType": "CREATED" }),
            serde_json::json!({ "title": "Approval" }),
            serde_json::json!({ "title": "Approval", "eventType": "" }),
            serde_json::json!({ "title": "Approval", "eventType": 7 }),
        ] {
            assert_eq!(delivery_of(&envelope), Delivery::New, "{envelope}");
        }
    }

    #[test]
    fn malformed_uris_have_no_payload() {
        assert!(parse_click_uri(CLICK_URI_PREFIX).is_none());
        assert!(parse_click_uri("openframe-desktop://notify?context=%7B%7D").is_some());
        assert!(parse_click_uri("openframe-desktop://notify?context=not-json").is_none());
        assert!(parse_click_uri("openframe-desktop://notify?id=x").is_none());
        assert!(parse_click_uri("openframe-chat://notify?context=%7B%7D").is_none());
    }
}
