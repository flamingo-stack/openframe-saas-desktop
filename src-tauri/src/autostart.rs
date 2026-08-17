//! Start at login.
//!
//! Registration is per-user and carries [`FLAG`], which is the only way this
//! process learns it was started by the OS rather than by the user — and so the
//! only way it knows to stay in the tray instead of opening a window. Everything
//! the app does while it sits there (tray, token refresh, NATS, notifications)
//! is already wired independently of any window, so a login start needs only two
//! things from the shell: not building one, and — on macOS — an Accessory
//! activation policy, without which it would sit in the dock with nothing to
//! show.
//!
//! Written by hand rather than through `tauri-plugin-autostart`, which was the
//! first choice until its source was read. It hands `auto-launch` an unquoted
//! exe path, and this product installs to `…\OpenFrame Desktop\OpenFrame
//! Desktop.exe` — Explorer runs Run entries through `CreateProcessW(NULL, …)`,
//! which walks space-delimited prefixes, so anything that ever dropped a
//! `%LOCALAPPDATA%\OpenFrame.exe` would own every login. Its macOS agent also
//! carries no `AssociatedBundleIdentifiers` key, without which the Login Items
//! entry is an unattributed background item rather than this app. Both backends
//! below are a few lines each over dependencies the crate already has.
//!
//! **The user can overrule a managed install, and that is reported rather than
//! fought.** Windows keeps its own switch in `StartupApproved\Run` (Task
//! Manager → Startup) and macOS keeps one in a background-item database with no
//! public API at all. So [`status`] can return `enabled: false` alongside
//! `enforced: true`, and [`decide`] deliberately leaves that state alone — the
//! alternative is rewriting the same registry value at every boot forever
//! without changing the outcome. Only an explicit request from the user clears
//! that switch, never the reconcile — see `backend::clear_os_override`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

use serde::Serialize;
use tauri::{menu::CheckMenuItem, AppHandle, Manager, WebviewWindow, Wry};

use crate::{load_config, notifications::lock, save_config, MAIN_LABEL};

/// Argument the registration carries. Distinct from the toast activator's
/// `-ToastActivated` (`windows_activator.rs`) and from the
/// `openframe-desktop://` URIs `handle_notification_argv` looks for, so the
/// three argv consumers cannot be confused for one another.
const FLAG: &str = "--autostart";

/// Whether the OS started this process at login.
pub(crate) fn launched_at_login() -> bool {
    has_flag(std::env::args_os())
}

fn has_flag<I: IntoIterator<Item = std::ffi::OsString>>(args: I) -> bool {
    args.into_iter().any(|arg| arg == FLAG)
}

/// Whether this process is going to live in the tray without ever building a
/// window, which is not the same question as [`launched_at_login`]: a login
/// start that an update asked to show a window is not headless. Only setup can
/// answer it, so it records the answer here.
///
/// `false` until then, which is the protective default — it keeps
/// `show_primary_window` refusing to build a window out from under setup.
static HEADLESS: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_headless(headless: bool) {
    HEADLESS.store(headless, Ordering::Relaxed);
}

pub(crate) fn is_headless() -> bool {
    HEADLESS.load(Ordering::Relaxed)
}

/// What the login registration currently is, in the terms a toggle needs.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
    /// Whether the app will start at the next login. On Windows this accounts
    /// for the Task Manager switch, and on Linux for GNOME's and the XDG
    /// `Hidden` one; on macOS it cannot (see the module docs), so there it means
    /// *registered* rather than *will run*.
    enabled: bool,
    /// Whether [`crate::AppConfig::autostart_enforced`] pins this, i.e. the
    /// toggle is not the user's to move.
    enforced: bool,
}

/// What went wrong, in the same two terms the updater reports: a `kind` the UI
/// turns into copy, and the message for the log.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AutostartError {
    kind: &'static str,
    message: String,
}

impl AutostartError {
    /// A managed install pins this. Told to the caller rather than swallowed:
    /// a silent no-op leaves the switch flipped in the UI until something else
    /// re-reads the status.
    fn enforced() -> Self {
        Self {
            kind: "enforced",
            message: "start at login is managed by your organization".into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: "unavailable",
            message: message.into(),
        }
    }

