// COM activator for the Windows toast action buttons.
//
// A toast's inline text box, and any button that must not foreground the app,
// are only reachable through a COM activator: protocol activation — which still
// carries the body click, see `windows_toast` — delivers neither user input nor
// an in-process call. So the buttons declare `activationType="background"`,
// Windows resolves them to the CLSID registered against our AUMID, and the press
// arrives at `Activate` below: in the running process when there is one, in a
// process COM starts (`-ToastActivated` in argv) when the app was quit.
//
// Registration is HKCU-only and rewritten on every launch, like
// `crate::register_url_scheme`, so the CLSID always names the current exe:
//
//   Software\Classes\CLSID\{CLSID}\LocalServer32            = "<exe>" -ToastActivated
//   Software\Classes\AppUserModelId\<AUMID>\CustomActivator = {CLSID}
//
// The installer already stamps the AUMID itself onto the Start Menu shortcut
// (tauri's NSIS `SetLnkAppUserModelId` writes the bundle identifier, which is
// what toasts are posted under) but has no property for the activator. The
// AppUserModelId key supplies it, and doubles as the AUMID registration for dev
// builds, which have no shortcut at all.

use std::ffi::c_void;
use std::sync::OnceLock;

use tauri::AppHandle;
use windows::core::{implement, Interface, Ref, Result as ComResult, BOOL, GUID, PCWSTR};
use windows::Win32::Foundation::CLASS_E_NOAGGREGATION;
use windows::Win32::System::Com::{
    CoInitializeEx, CoRegisterClassObject, IClassFactory, IClassFactory_Impl, CLSCTX_LOCAL_SERVER,
    COINIT_MULTITHREADED, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Notifications::{
    INotificationActivationCallback, INotificationActivationCallback_Impl,
    NOTIFICATION_USER_INPUT_DATA,
};

use crate::windows_toast::{self, Press};

/// Identifies this build's activator to the notification platform. Changing it
/// orphans the buttons on every toast already sitting in the Action Center —
/// they resolve their activator by CLSID, and an unregistered one does nothing.
const ACTIVATOR_CLSID: GUID = GUID::from_u128(0x912d80e7_8dc8_4d6a_b9e8_83f8325024a4);

/// Appended to the registered `LocalServer32` command, so a launch that exists
/// only to serve an activation can be told from one the user asked for — the
/// former must not open a window.
const TOAST_ACTIVATED_ARG: &str = "-ToastActivated";

/// Whether this process was started by COM to serve a button press rather than
/// by the user. Read in two places — setup, which then opens no window, and the
/// single-instance callback, which then raises none.
pub(crate) fn is_activation_launch(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == TOAST_ACTIVATED_ARG)
}

/// The activator has no state of its own; presses are routed back into the shell
/// through this slot, which is filled before the class object is registered so a
/// cold activation cannot arrive with nowhere to go.
static ROUTER: OnceLock<AppHandle> = OnceLock::new();

/// Called from `notifications::init` during Tauri setup, and only from the
/// instance that survived the single-instance check — a second process registers
/// nothing, so COM never routes an activation into one that is about to exit.
pub(crate) fn init(app: &AppHandle) {
    if ROUTER.set(app.clone()).is_err() {
        return;
    }
    register(app);
    serve();
}

fn register(app: &AppHandle) {
    use winreg::{enums::*, RegKey};

    let Ok(exe) = std::env::current_exe() else {
        log::warn!("[notifications] toast activator: current_exe unavailable — no action buttons");
        return;
    };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let clsid = clsid_string();
    // Lays the file down on first call, so it happens here rather than inside
    // the registry chain below — that chain is otherwise pure registry writes.
    let logo = windows_toast::ensure_logo(app);

    // Both of these are load-bearing: without the command COM cannot start us to
    // serve a press, and without CustomActivator the buttons resolve no activator
    // at all. A half-written registration is silent — every button simply does
    // nothing — so it must not be reported as success.
    let command = format!("\"{}\" {TOAST_ACTIVATED_ARG}", exe.display());
    let server = format!(r"Software\Classes\CLSID\{clsid}\LocalServer32");
    let config = app.config();
    let aumid = format!(r"Software\Classes\AppUserModelId\{}", config.identifier);
    let written = hkcu
        .create_subkey(&server)
        .and_then(|(key, _)| key.set_value("", &command))
        .and_then(|()| hkcu.create_subkey(&aumid))
        .and_then(|(key, _)| {
            // The notification's identity: the name beside the header icon, and
            // the header icon itself. Cosmetic, so neither is allowed to fail the
            // registration that makes the buttons work.
            //
            // A shortcut carrying the AUMID is not enough for the icon even when
            // it has the right one — verified on an MSI install whose Start Menu
            // shortcut points at the app's own `icon.ico` and whose toasts came
            // up blank until `IconUri` was set.
            let display_name = config.product_name.as_deref().unwrap_or("OpenFrame");
            let _ = key.set_value("DisplayName", &display_name);
            if let Some(logo) = logo {
                let _ = key.set_value("IconUri", &logo.display().to_string());
            }
            key.set_value("CustomActivator", &clsid)
        });
    match written {
        Ok(()) => log::info!("[notifications] toast activator registered as {clsid}"),
        Err(err) => {
            log::warn!("[notifications] toast activator not registered ({err}) — no action buttons")
        }
    }
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, the only shape the registry accepts.
/// Formatted from the GUID rather than kept as a second literal, so the value COM
/// resolves and the value the registry advertises cannot drift apart — and by
/// hand rather than through `Debug`, which happens to print this shape but does
/// not promise to keep printing it for a value we persist.
fn clsid_string() -> String {
    let g = ACTIVATOR_CLSID;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7],
    )
}

