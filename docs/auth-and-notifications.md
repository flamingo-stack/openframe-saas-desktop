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
- **Background loop.** A 30-second freshness poll rather than a scheduled timer.
  It backs off while refreshes keep failing, up to 16× the interval. Note that it
  does *not* self-heal across sleep, contrary to what this file used to say:
  `tokio::time::sleep` runs on `Instant`, which is `CLOCK_UPTIME_RAW` on macOS
  and stops while the machine is out, so on a laptop cycling through maintenance
  DarkWake a single tick can take over an hour of wall clock. Freshness itself is
  decided on the wall clock, so a late tick still refreshes correctly.
- **Wake and window-show.** macOS `NSWorkspaceDidWakeNotification`
  (`macos_wake.rs`) and showing the main window both nudge a refresh. A machine
  that slept through the access token's whole life comes back with every overdue
  timer firing at once, the webview's among them; getting ahead of that is
  cheaper than letting its first 401 drive the recovery. Neither covers much on
  its own — measured over six days of logs, wake fired five times and window-show
  once — and `NSWorkspaceDidWakeNotification` is not posted for DarkWake at all.
- **Wake settling.** `spawn_wake_watch` detects resume by watching the wall clock
  run ahead of the monotonic one, and no rotation goes out until the machine has
  been awake 15 seconds. This is the guard for the failure below: a laptop idling
  overnight is awake 2-9 seconds every ~15 minutes, and a rotation fired into one
  of those windows cannot get its answer back. It is also the only sleep signal
  that works on Windows, which has no wake observer in this shell.
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
> user signs in again.
>
> Observed end to end on 2026-08-01. The machine was idling on battery in
> maintenance DarkWake, awake 2-9 seconds every ~15 minutes. The access token died
> inside one of those sleeps; at the next resume the background poll asked for a
> rotation and POSTed at 01:33:30 UTC, the same second the machine went back to
> sleep. The answer never arrived. Thirty-two minutes later the next caller
> presented the same token and got a 401, and custody was cleared. Note where the
> POST came from: the poll, not the reconnect path this design had already moved
> rotations out of.
>
> What the shell can do, and does: refuse to rotate until the machine has been
> awake long enough for an answer to come back (`spawn_wake_watch`), hold the
> token back for a cooldown after a rotation whose fate is unknown rather than
> letting the next caller cash the ambiguity in as a 401, bound the request on the
> wall clock so a POST the machine sleeps through cannot hold the single-flight
> lock for fifteen minutes, never retry an unknown outcome, keep rotations as
> infrequent as the margin allows, and name both the case and the caller in the
> log when a rejection follows a lost answer.
>
> What it cannot do is make a lost rotation recoverable. That needs the
> authorization server to accept the previous refresh token for a few seconds
> after rotation and re-issue the same pair — the standard mitigation for this
> hazard, and the only fix that removes it. Everything above only narrows the
> window.

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
- With nobody signed in, the callback **parks** instead of returning an error.
  Returning one does not stop the fork — it logs and re-enters its connect loop,
  which is how one overnight logout produced a full TLS+WS handshake every 30
  seconds for two days. Nor can the loop be stopped from outside: between
  connections the connector task sits inside its own connect loop and is not
  polling the client's command channel, so dropping the `Client` is unobserved.
  Blocking in the callback suspends the task that actually needs suspending, and
  resuming is just returning a URL — so sign-in needs no separate wake-up.
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
  URI in argv. The action buttons take the other route, through a COM activator —
  see below.
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

### Action buttons

Some notifications complete their work from the banner, with no window and no
webview: **Approve / Reject** on an AI tool-approval request, **Reply** on a Mingo
message. Both are plain authenticated REST calls, so the whole decision runs in
Rust (`chat_api.rs`) against the tenant host, with a bearer from
`tokens::ensure_fresh`.

Which buttons a notification earns, what a pressed one does, and how the outcome
comes back have one implementation for both platforms (`notification_actions.rs`);
each backend owns only its own plumbing. The button set is derived from the click
payload — and only when the id the button needs actually survived the payload
projection, because an Approve that can resolve nothing is worse than no Approve.
Only the buttons run in the background: a body click or "Open" is still the
existing click path, and so is any action this build does not recognize.

> **The session, not the notification, is the authority.** A banner can sit in
> Notification Center or the Action Center for days, across a sign-out or a
> different user signing in on the same machine. So every background action first
> resolves a live session (`chat_api::active_session`: tokens that refresh, an
> access token still inside its `exp`, a `userId` claim) and refuses unless that
> session is **the same user the notification was delivered to** — the
> subscription's user id is stamped into the notification at fire time for exactly
> this comparison (macOS `userInfo`, Windows button arguments), and an unstamped
> notification from an older build is refused.

