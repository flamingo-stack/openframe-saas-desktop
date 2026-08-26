# OpenFrame Console

Native desktop shell for OpenFrame — a [Tauri 2](https://tauri.app) application
that embeds the [openframe-oss-frontend](https://github.com/flamingo-stack/openframe-oss-frontend)
static export and adds what a browser tab cannot do: OS notifications that keep
arriving while the app sits in the tray, a session that survives long idle, and
a system-browser OAuth flow.

**No UI code lives in this repo.** The interface is the frontend's static export,
staged into `www/` at build time and embedded in the binary. What is here is the
Rust shell around it.

Supported: macOS, Windows. Linux builds, but has neither an OS-notification
backend nor any URL-scheme registration — so notifications never arrive and the
OAuth callback has no way back into the app, leaving sign-in unable to complete.

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
and URL-scheme activation behave differently or not at all. **Sign-in is one of
them**: the OAuth callback comes back through the OS on `openframe-console://`,
which macOS routes only to a bundled `.app` it has seen (launch it once from
`/Applications`) and Windows only to an install that has written its HKCU key.

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
from the user's email, and the shell is told that tenant's origin once login
succeeds.

The shared host is baked in at build time and can be overridden per install by
`shared_host` in `config.json`, which lives in the OS app-config directory:

| OS | Path |
|---|---|
| macOS | `~/Library/Application Support/com.openframe.console/config.json` |
| Windows | `%APPDATA%\com.openframe.console\config.json` |
| Linux | `~/.config/com.openframe.console/config.json` |

> **Renamed from OpenFrame Desktop (2026-08-26)**, identifier included:
> `com.openframe.desktop` → `com.openframe.console`. The identifier keys the
> paths above, the log directory, the macOS notification authorization, the
> launchd label and the Windows AUMID, so a pre-rename install comes back **signed
> out** (its `tokens.json` stays behind at the old path) and re-prompts for
> notification permission. That was acceptable because the app had only ever been
> distributed to test users.
>
> Two leftovers are *not* inert, and a renamed build deletes both on first run.
> The **login registration** is named after the identifier, so an abandoned one
> keeps launching the old build every login — `autostart::remove_legacy_registration`
> clears the macOS/Linux file and the Windows `Run` value written under
> `OpenFrame Desktop`. On Windows the **old `openframe-desktop://` scheme key**
> still points at the old exe, so a toast left in the Action Center from before
> the rename would start the build it replaced — `remove_legacy_url_scheme`
> clears that and the stale `AppUserModelId` key. Both can be removed once no
> machine still has a pre-rename build on it. Nothing else is migrated: the old config directory is left where it
> is, and `installed-agent` rows written before the rename stay filed under the
> old `AGENT_TYPE`.
>
> **Start at Login is reset by the rename, not carried over.** `autostart_configured`
> lived in the old config directory, so the reconcile sees a renamed install as a
> first launch and applies the default — which re-enables start-at-login for anyone
> who had turned it off on a pre-rename build. It cannot be inferred from what the
> new identity can see: an opted-out pre-rename install and a fresh install both
> present as "no registration and no record". The old `config.json` *would* separate
> them, but reading it is the migration this rename deliberately does not do. The
> user's own toggle is authoritative from that point on.

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

- **Login** — the OAuth flow runs in the user's default browser, so an SSO
  session they already have there is reused instead of asking for credentials
  again, and comes back on the `openframe-console://auth` scheme this app
  registers with the OS. Tokens are then exchanged natively, because the response
  headers carrying them are invisible to a cross-origin webview fetch.
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
