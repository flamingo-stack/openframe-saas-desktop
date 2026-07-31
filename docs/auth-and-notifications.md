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
  expires within 120 seconds. That margin is squeezed from both sides: it must
  span several poll ticks (App Nap coalesces a hidden app's timers, and a margin
  the poll can skip entirely lets the access token die), yet stay a small
  fraction of the token's 900-second life, because every rotation is a chance to
  lose the session — see the hazard note below.
- **Refresh.** `POST {base}/oauth/refresh` with the `Refresh-Token` header;
  rotated tokens come back on the `Access-Token`/`Refresh-Token` response
  headers. `tenantId` is omitted — the gateway resolves the tenant from the token.
  Base resolution is the shared auth host, falling back to the learned tenant
  host.
- **Single-flight.** A mutex serializes refreshes, and every write to
  `tokens.json` takes it — including the webview's `setTokens`, which would
  otherwise be able to land a read-merge-write on top of a rotation that
  completed in between and restore a spent refresh token.
- **Retries, and what may not be retried.** A refresh is not idempotent, so the
  three failure modes are kept apart: *unreached* (DNS, connect, TLS — the
  gateway never saw it) is retried after 1s and 3s; *rejected* (401/403) is final
  on the first answer, since re-presenting the same token cannot change it; and
  *unknown* (a broken or timed-out response, a 5xx, a 200 with no token headers)
  is never retried, because a retry can only turn "maybe rotated" into a
  rejection.
- **Outcome contract**, consumed by the frontend's delegation: rotated → tokens;
  session over (rejected, or a rotation that could not be written to disk) →
  tokens cleared, empty set returned; unreached/unknown → error, stored tokens
  kept. The `refreshTokens` command softens the last case before the webview sees
  it: it resolves with the stored tokens instead of rejecting, because the
  frontend maps every refresh that yields no access token to `forceLogout`, and
  that deletes a refresh token which is good for days. Deciding a session is over
  stays here, where custody is.
- **Background loop.** A 30-second freshness poll rather than a scheduled timer,
  because polling self-heals across laptop sleep/wake. It backs off while
  refreshes keep failing, up to 16× the interval.
- **Wake and window-show.** macOS `NSWorkspaceDidWakeNotification`
  (`macos_wake.rs`) and showing the main window both nudge a refresh. A machine
  that slept through the access token's whole life comes back with every overdue
  timer firing at once, the webview's among them; getting ahead of that is
  cheaper than letting its first 401 drive the recovery.
- **Push to webview.** Every shell-side token change emits
  `native-auth:token-update` to all bundle windows, so the webview's cache mirrors
  rotations and session death.
- **Fresh on read.** `getTokens` refreshes first when the token is expiring, so
  the app resumes instantly after long idle instead of a 401→refresh round trip.
  `ensure_fresh` (awaits the rotation) is for callers with a person waiting;
  background reconnect loops use `refresh_soon` (fires it off) instead.

> **Every rotation is a chance to lose the session.** The authorization server
> issues rotating refresh tokens (`reuseRefreshTokens(false)`) and retires the
> presented one the moment it mints the next, with no grace window. So a refresh
> whose *response* is lost — connection broken mid-answer, which is what a Wi-Fi
> transition looks like — leaves this side holding a token the server has already
> retired. There is no client-side recovery: the next refresh is rejected and the
> user signs in again. Observed on 2026-07-29 at 23:04, five seconds apart: a POST
> that went out with no answer back, then a 401 on the same token.
>
> What the shell can do, and does: never retry an unknown outcome, keep rotations
> as infrequent as the margin allows, keep them out of reconnect loops (which run
> exactly when the network cannot deliver an answer), and name the case in the log
> when a rejection follows a lost answer instead of reporting it as a revoked
> session. What it cannot do is make a lost rotation recoverable. That needs the
> authorization server to accept the previous refresh token for a few seconds
> after rotation and re-issue the same pair — the standard mitigation for this
> hazard, and the only fix that removes it.

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
| `refreshTokens` | Optional. Its presence means "the shell owns refresh", and the webview stops calling the refresh endpoint entirely. Rejects only when there is genuinely nothing to refresh with — a rejection is a logout on the frontend side. |
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
- The dial reads the *stored* bearer and kicks a rotation off in the background
  rather than awaiting one. Awaiting it put a rotation inside this loop, which
  runs precisely when the network has just broken — the one condition under which
  a rotation's answer goes missing and the session is unrecoverable. A rejected
  dial costs a backoff; a lost rotation costs the login.
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
  the frontend's route mapping reads plus `approvalRequestId`, which the macOS
  action buttons need. The rest of a context can be arbitrarily large, and it has
  to fit inside a Windows activation URI.
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