There is no window to report into afterwards, so the outcome comes back as another
notification: a plain confirmation on success, and on failure a re-post of the
original — same buttons, same payload, the reason in the body — so the decision is
still there to retry with. A failed reply carries the typed text back with it,
since responding to a notification clears the inline field.

#### Superseded notifications

A notification is not always the last word on what it is about. The gateway
**republishes** one when its subject changes — for an approval request, once it has
been decided — under the same notification id, carrying:

| Field | Meaning |
|---|---|
| `eventType` | `CREATED` for the first push, `UPDATED` for one that supersedes it. Absent → `CREATED`. |
| `context.resolution` | Backend `ApprovalResolution`: `PENDING`, `APPROVED`, `REJECTED`, `CANCELLED`. Absent until decided. |
| `context.resolvedByName` | Who decided it. |

That is a *correction*, not a second notification, and the shell has to read it as
one — otherwise the republished copy arrives as a fresh banner still offering a
decision that has already been made, whose only possible outcome is the gateway
refusing it (`HTTP 409`). The frontend's drawer resolves this by mutating the
existing entry in place; the shell mirrors that decision by decision:

- **It does not alert again.** An `UPDATED` envelope is posted with
  `SuppressPopup`, so it lands in the Action Center without raising a banner, and it
  does not increment the unread badge. The user was already interrupted once for
  this notification; what changed is the outcome of a decision, not a new one.
- **It lands on the banner it supersedes.** Approval toasts are tagged on Windows
  with their `approvalRequestId` (`notification_actions::approval_request_id`), and a
  tagged toast *replaces* the one already carrying that tag. The follow-up the shell
  posts after its own press carries the same payload, and so the same tag, so it
  also lands on the toast it just resolved.
- **A settled request loses its buttons.** `note_resolution` records the verdict off
  the wire — from any envelope, whether or not it is displayed — and `live_kind_for`
  posts anything still carrying that request with no decision attached. Only the
  three terminal resolutions count: `PENDING` on the wire is still a decision to
  make, the same line the frontend's `resolutionToStatus` draws.
- **A press that beats the update is not an error.** Nothing recalls a banner the
  user is already looking at, so a press can still arrive against a settled request.
  It is caught one round-trip earlier than the gateway where the verdict is already
  known, and past that the `409` itself is read as *already resolved*: the outcome
  notification says so plainly instead of reporting a failure, and does not re-offer
  a closed decision.

The tag is deliberately approval-only. Messages are not republished, and replacing
a message toast would hide one the user has not read.

Two places where the shell is deliberately looser than the drawer: an `UPDATED`
envelope for a banner that is no longer in the Action Center still lands there
(silently, settled) rather than being dropped — the drawer refuses to resurrect a
dismissed card, but `ToastNotificationHistory`'s `GetHistory` has no `WithId`
variant, so an unpackaged app cannot ask what is still on screen. And the shell has
no equivalent of the drawer's auto-read: the Action Center entry is the user's to
dismiss.

#### macOS (`macos_un.rs`)

Three categories are registered at init (`setNotificationCategories` replaces the
whole set, so it is one call): approval, message, and the default one-button set
every other notification keeps.

**How many buttons the user sees is macOS's call, not ours.** A banner folds every
action behind the "Options" chevron regardless of count — two collapse just like
three (verified on 15.x), and no action or category option overrides it. Only the
Alerts notification style, which the user chooses in System Settings, renders
actions as buttons. What the app does control is what is in that menu, so the
approval category carries the decision and nothing else.

Approve additionally carries `AuthenticationRequired`, but that is only macOS's
device-lock gate: on an unlocked Mac it prompts for nothing, which is why it is not
the gate that matters. Reject is `Destructive` and one click — denying a tool
execution is the fail-safe direction, so it must not be harder than approving.

#### Windows (`windows_activator.rs`)

Protocol activation, which still carries the body click, cannot carry these: it
delivers no user input at all — so an inline reply is impossible through it — and
it cannot run in the process that is already running. Both need a COM
`INotificationActivationCallback`, so the buttons declare
`activationType="background"` and Windows resolves them to a CLSID registered
against the app's AUMID.

- **Registration** is HKCU-only and rewritten on every launch, like the URI scheme,
  so the CLSID always names the current exe:
  `Software\Classes\CLSID\{CLSID}\LocalServer32` gets `"<exe>" -ToastActivated`,
  and `Software\Classes\AppUserModelId\<AUMID>` gets a `CustomActivator` value. The
  installer already stamps the AUMID itself onto the Start Menu shortcut (tauri's
  NSIS `SetLnkAppUserModelId` writes the bundle identifier, which is what toasts
  are posted under) but has no property for the activator.
- That key is also the AUMID registration, so a **dev build** posts under its own
  identifier instead of borrowing PowerShell's — which could never have carried our
  CLSID, and so could never have had working buttons. Unlike macOS, the buttons can
  be exercised without an installed build.