    fn io(message: impl Into<String>) -> Self {
        Self {
            kind: "io",
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Payload renderers.
//
// Pure, and compiled for every test host as well as for their own target — the
// same arrangement as `windows_toast`, and for the same reason: these fail
// inside the OS rather than in a stack trace, so whichever host CI runs the
// tests on should cover all of them. The `test` half of each gate is not
// optional: `mod tests` below is not target-gated, so narrowing these to one
// host breaks `cargo test` and `cargo clippy --all-targets` on the others.
// ---------------------------------------------------------------------------

/// `"<exe>" --autostart`. The quotes are load-bearing: without them the space in
/// the install path lets `CreateProcessW` resolve a planted `OpenFrame.exe`
/// first.
#[cfg(any(target_os = "windows", test))]
fn windows_run_command(exe: &std::path::Path) -> String {
    format!("\"{}\" {FLAG}", exe.display())
}

/// A launchd user agent. `AssociatedBundleIdentifiers` is what makes macOS 13+
/// attribute the entry to this app in System Settings → Login Items instead of
/// listing an opaque background item. `KeepAlive` is deliberately absent: with
/// it, launchd would relaunch the app every time the user picks Quit.
#[cfg(any(target_os = "macos", test))]
fn macos_plist(exe: &std::path::Path, bundle_id: &str) -> Result<String, String> {
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{id}</string>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>{id}</string>
	</array>
	<key>ProgramArguments</key>
	<array>
		<string>{path}</string>
		<string>{FLAG}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#,
        id = escape_xml(bundle_id)?,
        path = escape_xml(&exe.display().to_string())?,
    ))
}

/// `&`, `<` and `>` are all legal in a macOS path and would otherwise produce a
/// plist launchd cannot parse.
///
/// A C0 control character is legal in an APFS filename too, and XML 1.0 has no
/// way to carry one — not even as a numeric reference — so that case is refused
/// rather than escaped. Substituting something printable would be worse than
/// failing: the plist would parse and point at a path that does not exist, and
/// the whole failure is invisible until the next login.
///
/// `\r` is refused for that same reason rather than because it is illegal:
/// XML 1.0 §2.11 has the parser normalize CR and CRLF to LF before anything
/// sees them, so a path carrying one would come back out as a different path.
/// U+FFFE and U+FFFF go with them — they are the only non-surrogate exclusions
/// from XML 1.0's `Char` production (§2.2), so like the C0 set they cannot be
/// carried even as a numeric reference.
#[cfg(any(target_os = "macos", test))]
fn escape_xml(value: &str) -> Result<String, String> {
    if let Some(c) = value.chars().find(|c| {
        (*c < '\x20' && !matches!(c, '\t' | '\n')) || matches!(*c, '\u{FFFE}' | '\u{FFFF}')
    }) {
        // "value", not "path": the bundle identifier comes through here too.
        return Err(format!(
            "value contains U+{:04X}, which XML cannot represent",
            c as u32
        ));
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;"))
}

/// An XDG autostart entry, carrying only the keys the shell owns. No
/// `X-GNOME-Autostart-enabled=true`: absent already means enabled, and writing
/// it would collide with the `=false` a user's disable puts in this same file.
#[cfg(any(target_os = "linux", test))]
fn linux_desktop_entry(exe: &std::path::Path) -> Result<String, String> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=OpenFrame\n\
         Exec=\"{}\" {FLAG}\n",
        desktop_exec_arg(&exe.display().to_string())?
    ))
}

/// Split a desktop-entry line into a lowercased key and its value. The spec has
/// whitespace around the `=` ignored, so `Hidden = true` and `Hidden=true` are
/// the same line and must not read differently.
#[cfg(any(target_os = "linux", test))]
fn desktop_pair(line: &str) -> Option<(String, &str)> {
    let (key, value) = line.split_once('=')?;
    Some((key.trim().to_ascii_lowercase(), value.trim()))
}

/// Whether the desktop environment's own switch, which it writes into the entry
/// we wrote, says the user turned this off. `Hidden=true` is the XDG-spec form
/// (XFCE, KDE); `X-GNOME-Autostart-enabled=false` is GNOME's. This is the Linux
/// counterpart of Windows' `StartupApproved`, and the reason it is readable at
/// all is that it lives in our own file.
#[cfg(any(target_os = "linux", test))]
fn linux_os_disabled(contents: &str) -> bool {
    contents.lines().any(|line| match desktop_pair(line) {
        Some((key, value)) if key == "hidden" => value.eq_ignore_ascii_case("true"),
        Some((key, value)) if key == "x-gnome-autostart-enabled" => {
            value.eq_ignore_ascii_case("false")
        }
        _ => false,
    })
}

