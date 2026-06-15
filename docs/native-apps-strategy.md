# OpenFrame Native Apps — Strategy, Decisions & Audit

> Working document. Captures the architecture decisions, the shipped desktop app,
> the planned mobile app, Apple distribution analysis, and the full audit of
> `openframe-frontend` for static export. Last updated: 2026-06-16.

Confidence tags (**high / moderate / low**) are kept on claims that depend on
Apple policy or Next.js version behavior, which change over time — re-verify
before acting on the moderate/low ones.

---

## 1. Goal & constraints

- Ship **native desktop + mobile apps** that deliver the existing
  `openframe-frontend` UI **without forking the frontend into a separate codebase**.
- Mobile hard requirement: **push notifications via APNS (iOS) + FCM (Android)**
  ("GNS" in the original ask = FCM/Firebase).
- Reuse the shared design system (`@flamingo-stack/openframe-frontend-core`) and
  the existing backend (GraphQL gateway, REST, NATS, MeshCentral).

### The single most important fact
`openframe-frontend` is a **Next.js SSR app** (`output: 'standalone'`,
`export const dynamic = 'force-dynamic'`, runtime env via `next-runtime-env`,
build-time `rewrites()`). It is **not** static. Every backend URL resolves from
`runtimeEnv.tenantHostUrl() || window.location.origin`. This drives every
decision below.

The product is **useless offline** (GraphQL/auth/NATS/MeshCentral all live on the
tenant gateway), so "bundle for offline" buys nothing — the only reasons to bundle
are **App Store approval** and **cold-start/perf**, not offline.

---

## 2. Architecture overview — the three possible shapes

A webview wrapper can only load **(a) bundled static files** or **(b) a URL**.
Given an SSR frontend, that yields three shapes:

| Shape | Frontend change | App Store posture | Notes |
|---|---|---|---|
| **A. Remote-URL shell** | None | Worst (4.2 risk) | Webview loads deployed tenant. Cookies/WS work same-origin. |
| **B. Bundled static export** | De-SSR workstream | Best (real native app) | `output: 'export'` SPA in-bundle; data over API. Requires token auth + CORS. |
| **C. Bundled SSR server (Node on device)** | n/a | n/a | **Rejected** — cannot run Node server on iOS. |

- **Desktop** shipped as **A** (remote shell) — see §3.
- **Mobile** should start as **A** for speed, then move to **B** for the App Store — see §4–§7.

---

## 3. Desktop app — SHIPPED ✅ (`openframe-desktop`)

Separate git repo at `~/flamingo/openframe-desktop`. Tauri 2 shell that loads the
deployed tenant URL in a webview. **Zero frontend changes.** Verified working
end-to-end (connect → tenant load → auth → dashboard) via a real `.app` build.

### How it works
- First launch → no saved host → **connect window** (local host-picker page,
  `connect/index.html`). On submit, `set_tenant_host` normalizes/validates the URL,
  persists it to `config.json` (app config dir), opens the **main** window at
  `https://<tenant>/` via `WebviewUrl::External`, closes the picker.
- Subsequent launches → saved host → straight to the tenant.
- Tray (Show / Switch Instance / Quit), close-to-tray, single-instance, macOS
  dock/activation-policy handling.

### Security posture
- The remote `main` window is in **no capability** → the loaded tenant site
  **cannot reach any Tauri command**. Only the trusted local `connect` window
  (`capabilities/connect.json`, `windows: ["connect"]`) has IPC.

### New-window handling (`window.open` / `target="_blank"`)
A webview drops new-window requests by default, so these links did nothing.
Implemented `on_new_window` so they work:
- **Same-origin as a currently-open window** → new **in-app** window (shared
  session), cascaded position, hidden-until-painted.
- **External http(s)** → system browser.
- **Other schemes** → blocked.
- Origin is compared against the **live** `WebviewWindow::url()` origin (NOT the
  config host) so links stay in-app even when login redirects across hosts
  (config host → shared auth host → the tenant you land on).
- The handler is attached to **every** window (main + children) so links chain to
  any depth.

### White-flash fix
Windows are created **hidden** with a dark `background_color`, shown on
`PageLoadEvent::Finished` (2.5s fallback timer). `background_color` alone is
insufficient on macOS (Tauri docs: not applied to the webview layer there), hence
hide-until-painted. **high**

### Key files
- `src-tauri/src/lib.rs` — windows, tray, `handle_new_window`, config, show-on-load.
- `connect/index.html` — host picker (uses `window.__TAURI_INTERNALS__.invoke`
  with `__TAURI__.core.invoke` fallback).
