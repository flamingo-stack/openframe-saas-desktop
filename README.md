# OpenFrame Desktop

Native desktop shell for OpenFrame — a [Tauri 2](https://tauri.app) application
that embeds the [openframe-oss-frontend](https://github.com/flamingo-stack/openframe-oss-frontend)
static export and adds what a browser tab cannot do: OS notifications that keep
arriving while the app sits in the tray, a session that survives long idle, and
a system-browser OAuth flow.

**No UI code lives in this repo.** The interface is the frontend's static export,
staged into `www/` at build time and embedded in the binary. What is here is the
Rust shell around it.

Supported: macOS, Windows. Linux builds, but has no OS-notification backend.

## Requirements

- Rust (stable) and the [Tauri 2 system prerequisites](https://tauri.app/start/prerequisites/)
  for your platform
- Node.js 20+
- `git` (the frontend export is cloned at build time)

## Quick start

```sh
npm install
npm run build:web   # clone + build openframe-oss-frontend, stage into www/
npm run dev         # tauri dev
```

`npm run dev` stages a placeholder bundle when `www/` is missing — the
`generate_context!` macro embeds `www/` at compile time, so the crate cannot
build without one. The placeholder is enough to compile and run the shell, never
enough to use the app.

Run the app through `npm run dev` or a built `.app`/`.exe`. The bare
`target/debug` binary is not registered with the OS, so tray icons, notifications
and URL-scheme activation behave differently or not at all.

## Building

The `Makefile` is the entry point; the npm scripts are the primitives it calls.
`npm run build` on its own is a raw `tauri build` that skips web staging and
bakes no host.

```sh
make lint     # rustfmt --check + clippy -D warnings
make test     # cargo test
make build OPENFRAME_SHARED_HOST_URL=https://auth.example.com
```

| Variable | Purpose |
|---|---|
| `OPENFRAME_SHARED_HOST_URL` | **Required for `build`.** Baked into the binary; see [Configuration](#configuration). |
| `OPENFRAME_VERSION` | App version, applied via `tauri build --config`. Defaults to `tauri.conf.json`. |
| `TARGET` | Rust target triple for cross-compilation. |
| `BUNDLES` | Bundle subset, e.g. `BUNDLES=app` for an unsigned macOS `.app`. |
| `FRONTEND_DIR` | Use an existing frontend checkout instead of cloning. No git operations are run against it — this is the local dev loop. |
| `FRONTEND_REF` | Frontend branch or tag to build. Default `main`. |
| `FRONTEND_REPO` | Frontend origin, if not the public repo. |

Code signing and notarization are deliberately **not** in the Makefile — they run
against the produced artifact, which is why `tauri.conf.json` sets
`certificateThumbprint: null`.

## Configuration

The shell knows exactly one configured URL: the **shared auth host**. There is no
tenant host to configure — the frontend's sign-in screen discovers the tenant
from the user's email, and the shell learns the tenant origin from the OAuth
callback.

The shared host is baked in at build time and can be overridden per install by
`shared_host` in `config.json`, which lives in the OS app-config directory:

| OS | Path |
|---|---|
| macOS | `~/Library/Application Support/com.openframe.desktop/config.json` |
| Windows | `%APPDATA%\com.openframe.desktop\config.json` |
| Linux | `~/.config/com.openframe.desktop/config.json` |

```json
{
  "shared_host": "https://auth.example.com",
  "learned_host": "https://acme.example.com"
}
```

`learned_host` is written by the shell after login; it is not meant to be edited.
Tokens live next to it in `tokens.json` (owner-readable only, plaintext —
keychain storage is a known gap).

Other keys, all optional:

| Key | Purpose |
|---|---|
| `self_update_enabled` | `false` disables the startup and background update checks. |
| `update_manifest_url` | Overrides the updater manifest endpoint. |
| `autostart_enforced` | Pins **Start at Login** on (`true`) or off (`false`) for a managed install, and takes the toggle away from the user. Absent leaves the choice to them. |
| `autostart_configured` | Shell-written. Records that the start-at-login default has been applied, so it is applied once rather than re-asserted over a user who turned it off. |
| `autostart_show_window_next_start` | Shell-written, cleared on the next launch. Set by an update that restarts while a window is open, so the relaunch shows one even in a login session. |

**Gateway prerequisite:** CORS must allow the shell's origins —
`tauri://localhost` (macOS/Linux) and `http://tauri.localhost` (Windows) —
including exposing the `Access-Token` and `Refresh-Token` response headers.
Without it the UI renders and every data call fails.

## What the shell owns

- **Login** — the OAuth flow runs in a dedicated window; tokens are exchanged
  natively because the response headers carrying them are invisible to a
  cross-origin webview fetch.
- **Tokens** — refresh is scheduled from the JWT expiry and runs on a background
  poll, so a session survives the webview being idle in the tray. The shell is
  the *only* refresher: refresh tokens rotate, so two would invalidate each other.
- **Notifications** — a second, Rust-owned NATS connection delivers OS
  notifications and a badge while the window is hidden, and routes clicks back
  into the UI (including clicks that cold-start the app).
- **Start at login** — registered per-user (a launchd agent on macOS, an `HKCU`
  Run entry on Windows, an XDG autostart entry on Linux) and on by default. A
  login start carries `--autostart` and stays in the tray; the window is only
  built when the user asks for one, from the tray or a notification — or when an
  update restarted a session that already had one open.

See [docs/auth-and-notifications.md](docs/auth-and-notifications.md) for how
those work, and [docs/architecture.md](docs/architecture.md) for the shell
itself — windows, the bundle pipeline, and the security model.

## Layout

```
Makefile              build entry point
scripts/
  build-web.sh        clone/build the frontend export → www/
  make-placeholder-web.mjs
src-tauri/
  src/lib.rs          windows, tray, config, NativeAuth commands
  src/tokens.rs       token custody, refresh, rotation events
  src/nats.rs         NATS-over-WebSocket connection lifecycle
  src/notifications.rs  subscription, dispatch, click delivery
  src/notification_actions.rs  what a notification button does, and its session gate
  src/chat_api.rs     the REST calls a button completes without a window
  src/macos_un.rs     UNUserNotificationCenter backend
  src/windows_toast.rs  toast XML + the button-argument codec
  src/windows_activator.rs  COM activator the toast buttons activate
  src/autostart.rs    start-at-login registration and the headless start
  src/updater.rs      silent startup update, background poll, update commands
www/                  staged frontend bundle (generated, git-ignored)
```
