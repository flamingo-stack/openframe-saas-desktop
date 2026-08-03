// The half of the notification action buttons that is not platform-specific:
// which buttons a notification earns, what a pressed one does, and how the
// outcome comes back.
//
// Each backend owns only its own plumbing — macOS registers UN categories and
// reads `userInfo` (`macos_un`), Windows emits toast XML and reads the COM
// activator's arguments (`windows_toast`, `windows_activator`) — and they meet
// here for the decision itself. The session gate especially has exactly one
// implementation: it is the only thing standing between a banner that has been
// sitting around for days and an authenticated write.

use tauri::AppHandle;

use crate::{chat_api, notifications::string_field};

/// Envelope `context.type` values that earn action buttons. The rest of the set
/// (tickets, client chats) has no action the shell can complete on its own.
const APPROVAL_CONTEXT_TYPE: &str = "ADMIN_APPROVAL_REQUEST";
const MESSAGE_CONTEXT_TYPE: &str = "ADMIN_AI_MESSAGE";

/// Which button set a notification gets. Derived from the click payload rather
/// than passed in, so the envelope contract lives in one place and the same
/// notification offers the same decision on either OS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ActionKind {
    /// Nothing to complete from the banner: open the app, that is all.
    Default,
    Approval,
    Message,
}

/// Buttons are offered only when the id the action needs actually survived the
/// payload projection — an Approve that can resolve nothing is worse than no
/// Approve at all.
pub(crate) fn kind_for(click: Option<&serde_json::Value>) -> ActionKind {
    match context_str(click, "type").as_deref() {
        Some(APPROVAL_CONTEXT_TYPE) if context_str(click, "approvalRequestId").is_some() => {
            ActionKind::Approval
        }
        Some(MESSAGE_CONTEXT_TYPE) if context_str(click, "dialogId").is_some() => {
            ActionKind::Message
        }
        _ => ActionKind::Default,
    }
}

pub(crate) enum Action {
    Approve,
    Reject,
    Reply(String),
}

impl Action {
    /// Success banner title.
    fn done(&self) -> &'static str {
        match self {
            Action::Approve => "Approved",
            Action::Reject => "Rejected",
            Action::Reply(_) => "Reply sent",
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Action::Approve => "approve",
            Action::Reject => "reject",
            Action::Reply(_) => "send the reply",
        }
    }

    /// Failure banner body. A failed reply carries the text back: responding to
    /// the notification cleared the inline field, so this is the only copy left.
    fn failed(&self, reason: &str) -> String {
        let line = format!("Could not {} — {reason}.", self.verb());
        match self {
            Action::Reply(text) => format!("{line} Your reply: {text}"),
            _ => line,
        }
    }
}

/// Everything a background action needs, lifted out of the OS's response before
/// the async work starts — on macOS none of it can cross a thread boundary
/// otherwise, and on Windows the activator's buffers are the OS's to free.
/// Keeping the original title also lets a failure re-post the notification still
/// actionable, so the decision is not lost.
pub(crate) struct ActionContext {
    pub(crate) title: String,
    pub(crate) payload: Option<serde_json::Value>,
    pub(crate) user_id: Option<String>,
}

/// Run a pressed button and report the outcome. Both backends hand off here as
/// soon as they have decoded the response, and neither waits for it: macOS is on
/// the main thread inside a delegate callback, Windows is on an RPC thread the
/// activator must release.
pub(crate) fn spawn(app: &AppHandle, context: ActionContext, action: Action) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let outcome = run_action(&app, &context, &action).await;
        report(&app, &context, &action, outcome);
    });
}

/// The session gate. A notification sitting in Notification Center or the Action
/// Center is not authority to act on anything: the shell acts only while the
/// user it was delivered to is still signed in, so an approval cannot be granted
/// from a signed-out app, and a stale notification cannot act under whoever
/// signed in after (the notification plane keeps running across a user switch on
/// the same machine).
async fn run_action(
    app: &AppHandle,
    context: &ActionContext,
    action: &Action,
) -> Result<(), String> {
    let session = chat_api::active_session(app)
        .await
        .ok_or("no one is signed in — open OpenFrame and sign in, then try again")?;
    if context.user_id.as_deref() != Some(session.user_id.as_str()) {
        return Err("this notification belongs to a different account".into());
    }

    match action {
        Action::Approve | Action::Reject => {
            let request_id = context_str(context.payload.as_ref(), "approvalRequestId")
                .ok_or("this notification carries no approval request")?;
            let approve = matches!(action, Action::Approve);
            chat_api::resolve_approval(app, &session, &request_id, approve).await
        }
        Action::Reply(text) => {
            let dialog_id = context_str(context.payload.as_ref(), "dialogId")
                .ok_or("this notification carries no conversation")?;
            chat_api::send_message(app, &session, &dialog_id, text).await
        }
    }
}