> **A built `.app` is not enough on macOS — it has to be signed.** `tauri build`
> ships the bundle carrying only the linker's ad-hoc signature (`codesign -dv`
> reports `flags=…(adhoc,linker-signed)` and an `Identifier` derived from the
> binary name, not `com.openframe.desktop`). UserNotifications refuses to serve
> an app it cannot identify: `requestAuthorization` **and** every
> `addNotificationRequest` fail with `UNErrorDomain error 1`
> (`UNErrorCodeNotificationsNotAllowed`), which reads in the log as
> "authorization failed" followed by "request rejected" for each notification —
> the plane is otherwise working, envelopes arrive and are dispatched. Signing the
> bundle with any real identity fixes it:
> `codesign --force --sign "Apple Development: …" "path/to/OpenFrame Desktop.app"`, after
> which `codesign -dv` shows `Identifier=com.openframe.desktop`. Every fresh build
> needs it again — signing runs against the produced artifact, which is why it is
> not in the Makefile.

A cold-start click fires long before the webview's listener mounts, so all paths
funnel through a stash: the payload is parked until the webview signals readiness,
which drains it and opens the gate for direct delivery. The gate closes again on
each new document and when the main window is destroyed, because the listener does
not survive either.

### Action buttons (macOS only)

Some notifications complete their work from the banner, with no window and no
webview: **Approve / Reject** on an AI tool-approval request, **Reply** on a Mingo
message. Both are plain authenticated REST calls, so the whole decision runs in
Rust (`chat_api.rs`) against the tenant host, with a bearer from
`tokens::ensure_fresh`.

- Three categories are registered at init (`setNotificationCategories` replaces
  the whole set, so it is one call): approval, message, and the default one-button
  set every other notification keeps. Which one a notification gets is derived
  from its click payload — and only when the id the button needs actually
  survived the payload projection, because an Approve that can resolve nothing is
  worse than no Approve.
- Only the buttons run in the background. A body click or "Open" is still the
  existing click path, and so is any action identifier this build does not know.

> **The session, not the notification, is the authority.** A banner can sit in
> Notification Center for days, across a sign-out or a different user signing in
> on the same Mac. So every background action first resolves a live session
> (`chat_api::active_session`: tokens that refresh, an access token still inside
> its `exp`, a `userId` claim) and refuses unless that session is **the same user
> the notification was delivered to** — the subscription's user id is stamped into
> `userInfo` at fire time for exactly this comparison. Approve additionally
> carries `AuthenticationRequired`, but that is only macOS's device-lock gate: on
> an unlocked Mac it prompts for nothing, which is why it is not the gate that
> matters here.

There is no window to report into afterwards, so the outcome comes back as another
notification: a plain confirmation on success, and on failure a re-post of the
original — same category, same payload, the reason in the body — so the buttons
are still there to retry with. A failed reply carries the typed text back with it,
since responding to a notification clears the inline field.

Windows gets none of this: its toasts activate a URI, and running an action without
foregrounding the app would need a COM `INotificationActivationCallback`.

## Known gaps

- No feature-flag gate on notifications. A webview-independent plane cannot wait
  on webview state; silence at the subject level is the effective gate.
- A user switch **on the same tenant** re-subscribes over a connection the gateway
  authorized with the previous user's bearer. Whether that is rejected depends on
  the gateway's subject-permission model, which is not verified here.
- Moving the interactive streams behind a shared transport abstraction, so the
  shell owns a single connection, remains possible but unjustified — nothing
  consumes those streams while hidden.
- A notification action taken while the app is **quit** relaunches it, and launch
  opens the main window — so that path completes the action but does not stay
  invisible. Suppressing the window would mean knowing at setup time that a
  response is coming, which the delegate only reports afterwards.