- `src-tauri/tauri.conf.json` — `withGlobalTauri`, dynamic windows, `com.openframe.desktop`.
- `src-tauri/capabilities/connect.json` — IPC scoped to connect window only.

### Commit history (this repo)
- `78b5606` scaffold
- `ec7fcc6` connect IPC resolution + real errors
- `8f24936` new-window handler (open `_blank` in windows)
- `c55ce0c` classify by live origin, not config host
- `d8a9cc3` cascade windows (fix "looks like 2 max")
- `3783c23` attach handler to child windows (links from child windows)
- `5dd3f4d` eliminate white flash (hide-until-painted)

### Run it
```bash
cd ~/flamingo/openframe-desktop
npm install
npm run dev        # tauri dev; enter an instance e.g. test-dev.openframe.build
```
> Note: running the bare `target/debug` binary isn't registered with macOS;
> `npm run dev` (or a built `.app`) is the supported path.

### Outstanding (desktop, nice-to-have)
- Real app icons (`npm run tauri icon ~/flamingo/openframe-app-icon.png`).
- Deep links (`openframe://`), native notifications bridged to NATS, auto-updater.
- CI signing/notarization (macOS) + code-signing (Windows).

---

## 4. Mobile app — PLANNED

### Wrapper choice: **Capacitor** (recommended)
- **First-party push** (`@capacitor/push-notifications`) covering **APNS + FCM** —
  turnkey, mature. This is the decisive factor. **high**
- Reuses the web app (thin native shell), rich plugin ecosystem (Face ID,
  camera, secure storage, deep links, status bar) → supplies the native features
  that move it out of "pure web wrapper" territory.
- Can load remote (`server.url`) like the desktop shell, or bundle static assets.

### Alternatives considered
- **Tauri 2 Mobile** — appealing (one codebase with desktop) but **no first-party
  push**; APNS/FCM would be DIY native plugins. Too risky given push is the hard
  requirement. **moderate**
- **React Native + WebView** — strong push (Firebase) but a whole new stack for no
  benefit here. Rejected.
- **PWA / TWA** — iOS web push is weak and not App-Store-distributed. Rejected.

### Push architecture (new backend work — unavoidable)
- NATS already powers **in-app/foreground** live updates; it does **not** deliver
  when the app is backgrounded/killed. APNS/FCM fill that gap.
- Backend must fan notification events out to **both** NATS (in-app) and APNS/FCM
  (push). New pieces:
  1. **Device-token endpoint** — store APNS/FCM token per user/device.
  2. **Send service** — call APNS + FCM on notification events.
- Token registration + deep-link-on-tap can be **shell-side** (native layer POSTs
  token with the webview's credentials; navigates webview to `https://tenant/<route>`
  on tap) → **frontend repo stays untouched** even in remote-load mode. **high**

### Native features to include (UX + App Store 4.2 ammunition)
Push, **Face ID/biometric gate**, camera (asset/QR scanning for MSP use), native
splash, native error/offline screen, deep links, secure storage.

---

## 5. Apple distribution

### The 4.2 problem
Apple Guideline **4.2 (Minimum Functionality)** targets "a website in a wrapper."
A **remote-loaded** webview is the highest-risk shape because Apple can't review
content that changes server-side. Android/Play Store does not care — **the entire
problem is iOS**. **high**

Distinction Apple actually cares about:
- **Remote UI/code** (screens download at runtime, changeable post-review) → bad.
- **Remote data** (bundled UI calls your API for JSON) → totally fine, normal.

### Apple Developer Enterprise Program (ADEP) — ❌ WRONG TOOL
- For distributing to **your own employees only**. $299/yr, tightened eligibility
  (effectively 100+ employees + justification). **moderate-high**
- Distributing to customers/other orgs **violates terms** → Apple **revokes the
  cert**, and **every installed copy stops launching**. Existential risk for a
  product. **high**
- Only fits if the app were purely Flamingo-internal tooling. Not our case.

### Apple Business Manager — Custom Apps / Unlisted — ✅ RIGHT TOOL
- Distribute through **normal App Store infra** but **private/unlisted**, on the
  **standard $99 program** (no Enterprise needed). **high**
- **Custom Apps via ABM**: assign to specific customer orgs by ABM/D-U-N-S; they
  install via MDM. Tightly controlled; requires customer to be set up in ABM.
- **Unlisted App Distribution**: normal app, not searchable, reachable via a direct
  App Store link. **Less onboarding friction** for many small MSPs (auth-gated app,
  so a shareable link is fine).
