// Sleep/wake is the one interruption the session cannot ride out on its own
// schedule. Everything else in the shell is self-healing — the refresh poll
// retries, async-nats reconnects — but a machine that slept through the access
// token's whole life comes back with every overdue timer in the process firing
// at once, before Wi-Fi has reassociated. The webview's timers are in that
// stampede, and its first 401 drives the refresh-or-log-out path.
//
// So the shell refreshes the moment macOS says the machine woke, rather than
// leaving it to whichever timer lands first.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSNotification, NSString};
use tauri::AppHandle;

/// Named by string rather than through the generated binding so this file needs
/// nothing from AppKit but the one class it calls.
const DID_WAKE: &str = "NSWorkspaceDidWakeNotification";

pub(crate) fn observe(app: AppHandle) {
    let handler = RcBlock::new(move |_notification: NonNull<NSNotification>| {
        crate::tokens::refresh_soon(&app, "system woke");
    });
    // NSWorkspace posts wake on its OWN notification center; the default
    // NSNotificationCenter never sees it.
    let observer = unsafe {
        NSWorkspace::sharedWorkspace()
            .notificationCenter()
            .addObserverForName_object_queue_usingBlock(
                Some(&NSString::from_str(DID_WAKE)),
                None,
                None,
                &handler,
            )
    };
    // The observer token is what keeps the registration alive, and this one is
    // never removed — it has to outlive `observe`, and the process ends with it.
    std::mem::forget(observer);
    log::info!("[wake] observing NSWorkspaceDidWakeNotification");
}
