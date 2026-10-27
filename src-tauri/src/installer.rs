//! "Deploy on this Device": run the agent install command on the machine the
//! console is running on, with administrator rights, and stream what it prints
//! back to the setup wizard.
//!
//! The command is the same one the wizard offers to copy - the web app builds
//! it and hands it over verbatim - so the shell adds exactly two things a
//! browser tab cannot: the elevation prompt and a place to run it.
//!
//! Elevation goes through the OS's own prompt, never a password of ours:
//! `osascript ... with administrator privileges` on macOS, `Start-Process
//! -Verb RunAs` (UAC) on Windows. Both run the command inside an elevated
//! process whose output we cannot read directly, so the command runs from a
//! small runner script that redirects everything into a log file in a
//! per-run temp directory, and a thread here tails that file into
//! [`EVENT_OUTPUT`] events until the launcher exits, then reports the exit
//! code it wrote beside the log in [`EVENT_DONE`].
//!
//! The run directory holds the install command, and the command holds the
//! workspace's registration secret: it is created readable by this user only
//! and removed as soon as the run is over, however it ended.

use std::sync::atomic::AtomicBool;

use serde::Serialize;
use tauri::{AppHandle, State, WebviewWindow};

use crate::MAIN_LABEL;

/// Lines the installer printed since the last event, in order. Batched per
/// tail interval, so a chatty installer costs one IPC round trip per interval
/// rather than one per line.
pub const EVENT_OUTPUT: &str = "agent-install:output";
/// The run is over: success, the exit code when the command ran, and the error
/// when it did not (the elevation prompt declined, the launcher failed).
pub const EVENT_DONE: &str = "agent-install:done";

/// One install at a time: a second "Deploy on this Device" while one is running
/// is answered with [`InstallError::busy`] instead of a second elevation prompt.
#[derive(Default)]
pub struct InstallerState {
    /// Whether an install is currently in flight.
    running: AtomicBool,
}

/// What went wrong, in the two terms the UI needs: a `kind` it turns into copy
/// and an action, and the raw message for the log.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallError {
    kind: &'static str,
    message: String,
}

impl InstallError {
    /// Another install is already running.
    fn busy() -> Self {
        Self {
            kind: "busy",
            message: "an install is already running".into(),
        }
    }

    /// This shell cannot run an installer on this platform at all.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn unsupported() -> Self {
        Self {
            kind: "unsupported",
            message: "local install is not supported on this platform".into(),
        }
    }

    /// Our own precondition failed: a call from the wrong window, an empty
    /// command. Nothing the user can retry.
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: "unavailable",
            message: message.into(),
        }
    }

    /// The run could not be set up or started - the temp directory, the
    /// launcher process.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn io(error: impl std::fmt::Display) -> Self {
        Self {
            kind: "io",
            message: error.to_string(),
        }
    }

    /// The user declined the elevation prompt.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn cancelled() -> Self {
        Self {
            kind: "cancelled",
            message: "the administrator prompt was declined".into(),
        }
    }

    /// The command ran and exited non-zero, or the launcher failed for a reason
    /// that is not a declined prompt. The log carries the installer's own words.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: "failed",
            message: message.into(),
        }
    }
}

/// Lines emitted to the frontend as [`EVENT_OUTPUT`].
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OutputEvent {
    lines: Vec<String>,
}

/// The final outcome emitted to the frontend as [`EVENT_DONE`].
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoneEvent {
    success: bool,
    /// The command's own exit code, when it ran at all.
    exit_code: Option<i32>,
    error: Option<InstallError>,
}

