# OpenFrame Mobile App — Launch Plan

> How to ship native iOS + Android apps that run the **existing**
> `openframe-frontend` UI, with native features (push, biometrics, camera).
> Companion to [native-apps-strategy.md](./native-apps-strategy.md) (the *why* +
> Apple distribution analysis) and the frontend's `docs/static-export-migration.md`
> (the static-export work, **done** — `npm run build:export` → static `dist/`).
> Last updated: 2026-06-29.

---

## 1. Principle: one frontend, three targets

The organizing rule is **no second UI codebase**. `openframe-frontend` already
builds two ways from one source, gated by `OPENFRAME_BUILD_TARGET`:

| Target | Build | Output | Shell |
|---|---|---|---|
| **Web** | `npm run build` | SSR `standalone` | — (deployed server) |
| **Mobile** | `npm run build:export` | static SPA → `dist/` | Capacitor (iOS/Android) |
| **Desktop** | `npm run build:export` | static SPA → `dist/` | Tauri (`openframe-desktop`) |

The mobile app is a **thin native shell** that loads the static `dist/` bundle and
adds only what the web can't do (push, biometrics, camera, secure storage). All
screens, routing, state, and data logic stay in `openframe-frontend`. This is the
single biggest reuse lever — see §5.

---

## 2. Frameworks & technologies

### Native runtime: **Capacitor 8** (decisive choice)
A WebView-based native container that loads our existing web bundle verbatim and
bridges to native APIs through TypeScript plugins.

- **First-party push covering APNS *and* FCM** (`@capacitor/push-notifications`) —
  the deciding factor; push is the hard requirement. **high**
- Loads the **unmodified** export bundle (`webDir = dist/`) — no UI rewrite.
- Mature plugin ecosystem for the other native features (below).
- Can run **remote-load** (`server.url`) *or* **bundled** (`webDir`) — lets us ship
  fast then harden (§4, §6).
- TypeScript-first: native Swift/Kotlin only for custom plugin glue, if ever.

**Rejected alternatives** (see strategy doc §4): Tauri 2 Mobile (no first-party
push), React Native + WebView (whole new stack, no benefit), PWA/TWA (weak iOS web
push, not App-Store distributable).

### Stack summary
| Layer | Technology |
|---|---|
| Native container | **Capacitor 8** (`@capacitor/core`, `/cli`, `/ios`, `/android`) — iOS uses Swift Package Manager (no CocoaPods) |
| Web UI | existing `openframe-frontend` static export (Next 16 `output: export`) |
| Design system | `@flamingo-stack/openframe-frontend-core` (already shared) |
| Push | `@capacitor/push-notifications` (APNS + FCM) + Firebase project (Android) |
| Biometrics | `@aparajita/capacitor-biometric-auth` (or `capacitor-native-biometric`) |
| Camera / QR | `@capacitor/camera` + `@capacitor-mlkit/barcode-scanning` |
| Secure storage | Keychain (iOS) / Keystore (Android) plugin for `of_access_token` |
| Deep links | `@capacitor/app` (`appUrlOpen`) + Universal Links / App Links |
| Native chrome | `@capacitor/splash-screen`, `@capacitor/status-bar` |
| Tooling | Xcode + Android Studio/Gradle, **Fastlane** (signing, TestFlight, Play track) |
| Languages | TypeScript everywhere; minimal Swift/Kotlin only for custom glue |

### What the native layer actually is (Swift/Kotlin?)

**Yes, there are real Swift and Kotlin/Java projects — but you write almost no Swift
or Kotlin.** Capacitor generates a thin native **host app** whose only job is to host a
`WKWebView` (iOS) / `WebView` (Android) that loads the `www/` bundle. You own, build,
and sign those projects, but their app code is generated boilerplate:

- **iOS** (`ios/App/`, a normal Xcode project): generated `AppDelegate.swift` + a root
  `CAPBridgeViewController` (the WebView host). Usually untouched.
  ```swift
  @main class AppDelegate: UIResponder, UIApplicationDelegate { /* generated */ }
  ```
- **Android** (`android/app/`, a normal Gradle project): `MainActivity` is the whole file.
  ```kotlin
  class MainActivity : BridgeActivity()
  ```