- A toast can wear the app's **logo** in two places, and they are separate
  mechanisms — neither covers the other:
  - the **header icon**, beside the app name, from `IconUri` on the AUMID key. This
    is the one in use. A Start Menu shortcut carrying the AUMID is not enough, even
    when its icon is the app's own: verified against an MSI install whose shortcut
    points at a world-readable copy of `icon.ico` and whose toasts still came up
    blank until `IconUri` was written.
  - the **image inside the notification**, from `<image placement="appLogoOverride">`
    in the XML — the large logo beside the text. Deliberately **not** used: it is out
    while the notification layout is being designed.

  The file is embedded in the binary — so a dev build wears the same logo as a
  shipped one — and laid down in the app-config directory on first use, because the
  notification platform resolves it out of process and a toast left in the Action
  Center still resolves it after the app has exited or been uninstalled.

  > **The identity is cached per AUMID, and the cache outlives the registration.**
  > Windows resolves name and icon the first time an AUMID posts, and the WPN user
  > service holds that answer: a build that posted before `IconUri` existed leaves
  > the AUMID stuck on the generic placeholder no matter what is written afterwards.
  > Fresh installs are unaffected — `register` runs at startup, before the first
  > toast — but a machine that ran an older build needs the cache dropped once
  > (`Restart-Service WpnUserService_*`, a sign-out, or a reboot). Verified both
  > ways on Windows 11 26200: a never-used AUMID with `IconUri` set renders the logo
  > immediately, and the app's own AUMID kept the placeholder until that service was
  > restarted. The shell does not restart system services to paper over this.
- The class object is registered from an **MTA** thread, so activations arrive on an
  RPC thread and no message pump is involved — the only thread with a pump is
  Tauri's event loop, which must not block on a REST call. It is registered from the
  instance that survived the single-instance check, so COM never routes an
  activation into a process that is about to exit.
- Windows has no per-notification sidecar like macOS's `userInfo`, so the payload,
  the title and the delivered-to user id ride in each button's `arguments` string.
  The click payload is already projected down to ids, which is what keeps a toast
  inside the platform's ~5 KB cap when the arguments are repeated per button.
- A press that arrives while the app is **quit** starts it through `LocalServer32`;
  that launch opens no window, so a background decision completes invisibly and
  leaves the app resident in the tray. A press that needs the app opens the window
  itself, since setup no longer will.
- Reject is styled `Critical` (red), the closest Windows has to Destructive. Windows
  renders these inline rather than behind a chevron, so the approval decision is
  actually visible on the banner — better than the macOS result above.
- **`hint-buttonStyle` has exactly two values**, `Success` and `Critical`; there is
  no arbitrary button color, and `useButtonStyle="true"` on the root is what enables
  either. An unrecognized value — a hex color, a plausible-sounding `Warning` — is
  not rejected, it is silently ignored and the button renders plain, so a colour
  that "did not take" looks identical to one never set. Verified on Windows 11 26200.
- The title and the body are clamped before the XML is built, because a document
  over the platform's ~5 KB cap is not rejected loudly — the toast simply never
  appears, and the title is repeated inside every button's arguments. One
  consequence: a failed reply's echoed text is bounded here where it is not on
  macOS, so a very long reply comes back truncated rather than not at all.

## Known gaps

- No feature-flag gate on notifications. A webview-independent plane cannot wait
  on webview state; silence at the subject level is the effective gate.
- A user switch **on the same tenant** re-subscribes over a connection the gateway
  authorized with the previous user's bearer. Whether that is rejected depends on
  the gateway's subject-permission model, which is not verified here.
- Moving the interactive streams behind a shared transport abstraction, so the
  shell owns a single connection, remains possible but unjustified — nothing
  consumes those streams while hidden.
- On **macOS**, an action taken while the app is quit relaunches it, and launch
  opens the main window — so that path completes the action but does not stay
  invisible. Suppressing the window would mean knowing at setup time that a
  response is coming, which the delegate only reports afterwards. Windows does not
  have this gap: its launch carries `-ToastActivated`, which says up front that the
  process exists only to serve a press.
- Toast activation does not reach a process running **elevated**, so action buttons
  do nothing for a shell started as administrator.
- A superseding notification is folded into the banner it supersedes on **Windows
  only**. Each UN request is posted under a fresh UUID, so on macOS the update
  arrives as a second banner, and it alerts. The platform-neutral halves — no
  buttons on a settled request, a `409` read as already resolved — apply there as
  they do here, so the second banner is inert rather than misleading. Giving
  `macos_un::post` the same identity (`UNNotificationRequest` replaces a request
  whose identifier it already holds) and honouring `Delivery::Update` would close
  it; it is left alone because it cannot be built or exercised from the Windows
  toolchain this was fixed on.
