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

use std::collections::VecDeque;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter};

use crate::chat_api;
use crate::notifications::{lock, string_field, Delivery};
use crate::MAIN_LABEL;

/// Emitted after a button completes a write against a conversation, naming it.
/// A background action reaches the gateway over REST from this process, so an
/// open window has no way to know it happened: its own live tail may be down
/// (that is usually why the user is answering from a notification at all), and
/// the reply it just sent would then sit unshown until the tail next recovered.
///
/// Consumed by `onNativeChatDialogChanged` in the frontend's native-shell.ts.
const DIALOG_CHANGED_EVENT: &str = "chat:dialog-changed";

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
///
/// Private: [`live_kind_for`] is what callers outside this module get, because
/// what a payload *could* earn and what it may still be offered are different
/// questions once a decision has been made.
fn kind_for(click: Option<&serde_json::Value>) -> ActionKind {
    if approval_request_id(click).is_some() {
        return ActionKind::Approval;
    }
    if dialog_id(click).is_some() {
        return ActionKind::Message;
    }
    ActionKind::Default
}

/// The approval request a payload is about — the primitive fact the Approval
/// button set is derived from, rather than the other way round. `None` unless
/// the context says it is an approval **and** the id survived the payload
/// projection, because an Approve that can resolve nothing is worse than no
/// Approve at all.
///
/// The single place that rule is written down: which buttons to offer, which
/// toast this replaces, and which request a press resolves are all the same
/// question.
pub(crate) fn approval_request_id(click: Option<&serde_json::Value>) -> Option<String> {
    (context_str(click, "type").as_deref() == Some(APPROVAL_CONTEXT_TYPE))
        .then(|| context_str(click, "approvalRequestId"))
        .flatten()
}

/// The conversation a payload is about, on the same terms as
/// [`approval_request_id`].
fn dialog_id(click: Option<&serde_json::Value>) -> Option<String> {
    (context_str(click, "type").as_deref() == Some(MESSAGE_CONTEXT_TYPE))
        .then(|| context_str(click, "dialogId"))
        .flatten()
}

/// [`kind_for`], minus any decision that is already settled. The gateway
/// republishes a decided approval request under the same notification id, and
/// the shell's own press settles one a moment before that republish lands —
/// either way, offering the decision again would offer one that cannot be made.
pub(crate) fn live_kind_for(click: Option<&serde_json::Value>) -> ActionKind {
    match approval_request_id(click) {
        Some(id) if is_resolved(&id) => {
            log::debug!("[notifications] approval already settled — posting without buttons");
            ActionKind::Default
        }
        _ => kind_for(click),
    }
}

/// Backend `ApprovalResolution`, absent while a request is still pending. Only
/// these three settle it — `PENDING` on the wire is still a decision to make,
/// and the frontend's `resolutionToStatus` draws the line in the same place.
const SETTLED_RESOLUTIONS: [&str; 3] = ["APPROVED", "REJECTED", "CANCELLED"];

/// Record the verdict an envelope carries, if it carries one. The gateway
/// republishes an approval request once it is decided — same notification id,
/// `eventType: "UPDATED"`, the verdict in `context.resolution` — so that every
/// consumer can bring its copy up to date. Taking it at face value is what lets
/// a decision made anywhere (the web UI, another device, another admin) reach
/// the banner sitting in this machine's Action Center.
///
/// Takes a source object carrying the resolution fields — the envelope's
/// `attributes` map (the spec contract) or its legacy `context` object — not
/// the projected click payload the rest of this module reads: `resolution` is
/// not one of the fields that survives the projection. The caller reads both
/// sources; recording is idempotent.
pub(crate) fn note_resolution(context: Option<&serde_json::Value>) {
    let Some(context) = context else { return };
    let settled = string_field(context, "resolution").is_some_and(|resolution| {
        SETTLED_RESOLUTIONS
            .iter()
            .any(|settled| resolution.eq_ignore_ascii_case(settled))
    });
    if !settled {
        return;
    }
    if let Some(request_id) = string_field(context, "approvalRequestId") {
        remember_resolved(&request_id);
    }
}

/// Approval requests known to be settled, oldest first. Bounded because this is
/// a working set and not a record: the authority on whether a request is still
/// pending is the gateway, which says so with a 409. Losing the oldest entries
/// only costs a press that lands there instead.
static RESOLVED: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
const RESOLVED_MEMORY: usize = 64;

/// Taken over the ring rather than the static so the eviction behaviour can be
/// exercised without a test flooding the set every other test shares.
fn remember(resolved: &mut VecDeque<String>, request_id: &str) {
    if resolved.iter().any(|id| id == request_id) {
        return;
    }
    if resolved.len() >= RESOLVED_MEMORY {
        resolved.pop_front();
    }
    resolved.push_back(request_id.to_string());
}

fn remember_resolved(request_id: &str) {
    remember(&mut lock(&RESOLVED), request_id);
}

