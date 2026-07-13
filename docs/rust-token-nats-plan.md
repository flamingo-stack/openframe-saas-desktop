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

## Step 2 — `openframe-nats-connector` crate + notification plane ✅ (as-built, 2026-07-10)

### The crate (`~/flamingo/openframe-nats-connector`, own git repo, local-only)

Extraction of openframe-chat's `nats_bridge/connection.rs`: connect/reconnect
state machine, jittered fast-then-exponential backoff, the auth-failure guard
(the fork resets its attempt counter after a successful `auth_url_callback`,
so a revoked token would hot-loop without it), URL build + token masking.
Generalized behind:

- `trait ConnectSource { async fn server_url() -> Option<String>; async fn token() -> Option<String>; }`
  — asked before every (re)connect, so a shell token refresher plugs in here.
  (The fork requires the auth callback future to be `Sync`; the crate hops
  through `tokio::spawn` so sources can run non-`Sync` work like HTTP refresh.)
- `ConnectorConfig` (client name, ws path, NATS user/pass; defaults =
  `machine`/`""`/`/ws/nats-api`).
- `on_connected` hooks (JetStream consumers die with the connection; plain
  SUBs are replayed by async-nats itself) + a `watch`-based status channel.
- Runtime-agnostic: `run()` is a plain future; uses the `log` crate.

Consumed by the desktop as a **sibling-checkout path dep**
(`../../openframe-nats-connector`, same convention as `FRONTEND_DIR`); flips
to a git dependency once pushed to the org.

**Deferred: the openframe-chat refactor onto the crate.** Chat's CI can't
resolve a path/unpushed dep — gated on pushing the crate repo. The agent
(`openframe-client`) can follow later. Until then chat keeps its original copy.

### Desktop notification plane (`src-tauri/src/notifications.rs`)

- Second NATS connection (webview keeps its own for interactive streams):
  `ShellConnectSource` = `learned_host || host` + `tokens::ensure_fresh()` —
  reconnect-after-sleep dials with a refreshed bearer inline.
- Subject `user.<userId>.notification`; **userId = the `userId` claim** of the
  access token (verified against a live gateway token; `sub` is the email).
  Router replaced when the signed-in user changes; kept across reconnects.
- Envelope contract (same as chat's): anything with a `title` is displayable;
  `description` truncated to 140 chars. Toasts fire only when the main window
  is hidden/unfocused (`should_notify`) — the webview's own subscription keeps
  driving the in-app drawer; duplicate delivery, different sinks.
- notify-rust toast (dev builds borrow `com.apple.Terminal`'s identity —
  unsigned binaries can't post as themselves), dock badge while hidden,
  click → show window + `notification:click` event with the **raw envelope**;
  window-focus within 30s also replays a pending click (chat's heuristic).
- Frontend (`feat/native-shell-token-lifecycle`): `onNativeNotificationClick`
  in native-shell.ts; `resolveNatsNotificationRoute` in
  notification-navigation.ts (same context mapping as the drawer, wire-shape
  input); `NativeShellInitializer` pushes the route (fallback
  `/notifications`). Drawer-only actions (mingo sidebar) have no URL yet.

### Known limitations

- "Switch Instance"/sign-out doesn't force-drop an established connection —
  the old-user subscription lives until the next natural reconnect (token
  rotation will refuse re-auth). Add a connector `disconnect()` if it matters.
- No feature-flag gate on toasts (chat has one, pushed from its webview): a
  webview-independent plane can't wait for webview state. Subject-level
  silence is the effective gate.

## Step 3 — full IPC transport (only if ever needed)

Move interactive streams behind a core-lib `NatsTransport` abstraction
(nats.ws on web, shell IPC in shells) so Rust owns the single connection.
Do this only when something concrete requires streams while hidden; step 2's
crate is the foundation either way.

## Crate/repo placement

`openframe-nats-connector` as a Cargo **git dependency** (org precedent: the
nats.rs fork, `openframe-frontend-core` on npm) — either its own repo or a
crate dir in openframe-oss-tenant. Path deps rejected (machine-specific).