/// Start the install command on this machine. Returns as soon as the run is
/// under way; everything after that arrives as [`EVENT_OUTPUT`] and one
/// [`EVENT_DONE`] on the main window.
#[tauri::command]
pub fn agent_install_start(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, InstallerState>,
    command: String,
) -> Result<(), InstallError> {
    if window.label() != MAIN_LABEL {
        return Err(InstallError::unavailable(
            "an install can only be started from the main window",
        ));
    }
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err(InstallError::unavailable("no install command"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (app, state, command);
        Err(InstallError::unsupported())
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        use std::sync::atomic::Ordering;
        if state
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(InstallError::busy());
        }
        let run = match run::RunDir::create(&command) {
            Ok(run) => run,
            Err(error) => {
                state.running.store(false, Ordering::Release);
                log::warn!("agent install: could not prepare the run: {error}");
                return Err(InstallError::io(error));
            }
        };
        std::thread::spawn(move || {
            use tauri::{Emitter, Manager};
            let done = run::execute(&app, run);
            log::info!(
                "agent install finished: success={} exit_code={:?} error={:?}",
                done.success,
                done.exit_code,
                done.error
            );
            app.state::<InstallerState>()
                .running
                .store(false, Ordering::Release);
            let _ = app.emit_to(MAIN_LABEL, EVENT_DONE, done);
        });
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod run {
    use std::{
        fs,
        io::{Read, Seek, SeekFrom},
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        time::Duration,
    };

    use tauri::{AppHandle, Emitter};

    use super::{DoneEvent, InstallError, OutputEvent, EVENT_OUTPUT};
    use crate::MAIN_LABEL;

    /// How often the log is read while the command runs. One frame per interval
    /// is faster than the installer prints and well under what the eye resolves.
    const TAIL_INTERVAL: Duration = Duration::from_millis(200);

    /// The files of one run, in a directory only this user can read.
    pub(super) struct RunDir {
        dir: PathBuf,
        /// The install command as the web app built it.
        script: PathBuf,
        /// The elevated entry point: runs `script` into `log`, writes `status`.
        runner: PathBuf,
        /// Everything the command printed, stdout and stderr interleaved.
        log: PathBuf,
        /// The command's exit code, written by the runner once it is over.
        status: PathBuf,
    }

    impl RunDir {
        pub(super) fn create(command: &str) -> std::io::Result<Self> {
            let dir =
                std::env::temp_dir().join(format!("openframe-install-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
            }
            let run = Self {
                script: dir.join(script_name()),
                runner: dir.join(runner_name()),
                log: dir.join("install.log"),
                status: dir.join("status"),
                dir,
            };
            write_private(&run.script, &script_body(command))?;
            write_private(&run.runner, &runner_script(&run))?;
            // The log exists before the command does, so the tail never has to
            // tell "not started" from "nothing printed yet".
            write_private(&run.log, "")?;
            Ok(run)
        }
    }

    impl Drop for RunDir {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.dir) {
                log::warn!("agent install: run directory not removed: {error}");
            }
        }
    }

    /// Write `contents` to `path`, visible under its final name only once it
    /// already carries owner-only permissions: on unix the file is created
    /// directly with mode `0o600` rather than tightened after the fact, so
    /// there is no window where a secret-bearing file is world- or
    /// group-readable.
    fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(contents.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            fs::write(path, contents)?;
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn script_name() -> &'static str {
        "install.sh"
    }
    #[cfg(target_os = "windows")]
    fn script_name() -> &'static str {
        "install.ps1"
    }
    #[cfg(target_os = "macos")]
    fn runner_name() -> &'static str {
        "run.sh"
    }
    #[cfg(target_os = "windows")]
    fn runner_name() -> &'static str {
        "run.ps1"
    }

    /// The command as a script of its own, so the runner can refer to it by
    /// path and no quoting of the command itself is ever needed.
    #[cfg(target_os = "macos")]
    fn script_body(command: &str) -> String {
        format!("#!/bin/bash\nset -o pipefail\n{command}\n")
    }
    #[cfg(target_os = "windows")]
    fn script_body(command: &str) -> String {
        format!("$ErrorActionPreference = 'Continue'\n{command}\n")
    }

    /// A path inside a single-quoted shell word.
    #[cfg(target_os = "macos")]
    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    /// A path inside a single-quoted PowerShell string.
    #[cfg(target_os = "windows")]
    fn ps_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "''"))
    }

    /// Inside an AppleScript string literal.
    #[cfg(target_os = "macos")]
    fn applescript_escape(text: &str) -> String {
        text.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// The elevated entry point. `do shell script` turns a non-zero exit into an
    /// AppleScript error that hides the code, so the runner records it itself
    /// and exits zero: a declined prompt is then the only way osascript fails.
    #[cfg(target_os = "macos")]
    fn runner_script(run: &RunDir) -> String {
        format!(
            "#!/bin/bash\n/bin/bash {script} > {log} 2>&1\necho $? > {status}\n",
            script = sh_quote(&run.script),
            log = sh_quote(&run.log),
            status = sh_quote(&run.status),
        )
    }

    /// The elevated entry point. Every stream goes into the log as it is
    /// written; the exit code of the last native command (the agent binary)
    /// is what the install amounts to.
    #[cfg(target_os = "windows")]
    fn runner_script(run: &RunDir) -> String {
        format!(
            "$ErrorActionPreference = 'Continue'\n& {script} *>&1 | Out-File -FilePath {log} -Encoding utf8\n$code = $LASTEXITCODE\nif ($null -eq $code) {{ $code = 0 }}\nSet-Content -Path {status} -Value $code\n",
            script = ps_quote(&run.script),
            log = ps_quote(&run.log),
            status = ps_quote(&run.status),
        )
    }

    #[cfg(target_os = "macos")]
    fn launch(run: &RunDir) -> std::io::Result<Child> {
        let shell_line = format!("/bin/bash {}", sh_quote(&run.runner));
        let apple = format!(
            "do shell script \"{}\" with administrator privileges",
            applescript_escape(&shell_line)
        );
        Command::new("osascript")
            .arg("-e")
            .arg(apple)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    }

    #[cfg(target_os = "windows")]
    fn launch(run: &RunDir) -> std::io::Result<Child> {
        use std::os::windows::process::CommandExt;
        /// CREATE_NO_WINDOW: the unelevated launcher gets no console of its own.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = format!(
            "$p = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',{runner}) -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
            runner = ps_quote(&run.runner)
        );
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    }

    /// The launcher's stderr when the user declined the elevation prompt.
    #[cfg(target_os = "macos")]
    fn was_cancelled(stderr: &str) -> bool {
        stderr.contains("User canceled") || stderr.contains("(-128)")
    }
    #[cfg(target_os = "windows")]
    fn was_cancelled(stderr: &str) -> bool {
        let text = stderr.to_ascii_lowercase();
        text.contains("canceled by the user") || text.contains("cancelled by the user")
    }

    /// Whole lines from `pending`, which keeps whatever came after the last
    /// newline for the next read. Carriage returns and a leading BOM (Out-File
    /// writes one) are not part of any line.
    pub(super) fn take_lines(pending: &mut String) -> Vec<String> {
        let Some(last_newline) = pending.rfind('\n') else {
            return Vec::new();
        };
        let complete: String = pending.drain(..=last_newline).collect();
        complete
            .split_inclusive('\n')
            .map(|line| {
                line.trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .trim_start_matches('\u{feff}')
                    .to_string()
            })
            .collect()
    }

    /// Read what the log gained since `offset`, emit the whole lines.
    fn drain_log(app: &AppHandle, log: &Path, offset: &mut u64, pending: &mut String) {
        let Ok(mut file) = fs::File::open(log) else {
            return;
        };
        if file.seek(SeekFrom::Start(*offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return;
        }
        if bytes.is_empty() {
            return;
        }
        *offset += bytes.len() as u64;
        pending.push_str(&String::from_utf8_lossy(&bytes));
        let lines = take_lines(pending);
        if !lines.is_empty() {
            let _ = app.emit_to(MAIN_LABEL, EVENT_OUTPUT, OutputEvent { lines });
        }
    }

    /// Run to completion: launch, tail the log while the launcher lives, then
    /// settle the outcome from the status file and the launcher's stderr.
    pub(super) fn execute(app: &AppHandle, run: RunDir) -> DoneEvent {
        let mut child = match launch(&run) {
            Ok(child) => child,
            Err(error) => {
                log::warn!("agent install: launcher did not start: {error}");
                return DoneEvent {
                    success: false,
                    exit_code: None,
                    error: Some(InstallError::io(error)),
                };
            }
        };
        // Drained on its own thread: a pipe nobody reads blocks the writer.
        let stderr = child.stderr.take();
        let stderr_reader = std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut stderr) = stderr {
                let _ = stderr.read_to_string(&mut text);
            }
            text
        });

        let mut offset = 0u64;
        let mut pending = String::new();
        let launcher_status = loop {
            drain_log(app, &run.log, &mut offset, &mut pending);
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => std::thread::sleep(TAIL_INTERVAL),
                Err(error) => break Err(error),
            }
        };
        drain_log(app, &run.log, &mut offset, &mut pending);
        let tail = pending
            .trim_end_matches('\r')
            .trim_start_matches('\u{feff}');
        if !tail.is_empty() {
            let _ = app.emit_to(
                MAIN_LABEL,
                EVENT_OUTPUT,
                OutputEvent {
                    lines: vec![tail.to_string()],
                },
            );
        }
        let stderr = stderr_reader.join().unwrap_or_default();

        let exit_code = fs::read_to_string(&run.status)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        match (exit_code, launcher_status) {
            (Some(0), _) => DoneEvent {
                success: true,
                exit_code: Some(0),
                error: None,
            },
            (Some(code), _) => DoneEvent {
                success: false,
                exit_code: Some(code),
                error: Some(InstallError::failed(format!(
                    "the installer exited with code {code}"
                ))),
            },
            // The command never ran: the prompt was declined, or the launcher
            // itself failed, and its stderr is the only account of which.
            (None, launcher) => {
                let detail = stderr.trim();
                let error = if was_cancelled(detail) {
                    InstallError::cancelled()
                } else {
                    let status = launcher
                        .map(|status| status.to_string())
                        .unwrap_or_else(|error| error.to_string());
                    InstallError::failed(if detail.is_empty() {
                        format!("the installer could not be started ({status})")
                    } else {
                        detail.to_string()
                    })
                };
                DoneEvent {
                    success: false,
                    exit_code: None,
                    error: Some(error),
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn take_lines_keeps_the_partial_tail() {
            let mut pending = String::from("\u{feff}one\r\ntwo\nthr");
            assert_eq!(take_lines(&mut pending), vec!["one", "two"]);
            assert_eq!(pending, "thr");
            assert!(take_lines(&mut pending).is_empty());
            pending.push_str("ee\n\nfour\n");
            assert_eq!(take_lines(&mut pending), vec!["three", "", "four"]);
            assert!(pending.is_empty());
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn sh_quote_survives_an_apostrophe() {
            let quoted = sh_quote(Path::new("/tmp/o'brien/run.sh"));
            assert_eq!(quoted, "'/tmp/o'\\''brien/run.sh'");
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn applescript_escape_handles_quotes_and_backslashes() {
            assert_eq!(applescript_escape(r#"a "b" \c"#), r#"a \"b\" \\c"#);
        }

        #[cfg(target_os = "windows")]
        #[test]
        fn ps_quote_doubles_an_apostrophe() {
            let quoted = ps_quote(Path::new(r"C:\Users\O'Brien\run.ps1"));
            assert_eq!(quoted, r"'C:\Users\O''Brien\run.ps1'");
        }

        #[test]
        fn a_run_holds_the_command_privately_and_cleans_up() {
            let run = RunDir::create("echo hello").expect("run dir");
            let dir = run.dir.clone();
            assert!(fs::read_to_string(&run.script)
                .unwrap()
                .contains("echo hello"));
            let runner = fs::read_to_string(&run.runner).unwrap();
            assert!(runner.contains("install.log"));
            assert!(runner.contains("status"));
            assert!(run.log.exists());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&run.script).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                assert_eq!(
                    fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            drop(run);
            assert!(!dir.exists());
        }
    }
}