- **Still goes through App Review.** Some guidelines are relaxed for custom apps and
  reviewers are more lenient on 4.2 in practice, but **4.2 is NOT formally waived**.
  **moderate**
- No cert-revocation cliff. MDM delivery aligns with MSP customers' workflows.

### Recommendation
**ABM Unlisted (primary) + Custom Apps for MDM-heavy customers. Never Enterprise.**
Make the shell genuinely native (auth/Face ID/push/camera/offline) so the webview
is the *content layer*, not the *whole app*.

---

## 6. Bundling vs remote-load (the key lever)

- **Remote-load (A):** zero frontend changes, fastest, but worst App Store posture
  → pushes you to ABM Custom/Unlisted.
- **Bundled static export (B):** real de-SSR workstream, but UI ships in-bundle →
  4.2 largely dissolves, **public App Store becomes viable**, faster cold start,
  resilient to flaky networks. Loses SSR (irrelevant behind auth).

Bundling = the better end state for iOS, and it **also** lets the *desktop* app
embed the UI instead of pointing at a remote URL → pays off twice.

The team already has a bundled-SPA precedent: **`openframe-chat`** is a Vite SPA
reusing the core lib, bundled into Tauri with token auth. But building a *second*
SPA that reimplements `openframe-frontend`'s routes = the "separate codebase" we're
avoiding. So the one-codebase path is to make **`openframe-frontend` itself
export-capable** (§7), not fork it.

---

## 7. Static-export audit of `openframe-frontend`

Target: build under **`output: 'export'`** behind an env flag (web deploy keeps
`standalone`; mobile/desktop bundle uses `export`). Audit run 2026-06-16.

### Verdict
**Feasible, lower-risk than expected.** No hard server-side blockers exist, and all
13 dynamic routes are already `'use client'` components reading params via
`useParams()` — the app is an SPA wearing an SSR shell. The one genuine design
decision is dynamic-route resolution under export (item 8).

### Already clean (zero work)
- No route handlers / API routes (`app/**/route.ts`).
- No middleware, no Server Actions (`'use server'`).
- No `next/headers` (`cookies()`/`headers()`/`draftMode()`).
- No dynamic `generateMetadata` (metadata is static).
- `images: { unoptimized: true }` already set.
- `dashboard/layout.tsx`, `auth/layout.tsx` are server components but touch no
  request data → render fine at build.

### Change-list

- [ ] **1. Config — trivial** (`next.config.mjs`)
  - `output: 'standalone'` → `'export'`; drop `outputFileTracingRoot` and
    `rewrites()`. `trailingSlash` fine; `skipTrailingSlashRedirect` becomes no-op.
    Gate by env so web build stays `standalone`.

- [ ] **2. Remove `export const dynamic = 'force-dynamic'` — mechanical, ~40 files**
  - Invalid under export. Safe to remove **because** the root layout already wraps
    children in `<Suspense>` (what `useSearchParams`, 27 files, needs). Verify each
    `useSearchParams` consumer is under a Suspense boundary. **high**

- [ ] **3. Two async server pages → client — small, 2 files**
  - `src/app/(app)/log-details/page.tsx` and `src/app/(app)/tickets/dialog/page.tsx`
    are `async` server components that `await searchParams` (request-time SSR).
    Convert to `'use client'` + `useSearchParams()`.

- [ ] **4. Runtime env injection — small** (`layout.tsx`, `src/lib/runtime-config.ts`)
  - `<PublicEnvScript />` needs a server. Replace with config injected by the
    Capacitor/Tauri shell before the bundle loads (`window.__ENV = {...}`).
    `runtime-config.ts` already falls back to `window.process.env`/`process.env`.

- [ ] **5. URL resolution must use tenant host, not origin — moderate, cross-cutting**
  - In a bundle, origin = `localhost`, so `window.location.origin` hits the device.
    - `src/lib/graphql-client.ts:13` — **hardcoded** `${window.location.origin}/api/graphql`,
      no tenant-host fallback → **must fix**.
    - `src/lib/nats/nats-app-config.tsx` — reads `tenantHostUrl()` but appears to
      prefer origin in-browser → **verify precedence**.
    - `src/lib/relay/environment.ts:29`, `src/lib/api-client.ts` — already
      `tenantHost || origin`; fine **once `NEXT_PUBLIC_TENANT_HOST_URL` is set**.
  - Make every API/WS/GraphQL/NATS/MeshCentral URL honor the shell-provided host.

