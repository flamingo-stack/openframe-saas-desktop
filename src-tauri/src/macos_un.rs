// `UNUserNotificationCenter` backend for bundled (production) macOS builds.
//
// Chosen over the deprecated `NSUserNotificationCenter` path (notify-rust)
// because UN delivers `didReceiveNotificationResponse` for every click — live
// banner, Notification Center hours later, or a click that launches the app
// cold — while the legacy path only reported clicks on a live banner.
//
// The click payload rides in the notification's `userInfo` (as the ready-made
// `notification:click` JSON), so it survives process restarts and no
// in-process id→target registry is needed.
//
// Notifications that carry an actionable context get buttons that complete the
// work without opening a window: Approve/Reject on an AI tool-approval request,
// inline Reply on a Mingo message. Those run as REST calls in `chat_api`, under
// the session gate in `run_action`.
//
// UN APIs abort in processes without a bundle identifier, so `init` refuses to
// activate for dev builds (`tauri dev` runs a bare binary) — those have no OS
// notifications.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread};
use objc2_foundation::{
    NSArray, NSBundle, NSDictionary, NSError, NSObject, NSObjectProtocol, NSSet, NSString,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
    UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNTextInputNotificationAction, UNTextInputNotificationResponse, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};
use tauri::AppHandle;

use crate::chat_api;
use crate::notifications::deliver_click;

/// Keeps its original id: notifications already sitting in Notification Center
/// resolve their category by identifier, and a rename would strip their button.
const CATEGORY_DEFAULT: &str = "openframe-desktop-notification";
const CATEGORY_APPROVAL: &str = "openframe-desktop-approval";
const CATEGORY_MESSAGE: &str = "openframe-desktop-message";

const OPEN_ACTION_ID: &str = "open";
const APPROVE_ACTION_ID: &str = "approve";
const REJECT_ACTION_ID: &str = "reject";
const REPLY_ACTION_ID: &str = "reply";

/// Envelope `context.type` values that earn action buttons. The rest of the set
/// (tickets, client chats) has no action the shell can complete on its own.
const APPROVAL_CONTEXT_TYPE: &str = "ADMIN_APPROVAL_REQUEST";
const MESSAGE_CONTEXT_TYPE: &str = "ADMIN_AI_MESSAGE";

const PAYLOAD_KEY: &str = "of-click-payload";
/// The user the notification was delivered to, stamped at fire time. An action
/// taken later is refused unless the session still belongs to them.
const USER_KEY: &str = "of-user-id";

/// The delegate has no state of its own; clicks are routed back into the shell
/// through this slot. Set once at `init`, before the delegate is installed.
static ROUTER: OnceLock<AppHandle> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Install the delegate and register the action categories. Called from
/// `notifications::init` during Tauri setup — early enough that a cold-start
/// click's response (delivered right after launch) finds the delegate.
pub fn init(app: &AppHandle) {
    if tauri::is_dev() {
        return;
    }
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        log::warn!("[notifications] no bundle identifier — OS notifications disabled");
        return;
    }
    if ROUTER.set(app.clone()).is_err() {
        return;
    }

    static DELEGATE: OnceLock<Retained<NotificationDelegate>> = OnceLock::new();
    DELEGATE.get_or_init(|| {
        let delegate = NotificationDelegate::new();
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        register_categories(&center);
        delegate
    });

    ACTIVE.store(true, Ordering::Release);
    log::info!("[notifications] UNUserNotificationCenter backend active");
}

/// Ask macOS for notification permission once. Denial is not an error —
/// notifications just won't display, which the user controls in System
/// Settings.
pub fn ensure_authorized() {
    if !active() {
        return;
    }
    static REQUESTED: AtomicBool = AtomicBool::new(false);
    if REQUESTED.swap(true, Ordering::AcqRel) {
        return;
    }
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &RcBlock::new(|granted: Bool, error: *mut NSError| {
                if let Some(error) = NonNull::new(error).map(|p| unsafe { p.as_ref() }) {
                    log::warn!(
                        "[notifications] authorization failed: {}",
                        error.localizedDescription()
                    );
                } else {
                    log::info!(
                        "[notifications] authorization granted: {}",
                        granted.as_bool()
                    );
                }
            }),
        );
}

/// Fire an incoming notification. `user_id` is the subscription's user, stamped
/// into `userInfo` so a button pressed hours later can check it still matches
/// the signed-in session.
pub fn fire(title: String, body: String, click: Option<serde_json::Value>, user_id: String) {
    let category = category_for(click.as_ref());
    post(title, body, click, Some(user_id), category.to_string());
}

