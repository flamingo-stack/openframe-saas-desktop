// Rust-owned background notification plane: a second NATS connection (the
// webview keeps its own for interactive streams) that stays alive while the
// window sits hidden in the tray, subscribed to `user.<userId>.notification`
// and dispatching OS toasts + badge. Ported from openframe-chat's
// nats_bridge/notifications.rs on top of the shared connector crate; the one
// behavioral difference is click routing — the raw payload is forwarded to the
// webview (`notification:click`), which owns the notification→route mapping,
// instead of parsing targets in Rust like chat does.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use futures::StreamExt;
use openframe_nats_connector::{async_nats, ConnectSource, ConnectorConfig, NatsConnector};
use tauri::{AppHandle, Emitter, Manager};

use crate::{load_config, show_primary_window, tokens, MAIN_LABEL};

const CLICK_EVENT: &str = "notification:click";
/// A toast click/window focus older than this no longer navigates.
const MAX_PENDING_AGE: Duration = Duration::from_secs(30);

struct ShellConnectSource {
    app: AppHandle,
}

impl ConnectSource for ShellConnectSource {
    async fn server_url(&self) -> Option<String> {
        let cfg = load_config(&self.app);
        cfg.learned_host.or(cfg.host).filter(|s| !s.is_empty())
    }

    async fn token(&self) -> Option<String> {
        // ensure_fresh refreshes an expiring bearer inline, so a reconnect
        // after hours in the tray (or laptop sleep) dials with a valid token
        // instead of waiting for the poll loop.
        tokens::ensure_fresh(&self.app).await.access_token
    }
}

pub struct NotificationsPlane {
    subscription: tokio::sync::Mutex<Option<SubscriptionSlot>>,
    unread: AtomicU32,
    /// Most recent toast's payload + when it fired. Consumed by the
    /// window-focus handler to emit `notification:click`.
    pending_click: StdMutex<Option<PendingClick>>,
}

struct SubscriptionSlot {
    subject: String,
    task: tauri::async_runtime::JoinHandle<()>,
}

struct PendingClick {
    payload: serde_json::Value,
    fired_at: Instant,
}

/// Build the connector, register the resubscribe hook, and spawn the connect
/// loop. Called once from setup().
pub fn init(app: &AppHandle) {
    app.manage(NotificationsPlane {
        subscription: tokio::sync::Mutex::new(None),
        unread: AtomicU32::new(0),
        pending_click: StdMutex::new(None),
    });

    let connector = NatsConnector::new(
        ConnectorConfig {
            client_name: "openframe-desktop".into(),
            ..ConnectorConfig::default()
        },
        ShellConnectSource { app: app.clone() },
    );

    let hook_app = app.clone();
    tauri::async_runtime::spawn(async move {
        connector
            .on_connected(move |client| {
                let app = hook_app.clone();
                async move {
                    ensure_subscription(app, client).await;
                }
            })
            .await;
        connector.run().await;
    });
}

/// Runs on every Connected event. async-nats replays plain SUBs across
/// reconnects, so an existing router for the same subject is kept; the router
/// is replaced only when the signed-in user changed.
async fn ensure_subscription(app: AppHandle, client: async_nats::Client) {
    let Some(user_id) = tokens::load_tokens(&app)
        .access_token
        .as_deref()
        .and_then(|token| tokens::jwt_claim_str(token, "userId"))
    else {
        log::warn!("[notifications] no userId claim in stored access token — not subscribing");
        return;
    };
    let subject = format!("user.{user_id}.notification");

    let plane = app.state::<NotificationsPlane>();
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

    let task_app = app.clone();
    let task_subject = subject.clone();
    let task = tauri::async_runtime::spawn(async move {
        notification_router(task_app, task_subject, subscriber).await;
    });
    *slot = Some(SubscriptionSlot { subject, task });
}

