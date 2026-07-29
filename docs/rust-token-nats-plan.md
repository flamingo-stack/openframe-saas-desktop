# Rust-owned token lifecycle + background NATS — plan

> Goal: make the desktop session (tokens) and background delivery (NATS
> notifications) independent of the webview's idle state, without forking any
> frontend realtime code. Three steps; **step 1 is implemented** (2026-07-10).
> Companion docs: [native-apps-strategy.md](./native-apps-strategy.md) §3
> (shell architecture), frontend `docs/static-export-migration.md`.

## Why this shape (decision record)

A full move of the webview's NATS connection to Rust (openframe-chat's model:
Rust owns the only connection, webview consumes over IPC) was **rejected** for
the desktop: chat needed two narrow consumers, but `openframe-frontend`'s
realtime goes through core-lib `NatsProvider` (notifications drawer, dialog
chunks, mingo, connection status) — moving that behind IPC means a transport
abstraction in `openframe-frontend-core` and regression-testing every realtime
flow, to gain background delivery for streams nothing consumes while hidden. A
localhost NATS proxy was also rejected (webview still owns the protocol
session, so it dies with the webview anyway).

Instead: **split-plane**. The webview keeps its interactive NATS connection
(idle-tolerant by design — same as a throttled browser tab). The shell owns
what actually must survive idle:

1. **Tokens** (step 1, done) — also a *correctness* requirement, not just
   resilience: refresh tokens rotate, so the moment the shell holds any
   long-lived authed connection there must be exactly ONE refresher. Two
   independent refreshers (webview + shell) sharing one refresh token
   invalidate each other → random logouts.
2. **A second, Rust-owned NATS connection for OS notifications** (step 2) —
   mirrors mobile's architecture, where the webview NATS is the interactive
   plane and APNS is the background plane.

## Step 1 — token lifecycle to Rust ✅ (as-built)

### Rust (`src-tauri/src/tokens.rs`)

- **Custody**: `tokens.json` (0600) in the app config dir (moved here from
  lib.rs). OS-keychain storage remains a hardening item.
- **Freshness**: JWT `exp` decoded from the access token (no signature check —
  schedule hint only). `needs_refresh` = refresh token present AND access
  token missing/expiring within 60s.
- **Refresh**: `POST {base}/oauth/refresh` with the `Refresh-Token` header;
  rotated tokens read from the `Access-Token`/`Refresh-Token` **response
  headers** (BFF sends them when `dev-ticket-enabled`; verified in
  `OAuthBffController.buildNoContentWithCookies`). `tenantId` is omitted — the
  BFF's `refreshTokensByLookup` resolves the tenant from the token itself
  (verified). Base resolution: `shared_host || learned_host || host` from
  config.json — this **fixes oss-mode shells**, where the webview's
  relative-path fallback resolved to the asset origin and refresh always
  failed.
- **Single-flight**: a tokio mutex serializes refreshes. Forced refreshes
  (delegated webview 401s) are dampened by comparing the pre-call access
  token — if it changed while waiting for the lock, a parallel refresh already
  rotated.
- **Outcome contract** (consumed by the frontend delegation):
  rotated → tokens; session over (BFF 401/403) → cleared + empty; transient
  (network/5xx/no headers) → error, stored tokens kept.
