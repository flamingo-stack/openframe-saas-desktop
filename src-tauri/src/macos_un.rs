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
// inline Reply on a Mingo message. What this file owns of that is the macOS
// half — categories, the delegate, and decoding a response; the decision itself
// and its session gate live in `notification_actions`, shared with Windows.
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

use crate::{
    notification_actions::{Action, ActionContext, ActionKind},
    notifications::deliver_click,
};

/// Renamed with the app, which is safe only because the bundle identifier was
/// renamed in the same change: notifications already in the Notification Center
/// belong to `com.openframe.desktop` and are another app's as far as the OS is
/// concerned, so there is nothing left whose category id these could fail to
/// match. Renaming these alone would strip the buttons off every delivered
/// notification.
const CATEGORY_DEFAULT: &str = "openframe-console-notification";
const CATEGORY_APPROVAL: &str = "openframe-console-approval";
const CATEGORY_MESSAGE: &str = "openframe-console-message";

const OPEN_ACTION_ID: &str = "open";
const APPROVE_ACTION_ID: &str = "approve";
const REJECT_ACTION_ID: &str = "reject";
const REPLY_ACTION_ID: &str = "reply";

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
pub(crate) fn init(app: &AppHandle) {
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
pub(crate) fn ensure_authorized() {
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

/// Post a notification. `user_id` is the user it is delivered to, stamped into
/// `userInfo` so a button pressed hours later can check it still matches the
/// signed-in session; a follow-up that reports an action's outcome carries none.
pub(crate) fn post(
    title: String,
    body: String,
    click: Option<serde_json::Value>,
    user_id: Option<String>,
    kind: ActionKind,
) {
    let category = category_for(kind).to_string();
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

fn category_for(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Approval => CATEGORY_APPROVAL,
        ActionKind::Message => CATEGORY_MESSAGE,
        ActionKind::Default => CATEGORY_DEFAULT,
    }
}

/// `setNotificationCategories` replaces the whole set, so all three are
/// registered in one call at init.
///
/// **How many buttons the user sees is macOS's call, not ours.** A banner folds
/// every action behind the "Options" chevron regardless of count — two actions
/// collapse just like three (verified on 15.x), and no `UNNotificationAction`
/// or category option overrides it. Only the Alerts notification style, which
/// the user chooses in System Settings, renders actions as buttons.
///
/// What the app does control is what is in that menu, so the approval category
/// carries the decision and nothing else: "Open" is left off there, since a
/// click on the notification body already activates the app. The other two
/// categories keep it — with nothing to decide, it is the only action they
/// offer.
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
    // enforced in `notification_actions::run_action`, because macOS has no
    // notion of our session.
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
        category(CATEGORY_APPROVAL, &[approve, reject]),
        category(CATEGORY_MESSAGE, &[reply.into_super(), open()]),
    ]));
}

// ---------------------------------------------------------------------------
// Response routing
// ---------------------------------------------------------------------------

/// Everything a background action needs, lifted out of the ObjC response before
/// the async work starts — none of it can cross a thread boundary otherwise.
/// The category identifier is deliberately not read back: the button set a
/// re-post needs is derived from the payload, which is where the category came
/// from at fire time.
fn action_context(response: &UNNotificationResponse) -> ActionContext {
    let content = response.notification().request().content();
    let user_info = content.userInfo();
    ActionContext {
        title: content.title().to_string(),
        payload: user_info_str(&user_info, PAYLOAD_KEY)
            .and_then(|json| serde_json::from_str(&json).ok()),
        user_id: user_info_str(&user_info, USER_KEY),
    }
}

fn handle_response(app: &AppHandle, action_id: &str, response: &UNNotificationResponse) {
    let context = action_context(response);
    let action = match action_id {
        APPROVE_ACTION_ID => Action::Approve,
        REJECT_ACTION_ID => Action::Reject,
        REPLY_ACTION_ID => {
            // A reply must arrive as the text-input subclass; anything else is
            // macOS behaving unexpectedly, and silently dropping the user's
            // words — the field is already cleared — would leave no trace.
            let Some(response) = response.downcast_ref::<UNTextInputNotificationResponse>() else {
                log::warn!("[notifications] reply response carried no text input — dropping");
                return;
            };
            let text = response.userText().to_string().trim().to_string();
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

    crate::notification_actions::spawn(app, context, action);
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
        /// answers something the user just did. No Sound option: the pre-UN
        /// path was silent.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present_notification(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // List as well as Banner: an action's follow-up must survive in
            // Notification Center, because a failed reply's banner is the only
            // place the typed text still exists once macOS clears the field.
            let options =
                UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List;
            completion_handler.call((options,));
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
