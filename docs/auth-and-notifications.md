# Auth and notifications

Two things the shell owns rather than the webview: the **session** (token custody
and refresh) and the **background notification plane** (a Rust-owned NATS
connection that outlives the webview's idle state).

## Why the shell owns them

Moving the webview's whole NATS connection into Rust — one connection owned by
the shell, consumed over IPC — was considered and rejected. The frontend's
realtime goes through a shared `NatsProvider` (notification drawer, dialog
chunks, assistant streams, connection status); putting that behind IPC means a
transport abstraction in the shared core library and regression-testing every
realtime flow, to gain background delivery for streams nothing consumes while
hidden. A localhost NATS proxy was rejected too: the webview still owns the
protocol session, so it dies with the webview anyway.

Instead the planes are split. The webview keeps its interactive connection, which
is idle-tolerant by design — the same as a throttled browser tab. The shell owns
what must survive idle:

1. **Tokens.** This is a correctness requirement, not just resilience. Refresh
   tokens rotate, so the moment the shell holds any long-lived authenticated
   connection there must be exactly **one** refresher. Two independent refreshers
   sharing a refresh token invalidate each other, producing random logouts.
2. **A second NATS connection for OS notifications**, mirroring the mobile
   architecture where the webview connection is the interactive plane and the
   push service is the background plane.

## Token lifecycle (`src-tauri/src/tokens.rs`)

- **Custody.** `tokens.json` in the app config directory, written owner-only via a
  temp-file rename so a concurrent reader never sees a half-written file. A torn
  read would deserialize as "no session" and tear down the notification
  subscription.
- **Freshness.** The access token's JWT `exp` is decoded without signature
  verification — it is a scheduling hint about our own token, nothing more. A
  refresh is due when a refresh token exists and the access token is missing or
  expires within 60 seconds.
- **Refresh.** `POST {base}/oauth/refresh` with the `Refresh-Token` header;
  rotated tokens come back on the `Access-Token`/`Refresh-Token` response
  headers. `tenantId` is omitted — the gateway resolves the tenant from the token.
  Base resolution is the shared auth host, falling back to the learned tenant
  host.
- **Single-flight.** A mutex serializes refreshes. Forced refreshes (delegated
  from a webview 401) are dampened by comparing the pre-call access token: if it
  changed while waiting for the lock, a parallel refresh already rotated it.
- **Outcome contract**, consumed by the frontend's delegation: rotated → tokens;
  session over (401/403) → tokens cleared, empty set returned; transient failure
  (network, 5xx, no headers) → error, stored tokens kept.
- **Background loop.** A 30-second freshness poll rather than a scheduled timer,
  because polling self-heals across laptop sleep/wake.
- **Push to webview.** Every shell-side token change emits
  `native-auth:token-update` to all bundle windows, so the webview's cache mirrors
  rotations and session death.
- **Fresh on read.** `getTokens` refreshes first when the token is expiring, so
  the app resumes instantly after long idle instead of a 401→refresh round trip.

> **The refresh POST sends a body on purpose.** reqwest/hyper omit
> `Content-Length` for bodyless POSTs — *and* for an explicit zero-length body.
> Over HTTP/1.1, some CDN front ends reject `Content-Length`-less POSTs with
> `411 Length Required` before they ever reach the gateway. Browsers always send
> `Content-Length: 0`, so the webview path never hit this. Symptom when it bites:
> every shell refresh fails, the access token expires, the webview's delegated
> refresh fails the same way, and the app force-logs-out after 15–30 minutes in
> the tray. The fix is a benign `{}` body, which the endpoint ignores.

### NativeAuth surface

The bridge is shell-agnostic; the frontend feature-detects each method.

| Method | Notes |
|---|---|
| `start` / `exchangeTicket` | OAuth in a shell-owned window, then a native token exchange — the response headers carrying tokens are invisible to a cross-origin webview fetch. |
| `get` / `set` / `clearTokens` | `set` merges per field: a rotation response may carry one token or both. |
| `refreshTokens` | Optional. Its presence means "the shell owns refresh", and the webview stops calling the refresh endpoint entirely. |
| `setTenantHost` | Persists the login-learned tenant origin shell-side, so shell networking has a gateway without depending on webview storage. |

Login accepts **any** callback URL carrying a dev ticket, checked in both the
navigation and page-load callbacks — server-side redirect hops do not reliably
surface as navigation-policy callbacks. Closing the window before capture rejects
as a user cancellation.

## Notification plane

### Connection (`src-tauri/src/nats.rs`)

NATS over WebSocket against the gateway, where the bearer rides on the connect
URL — so the connection expires with the token. The fork's `auth_url_callback` is
invoked when the server rejects the current token, and re-asks for a URL; that is
where the shell's token refresh plugs in, so a reconnect after hours in the tray
dials with a fresh bearer.

- Credentials are read before every dial. A capped exponential backoff, plus a
  separate guard on consecutive auth failures — without it a revoked token
  re-dials with full TLS+WS handshakes every ~200 ms forever.
- On every `Connected` the subscription is re-established, because the subject
  changes when the signed-in user does.
- Sign-in on the same tenant re-subscribes on the live connection; a tenant switch
  tears the connection down and re-dials, since the connection itself is then
  wrong. A single-runner guard prevents overlapping switches from starting two
  dial loops.

### Subscription and dispatch (`src-tauri/src/notifications.rs`)

- Subject `user.<userId>.notification`, where the user id is the `userId` claim of
  the access token (`sub` is the email).
- Envelope contract: anything with a `title` is displayable; the description is
  truncated for the notification body.
- Notifications fire **only when the main window is hidden or unfocused**. The
  webview's own subscription keeps driving the in-app drawer — duplicate delivery,
  different sinks. A badge accumulates while hidden and clears on focus.
- The click payload is the envelope's routing `context`, narrowed to the fields
  the frontend's route mapping reads. The rest of a context can be arbitrarily
  large, and it has to fit inside a Windows activation URI.
- Every logout path tears the subscription down — including a session death the
  shell detects itself, which the webview may not notice for hours while idle in
  the tray. Otherwise the previous user's notification content would keep
  arriving, and reach whoever signs in next.

### Click delivery

`notify-rust` was replaced because its click callback only fires for a live banner
in the current process: a click from the notification centre, or one that
cold-starts the app, went nowhere.

- **macOS, bundled builds** (`macos_un.rs`) — `UNUserNotificationCenter` with a
  delegate. The payload rides in the notification's `userInfo`, so every click is
  reported: live banner, notification centre hours later, or a cold start.
  Permission is requested once, after the first successful subscribe, so an
  unauthenticated shell never prompts.
- **Windows** (`windows_toast.rs`) — hand-built toast XML using protocol
  activation, because the available toast crates only support in-process
  foreground activation, which dies with the process. Clicks open a custom URI
  registered under `HKCU` at startup; a warm click reaches the running instance
  through single-instance argv forwarding, a cold click launches the app with the
  URI in argv.
- **macOS dev builds and Linux** have no backend. The notification APIs abort
  without a bundle identifier, so test notifications against a built app.

A cold-start click fires long before the webview's listener mounts, so all paths
funnel through a stash: the payload is parked until the webview signals readiness,
which drains it and opens the gate for direct delivery. The gate closes again on
each new document and when the main window is destroyed, because the listener does
not survive either.

## Known gaps

- No feature-flag gate on notifications. A webview-independent plane cannot wait
  on webview state; silence at the subject level is the effective gate.
- A user switch **on the same tenant** re-subscribes over a connection the gateway
  authorized with the previous user's bearer. Whether that is rejected depends on
  the gateway's subject-permission model, which is not verified here.
- Moving the interactive streams behind a shared transport abstraction, so the
  shell owns a single connection, remains possible but unjustified — nothing
  consumes those streams while hidden.