/// A background action has no window to report into, so the outcome comes back
/// as another notification. Failures re-post the original — same buttons, same
/// payload — so the decision is still there to retry with.
fn report(app: &AppHandle, context: &ActionContext, action: &Action, outcome: Result<(), String>) {
    match outcome {
        Ok(()) => post(
            app,
            action.done().to_string(),
            context.title.clone(),
            context.payload.clone(),
            None,
            ActionKind::Default,
        ),
        Err(reason) => {
            // The reason quotes the gateway, which can quote user content —
            // same policy as chat_api's error bodies, so it stays at debug.
            log::warn!("[notifications] could not {}", action.verb());
            log::debug!("[notifications] {} failed: {reason}", action.verb());
            let kind = kind_for(context.payload.as_ref());
            post(
                app,
                context.title.clone(),
                action.failed(&reason),
                context.payload.clone(),
                context.user_id.clone(),
                kind,
            )
        }
    }
}

/// The one way a notification reaches the OS, whichever backend is behind it and
/// whether it is an incoming envelope or an action reporting its outcome.
/// `user_id` is the user it is delivered to, which the session gate compares
/// against later; a follow-up carries none, so its buttons cannot act again.
pub(crate) fn post(
    app: &AppHandle,
    title: String,
    body: String,
    click: Option<serde_json::Value>,
    user_id: Option<String>,
    kind: ActionKind,
) {
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        crate::macos_un::post(title, body, click, user_id, kind);
    }
    #[cfg(target_os = "windows")]
    crate::windows_toast::post(app, title, body, click, user_id, kind);
}

/// A string field of the click payload's `context`, if present and non-empty.
fn context_str(click: Option<&serde_json::Value>, key: &str) -> Option<String> {
    string_field(click?.pointer("/context")?, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(context: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "context": context })
    }

    #[test]
    fn actionable_contexts_get_their_buttons() {
        assert_eq!(
            kind_for(Some(&click(serde_json::json!({
                "type": "ADMIN_APPROVAL_REQUEST",
                "approvalRequestId": "req-1",
            })))),
            ActionKind::Approval
        );
        assert_eq!(
            kind_for(Some(&click(serde_json::json!({
                "type": "ADMIN_AI_MESSAGE",
                "dialogId": "dlg-1",
            })))),
            ActionKind::Message
        );
    }

    /// No id, no button: an Approve that cannot resolve anything is worse than
    /// no Approve at all.
    #[test]
    fn contexts_without_the_id_the_action_needs_stay_default() {
        assert_eq!(
            kind_for(Some(&click(
                serde_json::json!({ "type": "ADMIN_APPROVAL_REQUEST", "ticketId": "t-1" })
            ))),
            ActionKind::Default
        );
        assert_eq!(
            kind_for(Some(&click(
                serde_json::json!({ "type": "ADMIN_AI_MESSAGE", "dialogId": "" })
            ))),
            ActionKind::Default
        );
        assert_eq!(
            kind_for(Some(&click(
                serde_json::json!({ "type": "TICKET_ASSIGNED", "ticketId": "t-1" })
            ))),
            ActionKind::Default
        );
        assert_eq!(kind_for(None), ActionKind::Default);
    }

    /// The typed text is the one thing a failed reply cannot reconstruct — the
    /// inline field is cleared by the time the outcome is known.
    #[test]
    fn a_failed_reply_carries_the_text_back() {
        let failed = Action::Reply("ship it".into()).failed("the gateway could not be reached");
        assert!(failed.contains("ship it"));
        assert!(!Action::Approve.failed("nope").contains("Your reply"));
    }
}