fn post(
    title: String,
    body: String,
    click: Option<serde_json::Value>,
    user_id: Option<String>,
    category: String,
) {
    if !active() {
        log::debug!("[notifications] UN backend inactive (dev/unbundled build) — dropping");
        return;
    }
    // UNUserNotificationCenter is thread-safe; keep the ObjC work off the NATS
    // router task all the same.
    std::thread::spawn(move || {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&title));
        if !body.is_empty() {
            content.setBody(&NSString::from_str(&body));
        }
        content.setCategoryIdentifier(&NSString::from_str(&category));

        let entries = [
            (PAYLOAD_KEY, click.as_ref().map(|click| click.to_string())),
            (USER_KEY, user_id),
        ];
        let entries: Vec<(Retained<NSString>, Retained<NSString>)> = entries
            .into_iter()
            .filter_map(|(key, value)| Some((NSString::from_str(key), NSString::from_str(&value?))))
            .collect();
        if !entries.is_empty() {
            let keys: Vec<&NSString> = entries.iter().map(|(key, _)| &**key).collect();
            let values: Vec<&NSString> = entries.iter().map(|(_, value)| &**value).collect();
            let dict = NSDictionary::from_slices(&keys, &values);
            // Erase the generics: setUserInfo takes an untyped NSDictionary.
            // Layout-identical (generics on objc2 collections are phantom);
            // NSStrings satisfy the plist requirement.
            let erased: &NSDictionary =
                unsafe { &*(Retained::as_ptr(&dict) as *const NSDictionary) };
            unsafe { content.setUserInfo(erased) };
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&request_id),
            &content,
            None,
        );

        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(
                &request,
                Some(&RcBlock::new(move |error: *mut NSError| {
                    if let Some(error) = NonNull::new(error).map(|p| unsafe { p.as_ref() }) {
                        log::warn!(
                            "[notifications] request rejected: {}",
                            error.localizedDescription()
                        );
                    } else {
                        log::info!("[notifications] fired: {title}");
                    }
                })),
            );
    });
}

/// Which button set a notification gets. Derived from the click payload rather
/// than passed in, so cross-platform code never has to know about macOS
/// category ids. Buttons are only offered when the id the action needs actually
/// survived the payload projection.
fn category_for(click: Option<&serde_json::Value>) -> &'static str {
    match context_str(click, "type").as_deref() {
        Some(APPROVAL_CONTEXT_TYPE) if context_str(click, "approvalRequestId").is_some() => {
            CATEGORY_APPROVAL
        }
        Some(MESSAGE_CONTEXT_TYPE) if context_str(click, "dialogId").is_some() => CATEGORY_MESSAGE,
        _ => CATEGORY_DEFAULT,
    }
}

/// `setNotificationCategories` replaces the whole set, so all three are
/// registered in one call at init.
///
/// macOS shows the first two actions inline and hides the rest behind the
/// chevron, hence the ordering: the decision first, "Open" last.
fn register_categories(center: &UNUserNotificationCenter) {
    // Foreground: clicking "Open" activates the app (a body click does so
    // inherently). No CustomDismissAction on any category — it would make macOS
    // relaunch the app just to report a dismissal after the user quit it.
    let open = || {
        UNNotificationAction::actionWithIdentifier_title_options(
            &NSString::from_str(OPEN_ACTION_ID),
            &NSString::from_str("Open"),
            UNNotificationActionOptions::Foreground,
        )
    };
    // AuthenticationRequired is the device-lock gate only; the gate that
    // matters — that a signed-in session still owns this notification — is
    // enforced in `run_action`, because macOS has no notion of our session.
    let approve = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(APPROVE_ACTION_ID),
        &NSString::from_str("Approve"),
        UNNotificationActionOptions::AuthenticationRequired,
    );
    // Destructive (red) and one click: denying a tool execution is the fail-safe
    // direction, so it must not be harder than approving.
    let reject = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(REJECT_ACTION_ID),
        &NSString::from_str("Reject"),
        UNNotificationActionOptions::Destructive,
    );
    let reply = UNTextInputNotificationAction::actionWithIdentifier_title_options_textInputButtonTitle_textInputPlaceholder(
        &NSString::from_str(REPLY_ACTION_ID),
        &NSString::from_str("Reply"),
        UNNotificationActionOptions::empty(),
        &NSString::from_str("Send"),
        &NSString::from_str("Reply to Mingo"),
    );

    let category = |id: &str, actions: &[Retained<UNNotificationAction>]| {
        UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
            &NSString::from_str(id),
            &NSArray::from_retained_slice(actions),
            &NSArray::new(),
            UNNotificationCategoryOptions::empty(),
        )
    };
    center.setNotificationCategories(&NSSet::from_retained_slice(&[
        category(CATEGORY_DEFAULT, &[open()]),
        category(CATEGORY_APPROVAL, &[approve, reject, open()]),
        category(CATEGORY_MESSAGE, &[reply.into_super(), open()]),
    ]));
}

// ---------------------------------------------------------------------------
// Response routing
// ---------------------------------------------------------------------------