fn is_resolved(request_id: &str) -> bool {
    lock(&RESOLVED).iter().any(|id| id == request_id)
}

/// Sign-out: the decisions belong to the session that made them.
pub(crate) fn forget_resolved() {
    lock(&RESOLVED).clear();
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
        // Only a press that actually wrote. `AlreadyResolved` changed nothing,
        // and announcing it would cost an open window a consumer rebuild and a
        // refetch to discover exactly what it already had.
        if matches!(outcome, Ok(Outcome::Done)) {
            announce_dialog_change(&app, &context);
        }
        report(&app, &context, &action, outcome);
    });
}

/// Name the conversation a completed press just changed, for whatever window
/// happens to be open. Silent when the payload carries no dialog — a
/// ticket-linked approval is answered on the ticket, which has its own live
/// tail — and cheap when there is no window: `emit_to` on a missing label is
/// not an error worth reporting to the user, who is looking at a notification.
fn announce_dialog_change(app: &AppHandle, context: &ActionContext) {
    // Read raw rather than through [`dialog_id`], whose `ADMIN_AI_MESSAGE` gate
    // exists to decide whether a REPLY has somewhere to go. An approval carries
    // a conversation too, and resolving one changes it just as much — the gate
    // would drop exactly the notification whose dialog most needs refetching.
    let Some(dialog_id) = context_str(context.payload.as_ref(), "dialogId") else {
        return;
    };
    if let Err(e) = app.emit_to(
        MAIN_LABEL,
        DIALOG_CHANGED_EVENT,
        serde_json::json!({ "dialogId": dialog_id }),
    ) {
        log::warn!("emit {DIALOG_CHANGED_EVENT}: {e}");
    }
}

/// What a press amounted to. `AlreadyResolved` is a third thing, neither the
/// success that reports "Approved" nor a failure that offers the decision
/// again: the request is settled, just not by this press.
enum Outcome {
    Done,
    AlreadyResolved,
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
) -> Result<Outcome, String> {
    let session = chat_api::active_session(app)
        .await
        .ok_or("no one is signed in — open OpenFrame and sign in, then try again")?;
    if context.user_id.as_deref() != Some(session.user_id.as_str()) {
        return Err("this notification belongs to a different account".into());
    }

    match action {
        Action::Approve | Action::Reject => {
            let request_id = approval_request_id(context.payload.as_ref())
                .ok_or("this notification carries no approval request")?;
            // A banner that was already on screen when the request was settled
            // keeps its buttons — nothing recalls a toast the user is looking at
            // — so the press still has to be caught here: one round-trip earlier
            // than the gateway's 409, and without spending a write on a request
            // that has no decision left to make.
            if is_resolved(&request_id) {
                return Ok(Outcome::AlreadyResolved);
            }
            let approve = matches!(action, Action::Approve);
            let outcome = chat_api::resolve_approval(app, &session, &request_id, approve).await?;
            // Remembered for either outcome: whoever resolved it, it is settled,
            // and the republished copy must not offer it again.
            remember_resolved(&request_id);
            Ok(match outcome {
                chat_api::ApprovalOutcome::Resolved => Outcome::Done,
                chat_api::ApprovalOutcome::AlreadyResolved => Outcome::AlreadyResolved,
            })
        }
        Action::Reply(text) => {
            let dialog_id = dialog_id(context.payload.as_ref())
                .ok_or("this notification carries no conversation")?;
            chat_api::send_message(app, &session, &dialog_id, text)
                .await
                .map(|()| Outcome::Done)
        }
    }
}