fn serve() {
    std::thread::spawn(|| {
        // MTA, so the activation arrives on an RPC pool thread and this one has
        // no message pump to run — an STA registration would need one, and the
        // only thread with a pump here is Tauri's event loop, which must not
        // block on a REST call.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let factory: IClassFactory = ActivatorFactory.into();
        match unsafe {
            CoRegisterClassObject(
                &ACTIVATOR_CLSID,
                &factory,
                CLSCTX_LOCAL_SERVER,
                REGCLS_MULTIPLEUSE,
            )
        } {
            Ok(_) => log::info!("[notifications] toast activator serving"),
            Err(err) => log::warn!("[notifications] toast activator not served: {err}"),
        }
        // The apartment must outlive the registration, and it ends when the last
        // thread that initialized it goes away — so this one stays.
        loop {
            std::thread::park();
        }
    });
}

// ---------------------------------------------------------------------------
// Activation routing
// ---------------------------------------------------------------------------

fn handle_activation(app: &AppHandle, args: &str, inputs: &[(String, String)]) {
    let Some(activation) = windows_toast::parse_action_args(args) else {
        // The body and "Open" activate the URI, never this — so an argument
        // string with no action in it came from a build that encoded its
        // buttons differently. Falling back to the window keeps the press from
        // doing nothing at all.
        log::warn!("[notifications] activation named no known action — opening window");
        open_window(app);
        return;
    };

    let action = match windows_toast::press_for(&activation, inputs) {
        Press::Run(action) => action,
        Press::Ignore => return,
        Press::Unknown => {
            log::warn!(
                "[notifications] unknown toast action '{}' — opening window",
                activation.action
            );
            open_window(app);
            return;
        }
    };

    crate::notification_actions::spawn(app, activation.context, action);
}

/// Deferred, because this runs on the COM thread the notification platform is
/// waiting on: raising or building a window blocks on Tauri's event loop, which
/// during a cold activation has not started dispatching yet.
fn open_window(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        crate::raise_or_open_main_window(&app);
    });
}

#[implement(INotificationActivationCallback)]
struct Activator;

impl INotificationActivationCallback_Impl for Activator_Impl {
    /// Runs on an RPC thread the notification platform is waiting on, so it only
    /// decodes and hands off: the REST call behind the press happens on Tauri's
    /// async runtime (`notification_actions::spawn`).
    fn Activate(
        &self,
        _app_id: &PCWSTR,
        invoked_args: &PCWSTR,
        data: *const NOTIFICATION_USER_INPUT_DATA,
        count: u32,
    ) -> ComResult<()> {
        // Both pointers are guarded rather than trusted: this is the one entry
        // point the shell does not call itself, and `PCWSTR::to_string` walks a
        // null pointer looking for its terminator.
        let args = if invoked_args.is_null() {
            String::new()
        } else {
            unsafe { invoked_args.to_string() }.unwrap_or_default()
        };
        let inputs = unsafe { user_inputs(data, count) };
        log::info!("[notifications] toast activation ({} inputs)", inputs.len());
        if let Some(app) = ROUTER.get() {
            handle_activation(app, &args, &inputs);
        }
        Ok(())
    }
}

/// # Safety
/// `data` must either be null or point at `count` initialized entries whose
/// strings — themselves non-null, unlike `data` — outlive the call. Null `data`
/// is in contract, since the platform passes it for a toast with no inputs, and
/// is checked because `from_raw_parts(null, 0)` is still undefined behaviour.
unsafe fn user_inputs(
    data: *const NOTIFICATION_USER_INPUT_DATA,
    count: u32,
) -> Vec<(String, String)> {
    if data.is_null() {
        return Vec::new();
    }
    std::slice::from_raw_parts(data, count as usize)
        .iter()
        .filter_map(|entry| Some((entry.Key.to_string().ok()?, entry.Value.to_string().ok()?)))
        .collect()
}

#[implement(IClassFactory)]
struct ActivatorFactory;

impl IClassFactory_Impl for ActivatorFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, windows::core::IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> ComResult<()> {
        if !outer.is_null() {
            // COM requires the out-pointer to be cleared on every failure path;
            // the `query` below does it for itself.
            unsafe { *object = std::ptr::null_mut() };
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activator: INotificationActivationCallback = Activator.into();
        unsafe { activator.query(iid, object).ok() }
    }

    /// Nothing to hold: the server is the app, and it outlives every activation
    /// it serves.
    fn LockServer(&self, _lock: BOOL) -> ComResult<()> {
        Ok(())
    }
}