/// The lines this shell owns, which is all a staleness check may compare.
///
/// An allow-list, not a deny-list. A desktop environment may add anything to the
/// entry — a start delay, an icon, a comment, its own switch — and under a
/// deny-list every unfamiliar key reads as staleness, so the reconcile rewrites
/// the entry and reverts the user's disable at every launch, forever. That is
/// the fight the module docs say this design refuses.
#[cfg(any(target_os = "linux", test))]
fn linux_ours(contents: &str) -> String {
    contents
        .lines()
        .filter(|line| {
            let line = line.trim();
            line == "[Desktop Entry]"
                || matches!(
                    desktop_pair(line).as_ref().map(|(key, _)| key.as_str()),
                    Some("type" | "name" | "exec")
                )
        })
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The switch lines to carry across a rewrite: a repair is about the binary
/// path, not about overruling the user. Only an explicit request drops them,
/// through `backend::clear_os_override`.
#[cfg(any(target_os = "linux", test))]
fn linux_preserved(contents: &str) -> String {
    contents
        .lines()
        .filter(|line| {
            matches!(
                desktop_pair(line).as_ref().map(|(key, _)| key.as_str()),
                Some("hidden" | "x-gnome-autostart-enabled")
            )
        })
        .map(|line| format!("{}\n", line.trim()))
        .collect()
}

/// Escaping for one quoted argument of an `Exec=` value, which is not the same
/// as shell quoting.
///
/// `%` introduces a field code (`%f`, `%U`, …) and the Desktop Entry spec has
/// launchers drop an unrecognized one, so a path under `/home/u/100%/` would
/// otherwise launch the wrong file. The reserved characters inside a quoted
/// argument take a backslash, and because the value is itself a desktop-file
/// `string` that backslash has to be written doubled — which is why a literal
/// backslash needs four, not three: two to survive the string decode, and those
/// two again to survive the argument's own unquoting.
///
/// A control character is refused rather than escaped, on the same principle
/// [`escape_xml`] applies: a raw newline would end the `Exec=` line and leave a
/// structurally broken entry, and refusing beats emitting something that parses
/// and points elsewhere. The two renderers share that principle, not a rejected
/// set — XML carries `\t` and `\n` happily, a desktop entry does not.
#[cfg(any(target_os = "linux", test))]
fn desktop_exec_arg(value: &str) -> Result<String, String> {
    if let Some(c) = value.chars().find(|c| c.is_control()) {
        return Err(format!(
            "path contains U+{:04X}, which a desktop entry cannot carry",
            c as u32
        ));
    }
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '%' => out.push_str("%%"),
            '\\' => out.push_str(r"\\\\"),
            '"' | '$' | '`' => {
                out.push_str(r"\\");
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-platform registration. Each backend answers the same questions: is
// something registered, will the OS honour it, does it still point at this
// binary, and how is it written or removed.
//
// Windows is its own module because a registry value and the separate
// `StartupApproved` switch have no analogue on the others; macOS and Linux
// differ only in where the file goes and what goes in it, so they share
// everything else, with the platform difference pushed down into `path` and
// `content`.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod backend {
    use tauri::AppHandle;
    use winreg::{enums::*, RegKey};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    /// Where Task Manager records the user switching a startup entry off.
    const APPROVED_KEY: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    /// Value name under both keys. Deliberately the product name rather than
    /// `app.config().identifier` — this one is user-facing, sitting alongside
    /// other applications' names in the Run key and in startup managers — and
    /// deliberately a literal, so renaming the product cannot orphan a
    /// registration an installed copy already wrote.
    const VALUE: &str = "OpenFrame Desktop";

    fn expected(app: &AppHandle) -> Result<String, String> {
        Ok(super::windows_run_command(&super::binary(app)?))
    }

    fn registered_command() -> Option<String> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .ok()?
            .get_value::<String, _>(VALUE)
            .ok()
    }

    /// Whether Task Manager's Startup tab still has the entry switched on.
    ///
    /// The value is a binary blob: a leading state dword followed by the
    /// FILETIME at which it was disabled, zero while it is enabled. The state
    /// byte has taken several values across Windows releases (`02` and `06`
    /// enabled, `03` and `07` disabled) and bit 0 is what separates them, so
    /// that is the rule applied here. A third-party startup manager may write a
    /// disable without a timestamp, or a timestamp without flipping the bit, so
    /// either signal alone can miss one and both are checked. An absent value
    /// means the switch was never touched.
    fn approved() -> bool {
        let Some(blob) = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(APPROVED_KEY)
            .ok()
            .and_then(|key| key.get_raw_value(VALUE).ok())
        else {
            return true;
        };
        let state_disabled = blob.bytes.first().is_some_and(|state| *state % 2 == 1);
        let timestamped = match blob.bytes.len().checked_sub(8) {
            Some(start) if start > 0 => blob.bytes[start..].iter().any(|byte| *byte != 0),
            _ => false,
        };
        !state_disabled && !timestamped
    }

    pub(super) fn registered(_app: &AppHandle) -> bool {
        registered_command().is_some()
    }

    pub(super) fn enabled(app: &AppHandle) -> bool {
        registered(app) && approved()
    }

    pub(super) fn matches_current(app: &AppHandle) -> bool {
        matches!((registered_command(), expected(app)), (Some(got), Ok(want)) if got == want)
    }

    pub(super) fn enable(app: &AppHandle) -> Result<(), String> {
        let (run, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN_KEY)
            .map_err(|e| e.to_string())?;
        run.set_value(VALUE, &expected(app)?)
            .map_err(|e| e.to_string())
    }

    pub(super) fn disable(_app: &AppHandle) -> Result<(), String> {
        let run = match RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        {
            Ok(run) => run,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        match run.delete_value(VALUE) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Drop Task Manager's "disabled" record, so the user's own request to turn
    /// this on is not silently overruled by a switch they flipped months ago.
    /// Reached only from an explicit [`super::set`] — never from the reconcile,
    /// which must leave that switch alone.
    pub(super) fn clear_os_override(_app: &AppHandle) {
        if let Ok(key) =
            RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(APPROVED_KEY, KEY_SET_VALUE)
        {
            let _ = key.delete_value(VALUE);
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod backend {
    use tauri::{AppHandle, Manager};

    #[cfg(target_os = "macos")]
    fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
        let home = app.path().home_dir().map_err(|e| e.to_string())?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", app.config().identifier)))
    }

    #[cfg(target_os = "linux")]
    fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
        let config = app.path().config_dir().map_err(|e| e.to_string())?;
        Ok(config
            .join("autostart")
            .join(format!("{}.desktop", app.config().identifier)))
    }

    #[cfg(target_os = "macos")]
    fn content(app: &AppHandle) -> Result<String, String> {
        super::macos_plist(&super::binary(app)?, &app.config().identifier)
    }

    #[cfg(target_os = "linux")]
    fn content(app: &AppHandle) -> Result<String, String> {
        super::linux_desktop_entry(&super::binary(app)?)
    }

    pub(super) fn registered(app: &AppHandle) -> bool {
        path(app).is_ok_and(|path| path.exists())
    }

    /// Whether the OS will honour the registration, which is the user's own
    /// switch to flip.
    ///
    /// macOS keeps that switch in a background-item database with no public
    /// API, so there this can only ever repeat [`registered`] — see the module
    /// docs. GNOME keeps it as `X-GNOME-Autostart-enabled=false` inside the very
    /// entry we wrote, which we can read, so on Linux it is honoured.
    pub(super) fn enabled(app: &AppHandle) -> bool {
        let Ok(path) = path(app) else {
            return false;
        };
        std::fs::read_to_string(path).is_ok_and(|contents| !os_disabled(&contents))
    }

    /// Nothing reachable to read: macOS records the user's switch in a
    /// background-item database with no public API.
    #[cfg(target_os = "macos")]
    fn os_disabled(_contents: &str) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    fn os_disabled(contents: &str) -> bool {
        super::linux_os_disabled(contents)
    }

    /// Nothing to exclude on macOS — the plist carries only what we wrote.
    #[cfg(target_os = "macos")]
    fn ours(contents: &str) -> &str {
        contents
    }

    #[cfg(target_os = "linux")]
    fn ours(contents: &str) -> String {
        super::linux_ours(contents)
    }

    pub(super) fn matches_current(app: &AppHandle) -> bool {
        let (Ok(path), Ok(want)) = (path(app), content(app)) else {
            return false;
        };
        std::fs::read_to_string(path).is_ok_and(|got| ours(&got) == ours(&want))
    }

    /// `launchctl` is deliberately not run: taking effect at the next login is
    /// exactly the semantics wanted, and loading it now would start a second
    /// copy of an app that is already running.
    pub(super) fn enable(app: &AppHandle) -> Result<(), String> {
        let path = path(app)?;
        let existing = std::fs::read_to_string(&path).ok();
        crate::write_atomic(&path, &preserve(existing.as_deref(), content(app)?))
    }

    /// Carry the desktop environment's switch across a rewrite, so a repair of
    /// the binary path does not double as re-enabling something the user turned
    /// off. Windows gets this for free — its switch lives in a different key
    /// that `enable` never touches.
    #[cfg(target_os = "macos")]
    fn preserve(_existing: Option<&str>, content: String) -> String {
        content
    }

    #[cfg(target_os = "linux")]
    fn preserve(existing: Option<&str>, mut content: String) -> String {
        if let Some(existing) = existing {
            content.push_str(&super::linux_preserved(existing));
        }
        content
    }

    pub(super) fn disable(app: &AppHandle) -> Result<(), String> {
        match std::fs::remove_file(path(app)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Nothing to clear on macOS, where the switch is out of reach. On Linux it
    /// is a line inside the entry, so writing the entry *without* preserving it
    /// is what drops it — reached only when the user has just asked for
    /// start-at-login to be on.
    pub(super) fn clear_os_override(app: &AppHandle) {
        let _ = app;
        #[cfg(target_os = "linux")]
        if let (Ok(path), Ok(content)) = (path(app), content(app)) {
            let _ = crate::write_atomic(&path, &content);
        }
    }
}

/// The binary to register.
///
/// `current_binary`, not `current_exe`: on Linux an AppImage runs from an
/// ephemeral `/tmp/.mount_*` path, which would be registered, then fail to
/// match at the next launch, and be rewritten forever — the exact
/// non-convergence [`decide`] exists to avoid. Tauri resolves the real image
/// path from `APPIMAGE`; elsewhere this is `current_exe`.
fn binary(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    tauri::process::current_binary(&app.env()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// The startup reconcile
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Nothing,
    /// Write the registration — also the repair for a stale one.
    Register,
    Unregister,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Decision {
    action: Action,
    /// Whether this launch is the one applying the product default, and so owes
    /// `autostart_configured` a write.
    mark_configured: bool,
}

/// Everything the decision turns on, separated from reading it so the whole
/// precedence order is testable without an OS to register against.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Facts {
    /// [`crate::AppConfig::autostart_enforced`]: the managed-install policy, if
    /// there is one.
    policy: Option<bool>,
    /// Whether the product default has already been applied on this machine.
    configured: bool,
    /// Whether a registration exists at all.
    registered: bool,
    /// Whether that registration still matches what we would write now.
    current: bool,
}

fn decide(facts: Facts) -> Decision {
    let plain = |action| Decision {
        action,
        mark_configured: false,
    };
    match facts.policy {
        // Managed install: converge on the policy. Keyed on `registered` rather
        // than on whether the OS will actually honour it — a user who switched
        // the entry off in Task Manager leaves it registered, and keying on the
        // effective state would rewrite the identical value at every launch
        // forever without ever changing the outcome. `status` reports that
        // conflict instead.
        Some(true) if !facts.registered || !facts.current => plain(Action::Register),
        Some(false) if facts.registered => plain(Action::Unregister),
        Some(_) => plain(Action::Nothing),
        // First launch on this machine: apply the default (on) once, and record
        // that it has been applied — so no later launch re-applies it over a
        // user who has since turned it off.
        None if !facts.configured => Decision {
            action: Action::Register,
            mark_configured: true,
        },
        // The user owns it from here. Repair a registration that no longer
        // points at this binary — the .app was moved, or reinstalled elsewhere
        // — but never resurrect one they removed.
        None if facts.registered && !facts.current => plain(Action::Register),
        None => plain(Action::Nothing),
    }
}

/// Serializes deciding what the registration should be, writing it, and
/// recording that the default was applied.
///
/// The startup reconcile runs on its own thread while a tray or webview toggle
/// runs on the main thread. Without this, a toggle landing between the
/// reconcile's decision and its write is silently reverted by a decision that
/// predates it — and both paths do a read-modify-write of config.json through
/// the same temp path, where the loser's rename can fail after the winner has
/// already published the loser's bytes.
static REGISTRATION: Mutex<()> = Mutex::new(());

/// Bring the login registration into line with policy and the product default,
/// once per launch.
///
/// On its own thread rather than the async runtime: this is a handful of
/// blocking registry or file operations, the same shape as
/// `spawn_show_fallback`.
pub(crate) fn reconcile_on_startup(app: &AppHandle) {
    // `current_binary` in a dev build is `target/debug/…`, which the OS never
    // registered — no tray, no notifications, no bundle identity. Registering
    // it would boot that at every login. An explicit toggle still works, so the
    // mechanism stays testable in dev.
    if cfg!(debug_assertions) {
        log::info!("[autostart] debug build — leaving the login registration alone");
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        {
            let _serialized = lock(&REGISTRATION);
            let cfg = load_config(&app);
            let facts = Facts {
                policy: cfg.autostart_enforced,
                configured: cfg.autostart_configured.unwrap_or(false),
                registered: backend::registered(&app),
                current: backend::matches_current(&app),
            };
            if let Some(policy) = facts.policy {
                log::info!(
                    "[autostart] pinned by config policy: {}",
                    if policy { "enabled" } else { "disabled" }
                );
                // Why a managed machine can still fail to start, in the one
                // place a post-mortem will look.
                if policy && facts.registered && !backend::enabled(&app) {
                    log::warn!(
                        "[autostart] policy requires start at login, but the OS reports it \
                         switched off by the user — reporting the conflict rather than \
                         overriding it"
                    );
                }
            }

            let decision = decide(facts);
            let outcome = match decision.action {
                Action::Nothing => Ok("already as it should be"),
                Action::Register => backend::enable(&app).map(|()| "registered for login"),
                Action::Unregister => backend::disable(&app).map(|()| "removed from login"),
            };
            match outcome {
                Ok(what) => log::info!("[autostart] {what}"),
                Err(e) => {
                    // Left unmarked so the next launch tries again.
                    log::warn!("[autostart] could not update the login registration: {e}");
                    return;
                }
            }
            if decision.mark_configured {
                mark_configured(&app);
            }
        }
        sync_tray(&app);
    });
}

/// Record that the product default has been applied, so it is never applied a
/// second time over a choice the user has made since. Callers hold
/// [`REGISTRATION`].
fn mark_configured(app: &AppHandle) {
    let mut cfg = load_config(app);
    if cfg.autostart_configured == Some(true) {
        return;
    }
    cfg.autostart_configured = Some(true);
    if let Err(e) = save_config(app, &cfg) {
        log::warn!("[autostart] could not record the applied default: {e}");
    }
}

// ---------------------------------------------------------------------------
// Reading and moving the toggle
// ---------------------------------------------------------------------------

/// Whether a policy pins start-at-login, i.e. the toggle is not the user's to
/// move. One definition, so [`status`] and [`set`] cannot drift apart on it.
fn enforced(app: &AppHandle) -> bool {
    load_config(app).autostart_enforced.is_some()
}

fn status(app: &AppHandle) -> Status {
    Status {
        enabled: backend::enabled(app),
        enforced: enforced(app),
    }
}

fn set(app: &AppHandle, enabled: bool) -> Result<Status, AutostartError> {
    if enforced(app) {
        return Err(AutostartError::enforced());
    }
    {
        let _serialized = lock(&REGISTRATION);
        if enabled {
            backend::enable(app).map_err(AutostartError::io)?;
            // Only here, and only for a request the user actually made.
            backend::clear_os_override(app);
        } else {
            backend::disable(app).map_err(AutostartError::io)?;
        }
        log::info!(
            "[autostart] start at login turned {} by the user",
            if enabled { "on" } else { "off" }
        );
        // A deliberate choice answers the "has the default been applied"
        // question as well as the default itself does.
        mark_configured(app);
    }
    // The value handed back and the value put on the tray are the same read, so
    // the two cannot disagree about what just happened.
    Ok(sync_tray(app))
}

/// The tray's check item, kept so both the tray and the webview move the same
/// one. Registered by `build_tray`; absent only before it has run.
pub(crate) struct TrayCheck(pub(crate) CheckMenuItem<Wry>);

/// Put the current state on the tray item and return it, so callers that need
/// both do not read it twice and risk two different answers.
///
/// The one place that decides how a [`Status`] renders as a menu item — the
/// tray is built with placeholder flags and then synced, rather than repeating
/// the mapping at construction.
pub(crate) fn sync_tray(app: &AppHandle) -> Status {
    let status = status(app);
    if let Some(item) = app.try_state::<TrayCheck>() {
        let _ = item.0.set_checked(status.enabled);
        // Re-applied rather than set once at build time: fleet tooling can
        // write or withdraw the policy while the app is running, and an item
        // left greyed from a policy that is gone could never be moved again.
        let _ = item.0.set_enabled(!status.enforced);
    }
    status
}

/// Tray "Start at Login". The OS has already flipped the item's own check state
/// by the time this runs, so [`sync_tray`] is called on the failure path too —
/// otherwise a rejected toggle would leave the menu claiming it worked.
pub(crate) fn toggle_from_tray(app: &AppHandle) {
    let want = !backend::enabled(app);
    if let Err(e) = set(app, want) {
        log::warn!("[autostart] tray toggle failed ({}): {}", e.kind, e.message);
        sync_tray(app);
    }
}

/// Ask the process that comes back from a restart to show its window, even
/// though [`FLAG`] would otherwise keep it in the tray.
///
/// A durable marker rather than the relaunch argv, because on Windows the
/// relaunch is not ours to shape: `download_and_install` hands this process's
/// own argv to the installer — `/ARGS` for NSIS, `LAUNCHAPPARGS` for MSI — and
/// then calls `std::process::exit(0)`, so nothing after it runs and
/// `--autostart` is replayed whatever we do. The marker survives that, and works
/// the same on every platform.
///
/// Cleared again if the install fails, so it cannot outlive the restart it was
/// recorded for; [`take_show_window_request`] is the backstop either way.
pub(crate) fn show_window_after_restart(app: &AppHandle, wanted: bool) {
    let _serialized = lock(&REGISTRATION);
    let mut cfg = load_config(app);
    let next = wanted.then_some(true);
    if cfg.autostart_show_window_next_start == next {
        return;
    }
    cfg.autostart_show_window_next_start = next;
    if let Err(e) = save_config(app, &cfg) {
        log::warn!("[autostart] could not record the pending window request: {e}");
    }
}

/// Whether a restart asked for a window, clearing the request as it reads it.
/// Called on every launch, not only a login start, so a marker left behind by an
/// update that never completed cannot outlive one boot.
///
/// Takes [`REGISTRATION`] because the reconcile thread is already running by
/// now and writes the same file: the lock is what stops its `mark_configured`
/// from republishing the marker this just cleared.
pub(crate) fn take_show_window_request(app: &AppHandle) -> bool {
    let _serialized = lock(&REGISTRATION);
    let mut cfg = load_config(app);
    let previous = cfg.autostart_show_window_next_start.take();
    if previous.is_some() {
        if let Err(e) = save_config(app, &cfg) {
            log::warn!("[autostart] could not clear the pending window request: {e}");
        }
    }
    // The value, not merely the key: a hand-written or fleet-written `false`
    // must not read as a request.
    previous == Some(true)
}

#[tauri::command]
pub(crate) fn autostart_status(app: AppHandle, window: WebviewWindow) -> Status {
    if window.label() != MAIN_LABEL {
        return Status {
            enabled: false,
            enforced: false,
        };
    }
    status(&app)
}

#[tauri::command]
pub(crate) fn autostart_set(
    app: AppHandle,
    window: WebviewWindow,
    enabled: bool,
) -> Result<Status, AutostartError> {
    if window.label() != MAIN_LABEL {
        return Err(AutostartError::unavailable(
            "start at login can only be changed from the main window",
        ));
    }
    set(&app, enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn flag_is_found_among_the_other_argv_consumers() {
        let mut argv = vec![
            OsString::from("/Applications/OpenFrame Desktop.app/Contents/MacOS/OpenFrame Desktop"),
            OsString::from("-ToastActivated"),
            OsString::from("openframe-desktop://notification?id=1"),
        ];
        assert!(!has_flag(argv.clone()));
        argv.push(OsString::from(FLAG));
        assert!(has_flag(argv));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_argument_does_not_stop_the_search() {
        use std::os::unix::ffi::OsStringExt;
        let argv = vec![OsString::from_vec(vec![0xff, 0xfe]), OsString::from(FLAG)];
        assert!(has_flag(argv));
    }

    #[test]
    fn first_launch_applies_the_default_once() {
        let d = decide(Facts::default());
        assert_eq!(d.action, Action::Register);
        assert!(d.mark_configured);
    }

    #[test]
    fn a_removed_registration_is_not_resurrected() {
        let d = decide(Facts {
            configured: true,
            ..Facts::default()
        });
        assert_eq!(d.action, Action::Nothing);
        assert!(!d.mark_configured);
    }

    #[test]
    fn a_stale_registration_is_repaired_without_remarking() {
        let d = decide(Facts {
            configured: true,
            registered: true,
            ..Facts::default()
        });
        assert_eq!(d.action, Action::Register);
        assert!(!d.mark_configured);
    }

    #[test]
    fn a_current_registration_is_left_alone() {
        let d = decide(Facts {
            configured: true,
            registered: true,
            current: true,
            ..Facts::default()
        });
        assert_eq!(d.action, Action::Nothing);
    }

    #[test]
    fn policy_registers_when_absent_and_repairs_when_stale() {
        for registered in [false, true] {
            let d = decide(Facts {
                policy: Some(true),
                configured: true,
                registered,
                ..Facts::default()
            });
            assert_eq!(d.action, Action::Register);
        }
    }

    /// The convergence property: a user who switched the entry off in Task
    /// Manager leaves it registered and current, and policy must then do
    /// nothing at all — otherwise every launch rewrites the same value forever.
    #[test]
    fn policy_does_not_fight_a_user_disable() {
        let d = decide(Facts {
            policy: Some(true),
            configured: true,
            registered: true,
            current: true,
        });
        assert_eq!(d.action, Action::Nothing);
    }

    #[test]
    fn policy_can_pin_it_off() {
        let d = decide(Facts {
            policy: Some(false),
            configured: true,
            registered: true,
            current: true,
        });
        assert_eq!(d.action, Action::Unregister);
        let d = decide(Facts {
            policy: Some(false),
            configured: true,
            ..Facts::default()
        });
        assert_eq!(d.action, Action::Nothing);
    }

    /// Policy never marks the default as applied, so withdrawing it later hands
    /// the machine back to the user-owned path with the default still to apply.
    #[test]
    fn policy_never_marks_the_default_applied() {
        for policy in [true, false] {
            for registered in [true, false] {
                let d = decide(Facts {
                    policy: Some(policy),
                    registered,
                    current: registered,
                    ..Facts::default()
                });
                assert!(!d.mark_configured);
            }
        }
    }

    /// The property the module docs actually argue for, over every reachable
    /// input: whatever one launch decides, the launch after it has nothing left
    /// to do. A rule that failed this would rewrite the same registration at
    /// every boot for the life of the install.
    #[test]
    fn every_decision_reaches_a_fixpoint() {
        for policy in [None, Some(true), Some(false)] {
            for configured in [false, true] {
                for registered in [false, true] {
                    for current in [false, true] {
                        // Nothing registered cannot also be up to date.
                        if !registered && current {
                            continue;
                        }
                        let first = decide(Facts {
                            policy,
                            configured,
                            registered,
                            current,
                        });
                        let settled = Facts {
                            policy,
                            configured: configured || first.mark_configured,
                            registered: match first.action {
                                Action::Register => true,
                                Action::Unregister => false,
                                Action::Nothing => registered,
                            },
                            current: match first.action {
                                Action::Register => true,
                                Action::Unregister => false,
                                Action::Nothing => current,
                            },
                        };
                        let second = decide(settled);
                        assert_eq!(
                            second.action,
                            Action::Nothing,
                            "did not settle from {policy:?}/{configured}/{registered}/{current}"
                        );
                        // Not just idle: a rule that kept re-marking would
                        // rewrite config.json at every launch forever.
                        assert!(
                            !second.mark_configured,
                            "kept re-marking from {policy:?}/{configured}/{registered}/{current}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_windows_command_quotes_a_path_containing_spaces() {
        let exe = Path::new(r"C:\Users\x\AppData\Local\OpenFrame Desktop\OpenFrame Desktop.exe");
        assert_eq!(
            windows_run_command(exe),
            r#""C:\Users\x\AppData\Local\OpenFrame Desktop\OpenFrame Desktop.exe" --autostart"#
        );
    }

    #[test]
    fn the_plist_is_attributed_and_does_not_keep_the_app_alive() {
        let plist = macos_plist(
            Path::new("/Applications/OpenFrame Desktop.app/Contents/MacOS/OpenFrame Desktop"),
            "com.openframe.desktop",
        )
        .expect("a plain path renders");
        assert!(plist.contains("<key>AssociatedBundleIdentifiers</key>"));
        assert!(plist.contains("<string>com.openframe.desktop</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        // Spelled out rather than interpolated: these assertions pin the bytes
        // the OS is handed, so a change to FLAG should fail them.
        assert!(plist.contains("<string>--autostart</string>"));
        // Quit must mean quit.
        assert!(!plist.contains("KeepAlive"));
    }

    #[test]
    fn a_path_with_xml_syntax_in_it_stays_parseable() {
        let plist = macos_plist(
            Path::new("/Users/a & b/<x>/OpenFrame.app/x"),
            "com.openframe.desktop",
        )
        .expect("escapable characters render");
        assert!(plist.contains("/Users/a &amp; b/&lt;x&gt;/OpenFrame.app/x"));
    }

    /// Escaping cannot save this one, so it has to fail where someone can see
    /// it rather than at the next login.
    #[test]
    fn a_path_with_a_control_character_is_refused() {
        assert!(macos_plist(
            Path::new("/Users/a\u{1}b/OpenFrame.app/x"),
            "com.openframe.desktop"
        )
        .is_err());
    }

    #[test]
    fn the_desktop_entry_carries_the_flag() {
        let entry = linux_desktop_entry(Path::new("/opt/openframe/openframe-desktop"))
            .expect("a plain path renders");
        assert!(entry.contains("Exec=\"/opt/openframe/openframe-desktop\" --autostart"));
    }

    /// The desktop environment's switch lives in the entry we wrote, so it has
    /// to be read back out of it. Spacing around `=` is spec-insignificant and
    /// must not change the answer.
    #[test]
    fn a_desktop_switch_is_read_however_it_is_written() {
        assert!(linux_os_disabled("Hidden=true"));
        assert!(linux_os_disabled("Hidden = true"));
        assert!(linux_os_disabled("hidden=TRUE"));
        assert!(linux_os_disabled("X-GNOME-Autostart-enabled=false"));
        assert!(linux_os_disabled("X-GNOME-Autostart-enabled = False"));
        assert!(!linux_os_disabled("Hidden=false"));
        assert!(!linux_os_disabled("X-GNOME-Autostart-enabled=true"));
        assert!(!linux_os_disabled(
            &linux_desktop_entry(Path::new("/opt/x")).unwrap()
        ));
    }

    /// The convergence property on Linux: whatever a desktop environment adds
    /// to our entry — its own switch, a start delay, an icon — the entry must
    /// still compare equal to what we would write, or the reconcile "repairs"
    /// it at every launch and reverts the user's disable forever.
    #[test]
    fn a_desktop_environments_additions_do_not_read_as_staleness() {
        let ours = linux_desktop_entry(Path::new("/opt/openframe/openframe-desktop")).unwrap();
        for addition in [
            "Hidden=true",
            "X-GNOME-Autostart-enabled=false",
            "X-GNOME-Autostart-Delay=10",
            "Icon=openframe",
            "Comment=added by the desktop",
        ] {
            let theirs = format!("{ours}{addition}\n");
            assert_eq!(
                linux_ours(&theirs),
                linux_ours(&ours),
                "{addition} read as staleness"
            );
        }
        // A genuinely different binary must still read as stale.
        let moved = linux_desktop_entry(Path::new("/opt/elsewhere/openframe-desktop")).unwrap();
        assert_ne!(linux_ours(&moved), linux_ours(&ours));
    }

    /// A repair is about the binary path; it must not double as re-enabling
    /// something the user switched off.
    #[test]
    fn a_rewrite_carries_the_users_switch_across() {
        let existing = format!(
            "{}Hidden=true\nIcon=openframe\n",
            linux_desktop_entry(Path::new("/old/openframe-desktop")).unwrap()
        );
        let preserved = linux_preserved(&existing);
        assert!(preserved.contains("Hidden=true"));
        // Only the switch, not everything the desktop added.
        assert!(!preserved.contains("Icon"));
    }

    /// A field code the launcher does not recognise is dropped, so an unescaped
    /// `%` in the path silently launches something else.
    #[test]
    fn the_desktop_entry_escapes_field_codes_and_reserved_characters() {
        let arg = |value| desktop_exec_arg(value).expect("no control characters");
        assert_eq!(arg("/home/u/100%/app"), "/home/u/100%%/app");
        assert_eq!(arg(r#"/home/u/a"b"#), r#"/home/u/a\\"b"#);
        assert_eq!(arg("/home/u/a$b"), r"/home/u/a\\$b");
        // Four, not two: the escape survives the desktop-file string decode and
        // then the quoted argument's own unquoting.
        assert_eq!(arg(r"/home/u/a\b"), r"/home/u/a\\\\b");
        // A newline would end the Exec line and break the entry structurally.
        assert!(desktop_exec_arg("/home/u/a\nb").is_err());
    }
}