/// A background action has no window to report into, so the outcome comes back
/// as another notification. Failures re-post the original — same buttons, same
/// payload — so the decision is still there to retry with.
fn report(
    app: &AppHandle,
    context: &ActionContext,
    action: &Action,
    outcome: Result<Outcome, String>,
) {
    match outcome {
        // Both carry the original payload, which on Windows is also the toast's
        // identity — so the confirmation replaces the notification it resolved
        // rather than stacking under a decision that is no longer open.
        // Delivered as `New` all the same: the user pressed a button a moment
        // ago and is owed a visible answer, which is exactly the interruption an
        // update is not.
        Ok(outcome) => {
            let title = match outcome {
                Outcome::Done => action.done().to_string(),
                Outcome::AlreadyResolved => {
                    log::info!(
                        "[notifications] nothing to {} — already settled",
                        action.verb()
                    );
                    "Already resolved".to_string()
                }
            };
            post(
                app,
                title,
                context.title.clone(),
                context.payload.clone(),
                None,
                ActionKind::Default,
                Delivery::New,
            )
        }
        Err(reason) => {
            // The reason quotes the gateway, which can quote user content —
            // same policy as chat_api's error bodies, so it stays at debug.
            log::warn!("[notifications] could not {}", action.verb());
            log::debug!("[notifications] {} failed: {reason}", action.verb());
            // The retry is only worth offering while the decision is still open.
            let kind = live_kind_for(context.payload.as_ref());
            post(
                app,
                context.title.clone(),
                action.failed(&reason),
                context.payload.clone(),
                context.user_id.clone(),
                kind,
                Delivery::New,
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
    delivery: Delivery,
) {
    #[cfg(target_os = "macos")]
    {
        // `delivery` is dropped here rather than passed on: each UN request is
        // posted under a fresh identifier, so macOS has nothing to supersede and
        // an update would alert again. See the known gap in
        // docs/auth-and-notifications.md.
        let _ = (app, delivery);
        crate::macos_un::post(title, body, click, user_id, kind);
    }
    #[cfg(target_os = "windows")]
    crate::windows_toast::post(app, title, body, click, user_id, kind, delivery);
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

    fn approval(request_id: &str) -> serde_json::Value {
        click(serde_json::json!({
            "type": "ADMIN_APPROVAL_REQUEST",
            "approvalRequestId": request_id,
        }))
    }

    /// The verdict on the wire is what settles a request, wherever it was made.
    /// `PENDING` is not a verdict — the backend sends it while the decision is
    /// still open, and stripping the buttons then would strand it.
    #[test]
    fn only_a_terminal_resolution_settles_a_request() {
        let pending = serde_json::json!({
            "type": "ADMIN_APPROVAL_REQUEST",
            "approvalRequestId": "wire-pending",
            "resolution": "PENDING",
        });
        note_resolution(Some(&pending));
        assert!(!is_resolved("wire-pending"));

        for (i, resolution) in ["APPROVED", "rejected", "CANCELLED"].iter().enumerate() {
            let id = format!("wire-settled-{i}");
            note_resolution(Some(&serde_json::json!({
                "type": "ADMIN_APPROVAL_REQUEST",
                "approvalRequestId": id,
                "resolution": resolution,
            })));
            assert!(is_resolved(&id), "{resolution} should settle the request");
        }

        // Nothing to record: no verdict, no id, no context at all.
        note_resolution(Some(
            &serde_json::json!({ "approvalRequestId": "wire-bare" }),
        ));
        assert!(!is_resolved("wire-bare"));
        note_resolution(None);
    }

    /// The bug this exists for: the gateway republishes a decided approval
    /// request under the same notification id. It must come without the
    /// decision — pressing it can only be refused.
    #[test]
    fn a_resolved_request_loses_its_buttons() {
        let payload = approval("resolved-req-1");
        assert_eq!(live_kind_for(Some(&payload)), ActionKind::Approval);
        remember_resolved("resolved-req-1");
        assert_eq!(live_kind_for(Some(&payload)), ActionKind::Default);
        // Only that request: a decision still open keeps its buttons.
        assert_eq!(
            live_kind_for(Some(&approval("resolved-req-2"))),
            ActionKind::Approval
        );
        // And the contract for what the payload *could* earn is unchanged.
        assert_eq!(kind_for(Some(&payload)), ActionKind::Approval);
    }

    /// Bounded on purpose — but the window has to be deep enough that an
    /// approval decided a moment ago is still in it.
    ///
    /// Runs against its own ring: flooding the shared one would evict what the
    /// other tests just recorded, and they run in parallel.
    #[test]
    fn the_resolved_window_forgets_oldest_first() {
        let mut ring = VecDeque::new();
        for i in 0..RESOLVED_MEMORY + 8 {
            remember(&mut ring, &format!("evicted-{i}"));
        }
        assert_eq!(ring.len(), RESOLVED_MEMORY);
        assert!(!ring.iter().any(|id| id == "evicted-0"));
        assert!(ring
            .iter()
            .any(|id| *id == format!("evicted-{}", RESOLVED_MEMORY + 7)));
        // Repeats do not consume the window.
        remember(&mut ring, "evicted-70");
        assert_eq!(ring.len(), RESOLVED_MEMORY);
    }

    /// Two notifications share a banner only when the later one supersedes the
    /// earlier, which is what having a request id means. Messages are not
    /// republished and must keep stacking — replacing one would hide a message
    /// the user has not read.
    #[test]
    fn only_approvals_have_an_identity_to_supersede() {
        assert_eq!(
            approval_request_id(Some(&approval("req-1"))).as_deref(),
            Some("req-1")
        );
        assert!(approval_request_id(Some(&click(serde_json::json!({
            "type": "ADMIN_AI_MESSAGE", "dialogId": "dlg-1",
        }))))
        .is_none());
        assert!(approval_request_id(Some(&click(
            serde_json::json!({ "type": "TICKET_ASSIGNED", "ticketId": "t-1" })
        )))
        .is_none());
        assert!(approval_request_id(None).is_none());
        // The id an approval carries is the id a press resolves: a payload whose
        // id did not survive the projection has neither.
        assert!(approval_request_id(Some(&click(
            serde_json::json!({ "type": "ADMIN_APPROVAL_REQUEST", "ticketId": "t-1" })
        )))
        .is_none());
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
