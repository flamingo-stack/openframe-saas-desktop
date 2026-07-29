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
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use tauri::AppHandle;

use crate::notifications::deliver_click;

const CATEGORY_ID: &str = "openframe-desktop-notification";
const OPEN_ACTION_ID: &str = "open";
const PAYLOAD_KEY: &str = "of-click-payload";

/// The delegate has no state of its own; clicks are routed back into the shell
/// through this slot. Set once at `init`, before the delegate is installed.
static ROUTER: OnceLock<AppHandle> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Install the delegate and register the action category. Called from
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
        register_category(&center);
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

pub fn fire(title: String, body: String, click: Option<serde_json::Value>) {
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
        content.setCategoryIdentifier(&NSString::from_str(CATEGORY_ID));

        if let Some(click) = &click {
            let key = NSString::from_str(PAYLOAD_KEY);
            let value = NSString::from_str(&click.to_string());
            let dict = NSDictionary::from_slices(&[&*key], &[&*value]);
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

fn register_category(center: &UNUserNotificationCenter) {
    // Foreground: clicking "Open" activates the app (a body click does so
    // inherently). No CustomDismissAction — it would make macOS relaunch the
    // app just to report a dismissal after the user quit it.
    let action = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(OPEN_ACTION_ID),
        &NSString::from_str("Open"),
        UNNotificationActionOptions::Foreground,
    );
    let category = UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(CATEGORY_ID),
        &NSArray::from_retained_slice(&[action]),
        &NSArray::new(),
        UNNotificationCategoryOptions::empty(),
    );
    center.setNotificationCategories(&NSSet::from_retained_slice(&[category]));
}

fn payload_from_response(response: &UNNotificationResponse) -> Option<serde_json::Value> {
    let user_info = response.notification().request().content().userInfo();
    let key = NSString::from_str(PAYLOAD_KEY);
    let key: &AnyObject = &key;
    let value = user_info.objectForKey(key)?;
    let json = value.downcast_ref::<NSString>()?.to_string();
    serde_json::from_str(&json).ok()
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OpenFrameDesktopNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        /// Without this, macOS silently drops notifications while the app is
        /// frontmost. `should_notify` already gates on window focus, so
        /// anything reaching this point should display. Banner only — the
        /// pre-UN path was silent.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present_notification(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner,));
        }

        /// Every response is a user click (notification body or the Open
        /// button): dismiss events are only reported with CustomDismissAction,
        /// which the category deliberately does not set.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive_response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &DynBlock<dyn Fn()>,
        ) {
            log::info!(
                "[notifications] response (action={})",
                response.actionIdentifier()
            );
            if let Some(app) = ROUTER.get() {
                deliver_click(app, payload_from_response(response));
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