- **Background loop**: 30s freshness poll (poll, not timer — self-heals across
  laptop sleep/wake; chat's token-watcher precedent).
- **Push to webview**: every Rust-side token change emits
  `native-auth:token-update` (full token set; empty = session over) to all
  bundle windows (`main` + `child-*`).
- **Fresh-on-read**: `NativeAuth.getTokens` now refreshes first when expiring,
  so the app resumes instantly after long idle instead of a 401→refresh dance.

### New NativeAuth plugin surface (shell-agnostic contract)

| Method | Status | Notes |
|---|---|---|
| `refreshTokens()` | desktop ✅ / mobile ❌ | Optional; presence = "shell owns refresh". Resolves with stored tokens after attempt (empty = session over); rejects on transient failure. |
| `setTenantHost({origin})` | desktop ✅ / mobile ❌ | Persists the login-learned tenant host shell-side (`learned_host` in config.json); called next to `storeTenantHost()` in native-login. |

Mobile parity is mechanical (URLSession refresh + Keychain in
`NativeAuthPlugin.swift`) and optional — the frontend feature-detects.

### Frontend changes (committed on branch `feat/native-shell-token-lifecycle`)

> That branch = latest `main` + the rebased mobile-push commit + the host-less
> boot fix + the changes below. `hotfix/mobile-push` itself is untouched.

- `native-shell.ts` — optional `refreshTokens`/`setTenantHost` on the plugin
  interface; `onNativeTokenUpdate()` listener helper (Tauri event transport,
  no-op elsewhere).
- `token-refresh-manager.ts` — `executeRefresh` delegates to
  `plugin.refreshTokens()` when present; the webview stops calling
  `/oauth/refresh` in such shells entirely.
- `token-store.ts` — `initTokenStore` subscribes to `native-auth:token-update`
  and mirrors rotations (and session death) into the in-memory cache.
- `native-login.ts` — pushes the learned host via `setTenantHost` (best-effort).

### Verification status

- `cargo check` clean; frontend `tsc --noEmit` clean; export bundle + release
  `.app` build green. Native login → dev-ticket exchange → tokens.json worked
  end-to-end against test-dev (2026-07-10).
- **Field bug found & fixed (2026-07-10): `411 Length Required`.** The Rust
  refresh POSTed without `Content-Length`: reqwest/hyper omit it for bodyless
  POSTs — **and even for an explicit zero-length `.body("")`** (the first fix
  attempt, disproved in the field and by a scratch-crate probe). Without the
  `http2` feature reqwest speaks HTTP/1.1, and GCP's front end rejects
  `Content-Length`-less POSTs with 411 before they reach the gateway. Browsers
  always send `Content-Length: 0`, so the webview path never hit it. Every
  shell refresh failed → access token expired (15 min TTL) → the webview's
  delegated refresh also 411'd → frontend force-logout after ~15-30 min in the
  tray. Working fix (probe-verified: 401-for-dummy-token, i.e. reaches auth):
  a benign `.body("{}")` — the BFF takes no request body. Lessons: mirror
  browser transport details, not just headers; and verify claimed fixes
  against the real front end, not by reasoning about the HTTP stack.
- Remaining to verify: a full login → idle past access-token expiry → shell
  rotation (`[tokens] refreshed` in the log) → resume-without-relogin cycle.

## Step 2 — NATS connection layer + notification plane ✅ (as-built, 2026-07-10; revised 2026-07-28)

### Connection layer (`src-tauri/src/nats.rs`)

Port of openframe-chat's `nats_bridge/connection.rs`: connect/reconnect state
machine, jittered fast-then-exponential backoff, the auth-failure guard (the
fork resets its attempt counter after a successful `auth_url_callback`, so a
revoked token would hot-loop without it), URL build + token masking.

- Credentials are asked for before every (re)connect: `learned_host || host`
  and `tokens::ensure_fresh()`, so a reconnect after sleep dials with a
  refreshed bearer inline. (The fork requires the auth callback future to be
  `Sync`; ours isn't — HTTP refresh — so it hops through `tokio::spawn`.)
- `/ws/nats-api`, NATS-level creds `machine`/`""`; the real auth is the URL
  bearer the gateway validates at WS upgrade.
- On every Connected it re-runs `notifications::ensure_subscription` (plain
  SUBs are replayed by async-nats itself, but the subject changes when the
  signed-in user does).

**History:** this started as a separate `openframe-nats-connector` crate
(generic `ConnectSource` trait, `ConnectorConfig`, `on_connected` hook list,
status watch channel) consumed as a sibling-checkout path dep. The crate was
never pushed, so chat could never adopt it and the desktop was the only
consumer; it was **inlined here on 2026-07-28** and the generic surface the
single consumer never used was dropped. Extract again only when a second
native shell actually needs it — from a pushed repo, not a path dep.

### Desktop notification plane (`src-tauri/src/notifications.rs`)

- Second NATS connection (webview keeps its own for interactive streams).
- Subject `user.<userId>.notification`; **userId = the `userId` claim** of the
  access token (verified against a live gateway token; `sub` is the email).
  Router replaced when the signed-in user changes; kept across reconnects.
- Envelope contract (same as chat's): anything with a `title` is displayable;
  `description` truncated to 140 chars. Notifications fire only when the main
  window is hidden/unfocused (`should_notify`) — the webview's own
  subscription keeps driving the in-app drawer; duplicate delivery, different
  sinks. Dock/taskbar badge while hidden, cleared on main-window focus.
- Click payload = the envelope's **`context`** object (all the frontend route
  mapping reads, and small enough for the Windows activation URI), emitted as
  `notification:click`.

### Click delivery (aligned with chat's #2115 refactor, 2026-07-28)

notify-rust is gone: its click callback only fires for a *live* banner in the
*current* process, so a click from Notification Center or one that cold-starts
the app went nowhere — which is what the old "window focused within 30s
replays the pending click" heuristic was papering over. Per platform now:

- **macOS bundled builds** (`src-tauri/src/macos_un.rs`):
  `UNUserNotificationCenter` + a delegate; the payload rides in the
  notification's `userInfo`, so `didReceiveNotificationResponse` reports every
  click — live banner, Notification Center hours later, or a cold start.
  Permission is requested once, after the first successful subscribe (i.e.
  only for signed-in shells).
- **Windows** (`src-tauri/src/windows_toast.rs`): hand-built toast XML with
  `activationType="protocol"` (tauri-winrt-notification only does in-process
  foreground activation). Clicks open `openframe-desktop://notify?context=…`,
  registered under `HKCU\Software\Classes` at startup; a warm click reaches
  the running instance via single-instance argv forwarding, a cold click
  launches us with the URI in argv.
- **macOS dev builds** (`tauri dev` runs an unbundled binary — UN APIs abort
  without a bundle id) **and Linux: no OS notifications.** The old
  `com.apple.Terminal` identity-borrowing dev workaround died with notify-rust;
  test toasts against a `tauri build` bundle.

Cold-start clicks fire long before React mounts, so all paths funnel through
`deliver_click` → `emit_or_stash`: the payload is parked until the webview
calls `take_pending_notification_click`, which opens the gate and drains it.

### Frontend counterpart (openframe-frontend)

`onNativeNotificationClick` + `takeNativeStartupNotificationClick` in
native-shell.ts; `resolveNatsNotificationRoute` in notification-navigation.ts
(same context mapping as the drawer, wire-shape input); `NativeShellInitializer`
registers the listener, then drains the startup click and pushes the route
(fallback `/notifications`). Drawer-only actions (mingo sidebar) have no URL yet.

### Known limitations

- Tray "Sign Out" doesn't force-drop an established connection —
  the old-user subscription lives until the next natural reconnect (token
  rotation will refuse re-auth). Add a `disconnect()` if it matters.
- No feature-flag gate on notifications (chat has one, pushed from its
  webview): a webview-independent plane can't wait for webview state.
  Subject-level silence is the effective gate.
- The webview-ready gate is not reset when the main window is recreated
  (`set_tenant_host` / `switch_instance`), so a click landing during that
  reload is emitted into a not-yet-mounted listener. Only reachable by
  clicking a notification mid-instance-switch.

## Step 3 — full IPC transport (only if ever needed)

Move interactive streams behind a core-lib `NatsTransport` abstraction
(nats.ws on web, shell IPC in shells) so Rust owns the single connection.
Do this only when something concrete requires streams while hidden; step 2's
connection layer is the foundation either way.