async fn notification_router(app: AppHandle, subject: String, mut subscriber: async_nats::Subscriber) {
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
        maybe_notify(&app, payload);
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
/// with a `title` fires as a toast unless the user is already looking at the
/// app (the webview's own subscription drives the in-app drawer either way).
fn maybe_notify(app: &AppHandle, payload: serde_json::Value) {
    let Some(title) = payload
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        log::debug!("[notifications] ignoring envelope without title");
        return;
    };
    if !should_notify(app) {
        log::debug!("[notifications] window visible+focused — skipping toast");
        return;
    }
    let body = payload
        .get("description")
        .and_then(|v| v.as_str())
        .map(|d| truncate_for_toast(d, 140))
        .unwrap_or_default();

    let plane = app.state::<NotificationsPlane>();
    if let Ok(mut pending) = plane.pending_click.lock() {
        *pending = Some(PendingClick {
            payload: payload.clone(),
            fired_at: Instant::now(),
        });
    }
    let unread = plane.unread.fetch_add(1, Ordering::Relaxed) + 1;
    set_badge(app, unread);

    fire_toast(app.clone(), title, body, payload);
}

fn fire_toast(app: AppHandle, title: String, body: String, payload: serde_json::Value) {
    // notify-rust's response wait blocks; keep it off the async runtime.
    std::thread::spawn(move || {
        let mut notification = notify_rust::Notification::new();
        notification.summary(&title);
        if !body.is_empty() {
            notification.body(&body);
        }
        notification.action("open", "Open");

        #[cfg(target_os = "macos")]
        {
            // Unsigned dev binaries can't post notifications under their own
            // identity — borrow Terminal's, same workaround as openframe-chat.
            let identifier = if tauri::is_dev() {
                "com.apple.Terminal".to_string()
            } else {
                app.config().identifier.clone()
            };
            let _ = notify_rust::set_application(&identifier);
        }
        #[cfg(target_os = "windows")]
        if !tauri::is_dev() {
            notification.app_id(&app.config().identifier);
        }

        match notification.show() {
            Ok(handle) => {
                log::info!("[notifications] toast fired: {title}");
                use notify_rust::NotificationResponse;
                let _ = handle.wait_for_response(|response: &NotificationResponse| {
                    if !matches!(response, NotificationResponse::Closed(_)) {
                        log::info!("[notifications] toast activated — opening window");
                        show_primary_window(&app);
                        let _ = app.emit_to(MAIN_LABEL, CLICK_EVENT, payload.clone());
                        // The focus handler must not replay this click.
                        if let Some(plane) = app.try_state::<NotificationsPlane>() {
                            if let Ok(mut pending) = plane.pending_click.lock() {
                                *pending = None;
                            }
                        }
                    }
                });
            }
            Err(err) => log::warn!("[notifications] toast show failed: {err}"),
        }
    });
}

/// Called from the main window's Focused(true) event: clears the badge and,
/// when a toast fired recently, emits `notification:click` so the webview can
/// navigate (covers platforms where the toast's own activation callback is
/// unreliable and the user clicks the window instead).
pub fn on_main_focused(app: &AppHandle) {
    let Some(plane) = app.try_state::<NotificationsPlane>() else {
        return;
    };
    if plane.unread.swap(0, Ordering::Relaxed) > 0 {
        set_badge(app, 0);
    }
    let pending = match plane.pending_click.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => None,
    };
    let Some(pending) = pending else { return };
    if pending.fired_at.elapsed() > MAX_PENDING_AGE {
        log::debug!("[notifications] dropping stale pending click");
        return;
    }
    log::info!("[notifications] window focused after toast — emitting {CLICK_EVENT}");
    let _ = app.emit_to(MAIN_LABEL, CLICK_EVENT, pending.payload);
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

fn set_badge(app: &AppHandle, count: u32) {
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        let badge = if count == 0 { None } else { Some(count as i64) };
        if let Err(err) = window.set_badge_count(badge) {
            log::debug!("[notifications] set_badge_count failed: {err}");
        }
    }
}

fn truncate_for_toast(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