- **Plugins ship their own native code.** You `npm install` a plugin and `npx cap sync`
  wires its Swift/Kotlin into the projects (Swift Package Manager / Gradle). You call it from
  **TypeScript** and never touch its native source.

**Where you actually touch native — config, not app logic:**
- **iOS:** capabilities (Push, Background Modes), APNS entitlement, `Info.plist` usage
  strings (`NSCameraUsageDescription`, `NSFaceIDUsageDescription`), `GoogleService-Info.plist`
  (FCM-on-iOS), Associated Domains (Universal Links), signing / provisioning.
- **Android:** `google-services.json` (FCM), manifest permissions, App Links intent
  filters + `assetlinks.json`, signing config. (Plugin Gradle deps auto-wire on `cap sync`.)
- **Custom Swift/Kotlin** only if a feature has no plugin — a small plugin class per
  platform exposed to TS. For our set (push / biometrics / camera / secure-storage),
  plugins exist, so this is ~zero.

| | What | Language |
|---|---|---|
| ~99% of the app | shared `openframe-frontend` bundle + thin shell glue | **TypeScript** |
| Native host app | generated `AppDelegate` / `MainActivity` | Swift / Kotlin (generated, untouched) |
| Native features | official + community plugins | their Swift/Kotlin (you don't write it) |
| You maintain | capabilities, entitlements, `Info.plist`/manifest, Firebase config, signing, links | XML / plist / Gradle config |

Mental model: a **WebView host app**, not a SwiftUI/Compose app. The "native layer" is
config + plugin glue; the few real Swift/Kotlin files stay generated.

---

## 3. Repository & project layout

A **new `openframe-mobile` repo** (sibling of `openframe-desktop`), holding *only*
the Capacitor shell — never a copy of the frontend.

```
openframe-mobile/
├─ capacitor.config.ts        # appId, appName, webDir → the copied bundle
├─ package.json               # Capacitor deps + the build/copy scripts
├─ scripts/
│  ├─ build-web.sh            # build openframe-frontend export + copy dist → www/
│  └─ inject-env.mjs          # prepend window.__ENV bootstrap to www/index.html
├─ www/                       # the copied static bundle (gitignored build artifact)
├─ ios/                       # `npx cap add ios`  — Xcode project
├─ android/                   # `npx cap add android` — Gradle project
└─ src/native/                # only native-feature glue (push reg, biometrics, …)
```

**How the shell gets the frontend bundle** (pick one; recommended: submodule + CI):
- **git submodule** of the frontend repo (once it's split out per the strategy doc)
  → `build-web.sh` runs `npm run build:export` in the submodule and copies `dist/`.
- **CI artifact**: the frontend pipeline publishes the export `dist/` as an artifact;
  the mobile pipeline pulls the pinned version. Cleanest once both repos have CI.
- **npm package**: publish the export output as a versioned tarball the shell depends
  on. Heaviest; only if other consumers need it.

All three keep **zero frontend source** in the mobile repo — the bundle is an input,
not a fork.

---

## 4. Bundling the static frontend into the native app

The frontend already produces the bundle; the shell's job is to wrap it.

### Build pipeline
```bash
# 1. Build the static export (in the frontend repo / submodule)
OPENFRAME_BUILD_TARGET=export npm run build        # → dist/  (npm run build:export)

# 2. Copy into the Capacitor web dir + inject runtime env
cp -R <frontend>/dist  openframe-mobile/www
node scripts/inject-env.mjs                         # prepend window.__ENV bootstrap

# 3. Sync into the native projects and build
npx cap copy && npx cap sync
npx cap open ios        # Xcode → .ipa
npx cap open android    # Android Studio → .aab
```

`capacitor.config.ts`:
```ts
import type { CapacitorConfig } from '@capacitor/cli';
const config: CapacitorConfig = {
  appId: 'cx.flamingo.openframe',
  appName: 'OpenFrame',
  webDir: 'www',                       // the copied export bundle
  // server.url is set ONLY in the Phase-1 remote-load build (see §6)
};
export default config;
```

### Three mechanics the export build depends on

1. **Runtime env injection (no server to do it).** The bundle reads config from
   `window.__ENV` (next-runtime-env's global; `runtime-config.ts` also falls back to
   `window.process.env`). The shell must set it **before** the bundle's JS runs —
   prepend a `<script>` to `www/index.html` at copy time (`inject-env.mjs`):
   ```html
   <script>window.__ENV = { NEXT_PUBLIC_TENANT_HOST_URL: "https://<tenant>", /* … */ };</script>
   ```
   For a multi-tenant app the tenant host is **discovered at login** from a baked
   shared auth host and persisted by the frontend — see native-apps-strategy.md
   §"Hosts: shared only, tenant discovered". (The runtime host-picker this
   originally proposed was built for desktop, then deleted on 2026-07-28.)

2. **SPA fallback for deep links.** Capacitor serves files from `webDir`. The
   **dashboard query-param routes already resolve** — `/devices/details?id=123` hits
   the static `/devices/details/index.html` and the client reads `?id=`. The only
   gap is the help-center CMS routes (prerendered as placeholder shells); configure
   the WebView to serve `index.html` for any unmatched path so the Next client router
   takes over. (Native: a `WKURLSchemeHandler` / `shouldOverrideUrlLoading` fallback,
   or Capacitor's `server` config.)

3. **Auth + CORS from a `localhost` origin.** In a bundle `origin =
   capacitor://localhost`, so cookies don't survive cross-origin. Use the existing
   **Bearer/dev-ticket** path (`of_access_token` + `NEXT_PUBLIC_ENABLE_DEV_TICKET_OBSERVER`);
   the shell stores the token in Keychain/Keystore (biometric-gated) and injects it.
   **Backend work:** allow `capacitor://localhost` / `https://localhost` via CORS.
   (Tracked as items A–C in the frontend's static-export-migration doc.)

---

## 5. Maximizing code reuse

Reuse is the point of this architecture. In priority order:

1. **One frontend codebase, env-gated** — the same `openframe-frontend` serves web
   (`standalone`) and mobile/desktop (`export`). No fork, no parallel SPA. A feature
   shipped once appears on all three targets. *(This is already done and verified.)*
2. **Same export bundle for mobile *and* desktop** — `openframe-desktop` (Tauri)
   **now bundles the same `dist/`** (done 2026-07-08), with the same
   `window.__ENV` injection done at runtime via a Tauri initialization script
   instead of a copy-time `<script>`, plus a Capacitor-compatible
   `window.Capacitor.Plugins.NativeAuth` shim so the frontend's native-shell
   path works unchanged. The bundling work paid off twice.
3. **Shared design system** — `@flamingo-stack/openframe-frontend-core` is already the
   UI/component SSOT across frontend and `openframe-chat`. Mobile inherits it for free.
4. **Same backend, same contracts** — mobile calls the identical GraphQL/REST/NATS/
   MeshCentral endpoints; only the auth transport (Bearer vs cookie) and CORS differ.
5. **Thin, shell-only native code** — keep the native layer to feature glue (push
   registration, biometric unlock, camera launch, secure storage). Token registration
   and deep-link-on-tap can be **shell-side** (native posts the token with the
   WebView's credentials; on tap, navigates the WebView to the in-app route), so the
   frontend repo stays untouched even before bundling.
6. **Prefer community Capacitor plugins over custom native** — every line of Swift/
   Kotlin is per-platform maintenance. Reach for a maintained plugin first.
7. **`openframe-chat` precedent** — the team already bundles a core-lib SPA into a
   Tauri shell with token auth; mirror its env/token patterns rather than inventing new.

Net: the mobile app is ~one small repo of Capacitor config + native glue. Everything
a user sees and does is the shared frontend.

---

## 6. Native features

| Feature | Plugin | Maintainer | Integration point |
|---|---|---|---|
| Push (APNS + FCM) | `@capacitor/push-notifications` | **Official** | Register on login → POST token to backend; tap → navigate WebView to `/x?id=…` |
| Camera | `@capacitor/camera` | **Official** | Photo capture / picker for MSP workflows |
| Deep links / app state | `@capacitor/app` | **Official** | `appUrlOpen` → in-app route (query-param routes resolve cold) |
| Splash / status bar | `@capacitor/splash-screen`, `@capacitor/status-bar` | **Official** | Native launch chrome |
| Key-value prefs | `@capacitor/preferences` | **Official** | Non-secret prefs only — NOT encrypted, never for tokens |
| QR / barcode | `@capacitor-mlkit/barcode-scanning` | Community (Capawesome) | QR / asset scanning |
| Biometric gate | `@aparajita/capacitor-biometric-auth` | Community | Unlock app + release `of_access_token` |
| Secure storage | e.g. `@aparajita/capacitor-secure-storage` (Keychain/Keystore) | Community | Holds `of_access_token`, biometric-gated |

These also serve the App Store **4.2** posture — they make the shell a real native
app, not "a website in a wrapper" (strategy doc §5).

### Official vs community plugins
Capacitor ships a set of **official plugins** (the `@capacitor/*` scope, maintained by
the Capacitor/Ionic team): **push-notifications, camera, app, preferences, geolocation,
filesystem, device, network, splash-screen, status-bar, share, haptics, clipboard,
browser, local-notifications**, and more. The two headline native features — **push and
camera — are official.** The features with **no official plugin** are **biometrics**,
**secure (encrypted) storage**, and **barcode scanning**, all covered by mature
community plugins (Capawesome / aparajita). Note: `@capacitor/preferences` is official
but **not encrypted**, so the token needs a secure-storage community plugin. Treat each
community plugin as a maintenance / supply-chain dependency — pin the version, confirm
it's actively maintained, and keep a thin TS wrapper so swapping it is a one-file change.

### Push pipeline (backend — new work, unavoidable)
NATS drives in-app/foreground live updates but does **not** deliver when the app is
backgrounded/killed; APNS/FCM fill that gap.
1. **Device-token endpoint** — store APNS/FCM token per user/device.
2. **Send service** — on notification events, fan out to **both** NATS (foreground)
   and APNS/FCM (push).

---

## 7. Distribution (summary — full analysis in strategy doc §5)

- **ABM Unlisted Distribution** (primary): normal App Store infra, private/unlisted,
  standard $99 program, shareable link — low onboarding friction for MSPs.
- **Custom Apps via ABM** for MDM-heavy customers (assigned by org, MDM-delivered).
- **Never Apple Enterprise (ADEP)** — cert-revocation cliff kills installed copies.
- Android/Play Store is unaffected; the entire 4.2 concern is iOS, and the **bundled**
  build is what makes it defensible (and a public listing viable later).

---

## 8. Sequencing / milestones

1. **Shell MVP — remote-load (fast, de-risks push + distribution).** New
   `openframe-mobile` repo; Capacitor shell with `server.url` = deployed tenant; add
   push (APNS/FCM), biometric gate, camera. Backend: device-token endpoint + send
   service. Ship to TestFlight / Play internal track. Validates the **ABM Unlisted**
   mechanics end-to-end. *(Frontend untouched — token reg + deep-links are shell-side.)*
2. **Backend runtime-enable the bundle** — CORS for the device origin + confirm the
   Bearer/dev-ticket path from a `localhost` origin (frontend items A–C).
3. **Flip to bundled.** Point the same shell at `webDir = dist/` (+ `window.__ENV`
   injection + SPA fallback). Strongest App Store posture; faster cold start. ~~Apply
   the same bundle to the desktop app.~~ **Desktop done 2026-07-08** (see
   `native-apps-strategy.md` §3).
4. **Harden** — Universal/App Links, offline/error screens, code signing + notarization,
   Fastlane CI (TestFlight + Play tracks), crash reporting.

---

## 9. Open decisions

- ~~**Tenant host UX on mobile**~~ — settled 2026-07-28: baked shared auth host,
  tenant discovered at login. Mobile keeps an optional build-time single-tenant
  pin (`NEXT_PUBLIC_TENANT_HOST_URL`); desktop supports no pin at all.
- **Bundle delivery to the mobile repo** — submodule vs CI artifact vs npm package (§3).
- **Public App Store** as an eventual goal (only realistic with the bundled build).
- **Firebase project ownership** for FCM (Android push) — new vs existing Flamingo project.