- [ ] **6. Auth → token mode + gateway CORS — moderate (mostly already built)**
  - Cookies fail cross-origin from `localhost`. Use the **existing** Bearer/dev-ticket
    path (api-client + relay already support it; `of_access_token` in storage +
    `NEXT_PUBLIC_ENABLE_DEV_TICKET_OBSERVER`). New work: **gateway CORS** for the
    `capacitor://localhost` / `https://localhost` origin + shell injecting the token
    (Face-ID-gated secure storage).

- [ ] **7. `/content/*` embedded-chat proxy — moderate, localized**
  - `src/app/components/openframe-chat-runtime-provider.tsx` relies on the
    `rewrites()` same-origin proxy that export removes. Switch to absolute gateway
    URLs + Bearer + CORS (provider already has cross-origin/Bearer logic).

- [ ] **8. Dynamic routes under export — the one real decision, 13 routes**
  - All 13 are client components already (12 use `useParams()`, 1 uses the `params`
    prop). Static export needs `generateStaticParams` for `[id]`/`[deviceId]`, but
    IDs are runtime-arbitrary (can't enumerate). Options:
    - **(a)** placeholder `generateStaticParams` + rely on client nav — works for
      in-app nav, **but cold deep-links** (push tap → `/devices/details/123`) 404
      against static files. **moderate**
    - **(b)** query-string (`/devices/details?id=123`) or a catch-all client shell
      (`[[...slug]]`) reading the param client-side → **survives cold deep-links**.
      **Recommended** since push deep-linking is a requirement.
  - Also switch `devices/details/[deviceId]` from `params` prop → `useParams()` (1 file).
  - Dynamic route dirs:
    `customers/details/[id]`, `customers/edit/[id]`, `devices/details/[deviceId]`,
    `knowledge-base/{details,edit,folders}/[id]`, `monitoring/{policy,query}/[id]`,
    `monitoring/{policy,query}/edit/[id]`, `scripts/{details,edit,schedules}/[id]`.

### Effort
Mechanical items (1–4): low. Real cost is **item 8** (deep-link routing) and
**items 5–7** (localhost-origin / CORS / auth / chat proxy), plus end-to-end
re-testing every data flow from a `localhost` origin. Rough order: **~1–2 focused
weeks** on the frontend, plus separate backend work for CORS + the push pipeline.
**moderate** (dominated by item 8 and how clean the token-auth path proves).

---

## 8. Roadmap / sequencing

1. **Mobile MVP (remote-load, fast):** Capacitor shell pointing at the deployed
   tenant + push (APNS/FCM) + Face ID + camera. Validates push, native features,
   and the **ABM Unlisted** distribution mechanics. Backend: token endpoint + send
   service.
2. **Frontend export-compatibility (parallel):** §7 items 1–4 → get a static build
   *building*; then decide item 8 (recommend query-param/catch-all) and convert the
   13 routes; then items 5–7 (host resolution, token auth, CORS, `/content`).
3. **Flip to bundled:** switch the same Capacitor shell from `server.url` (remote)
   to bundled assets. Same shell, same backend, far stronger App Store posture
   (public store becomes viable). Apply the same bundling to the desktop app.
4. **Hardening:** deep links, offline/error screens, signing/notarization, CI.

---

## 9. Open decisions (need owner input)

- **Dynamic-route strategy** (§7.8): query-param / catch-all (deep-link safe,
  recommended) vs placeholder + client-nav (less work, deep-link-fragile).
- **Distribution**: ABM Unlisted vs Custom-Apps-via-MDM (or both) per customer tier.
- **Mobile MVP shape**: ship remote-load first, or wait for bundled? (Recommend:
  ship remote-load to de-risk push/distribution while export work proceeds.)
- **Public App Store** as a goal? (Only realistic with the bundled build.)
- **Tenant host UX on mobile**: same connect/host-picker pattern as desktop, or
  per-customer pre-baked builds?

---

## 10. References

- Desktop repo: `~/flamingo/openframe-desktop` (this repo).
- Frontend: `~/flamingo/openframe-oss-tenant/openframe/services/openframe-frontend`.
- Bundled-SPA precedent: `~/flamingo/openframe-oss-tenant/clients/openframe-chat`
  (Vite + Tauri + core lib + token auth).
- Core design system: `@flamingo-stack/openframe-frontend-core`.
- Key frontend auth/URL touchpoints: `src/lib/api-client.ts`,
  `src/lib/relay/environment.ts`, `src/lib/graphql-client.ts`,
  `src/lib/runtime-config.ts`, `src/lib/nats/nats-app-config.tsx`,
  `src/app/(auth)/auth/components/dev-ticket-observer.tsx`.