enum Action {
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

/// Everything a background action needs, lifted out of the ObjC response before
/// the async work starts — none of it can cross a thread boundary otherwise.
/// Keeping the original title and category also lets a failure re-post the
/// notification still actionable, so the decision is not lost.
struct ActionContext {
    title: String,
    category: String,
    payload: Option<serde_json::Value>,
    user_id: Option<String>,
}

impl ActionContext {
    fn from_response(response: &UNNotificationResponse) -> Self {
        let content = response.notification().request().content();
        let user_info = content.userInfo();
        Self {
            title: content.title().to_string(),
            category: content.categoryIdentifier().to_string(),
            payload: user_info_str(&user_info, PAYLOAD_KEY)
                .and_then(|json| serde_json::from_str(&json).ok()),
            user_id: user_info_str(&user_info, USER_KEY),
        }
    }
}

fn handle_response(app: &AppHandle, action_id: &str, response: &UNNotificationResponse) {
    let context = ActionContext::from_response(response);
    let action = match action_id {
        APPROVE_ACTION_ID => Action::Approve,
        REJECT_ACTION_ID => Action::Reject,
        REPLY_ACTION_ID => {
            let text = response
                .downcast_ref::<UNTextInputNotificationResponse>()
                .map(|response| response.userText().to_string())
                .unwrap_or_default();
            let text = text.trim().to_string();
            if text.is_empty() {
                // Nothing was typed — not a failure worth a banner.
                return;
            }
            Action::Reply(text)
        }
        // The notification body, "Open", and anything unrecognized (a category
        // from an older build): raise the window, today's behaviour.
        _ => {
            deliver_click(app, context.payload);
            return;
        }
    };

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        report(&context, &action, run_action(&app, &context, &action).await);
    });
}

/// The session gate. A notification sitting in Notification Center is not
/// authority to act on anything: the shell acts only while the user it was
/// delivered to is still signed in, so an approval cannot be granted from a
/// signed-out app, and a stale notification cannot act under whoever signed in
/// after (the notification plane keeps running across a user switch on the same
/// machine).
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
/// as another notification. Failures re-post the original — same category, same
/// payload — so the buttons are still there to retry with.
fn report(context: &ActionContext, action: &Action, outcome: Result<(), String>) {
    match outcome {
        Ok(()) => post(
            action.done().to_string(),
            context.title.clone(),
            context.payload.clone(),
            None,
            CATEGORY_DEFAULT.to_string(),
        ),
        Err(reason) => {
            log::warn!("[notifications] could not {}: {reason}", action.verb());
            post(
                context.title.clone(),
                action.failed(&reason),
                context.payload.clone(),
                context.user_id.clone(),
                context.category.clone(),
            )
        }
    }
}

/// A string field of the click payload's `context`, if present and non-empty.
fn context_str(click: Option<&serde_json::Value>, key: &str) -> Option<String> {
    click
        .and_then(|click| click.pointer("/context")?.get(key))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn user_info_str(user_info: &NSDictionary, key: &str) -> Option<String> {
    let key = NSString::from_str(key);
    let key: &AnyObject = &key;
    let value = user_info.objectForKey(key)?;
    Some(value.downcast_ref::<NSString>()?.to_string())
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OpenFrameDesktopNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        /// Without this, macOS silently drops notifications while the app is
        /// frontmost. Incoming ones are already gated on window focus by
        /// `should_notify`; an action's follow-up deliberately is not — it
        /// answers something the user just did. Banner only — the pre-UN path
        /// was silent.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present_notification(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner,));
        }

        /// A response is a body click, an action button, or a submitted inline
        /// reply: dismiss events are only reported with CustomDismissAction,
        /// which no category sets.
        ///
        /// The completion handler is called before the background actions
        /// finish, because it cannot be moved onto another thread (blocks are
        /// not `Send`) and this runs on the main thread. That is safe here: the
        /// shell is a normal app with a live event loop, not an extension the
        /// system tears down on completion.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive_response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &DynBlock<dyn Fn()>,
        ) {
            let action_id = response.actionIdentifier().to_string();
            log::info!("[notifications] response (action={action_id})");
            if let Some(app) = ROUTER.get() {
                handle_response(app, &action_id, response);
            }
            completion_handler.call(());
        }
    }
);

impl NotificationDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(context: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "context": context })
    }

    #[test]
    fn actionable_contexts_get_their_category() {
        assert_eq!(
            category_for(Some(&click(serde_json::json!({
                "type": "ADMIN_APPROVAL_REQUEST",
                "approvalRequestId": "req-1",
            })))),
            CATEGORY_APPROVAL
        );
        assert_eq!(
            category_for(Some(&click(serde_json::json!({
                "type": "ADMIN_AI_MESSAGE",
                "dialogId": "dlg-1",
            })))),
            CATEGORY_MESSAGE
        );
    }

    /// No id, no button: an Approve that cannot resolve anything is worse than
    /// no Approve at all.
    #[test]
    fn contexts_without_the_id_the_action_needs_stay_default() {
        assert_eq!(
            category_for(Some(&click(
                serde_json::json!({ "type": "ADMIN_APPROVAL_REQUEST", "ticketId": "t-1" })
            ))),
            CATEGORY_DEFAULT
        );
        assert_eq!(
            category_for(Some(&click(
                serde_json::json!({ "type": "ADMIN_AI_MESSAGE", "dialogId": "" })
            ))),
            CATEGORY_DEFAULT
        );
        assert_eq!(
            category_for(Some(&click(
                serde_json::json!({ "type": "TICKET_ASSIGNED", "ticketId": "t-1" })
            ))),
            CATEGORY_DEFAULT
        );
        assert_eq!(category_for(None), CATEGORY_DEFAULT);
    }
}
