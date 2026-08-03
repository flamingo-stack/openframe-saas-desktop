// NATS-over-WebSocket connection lifecycle for the shell's background
// notification plane. The gateway's WS endpoint takes the bearer on the
// connect URL (`?authorization=`), so the connection expires with the token:
// the flamingo async-nats fork's `auth_url_callback` re-asks us for a fresh URL
// whenever the server rejects the current one, which is where the shell-owned
// token refresh plugs in. Ported from openframe-chat's
// nats_bridge/connection.rs.
//
// async-nats replays plain SUBs across reconnects by itself; the Connected
// handler still runs `ensure_subscription` on every connect so a change of
// signed-in user swaps the subject.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_nats::{Client, Event};
use percent_encoding::utf8_percent_encode;
use tauri::{AppHandle, Manager};
use tokio::sync::RwLock;

use crate::{load_config, notifications, tenant_host, tokens, UNRESERVED};

/// NATS client name, as it shows up server-side.
const CLIENT_NAME: &str = "openframe-desktop";
const WS_PATH: &str = "/ws/nats-api";
/// NATS-protocol-level credentials — the real auth is the URL bearer, which
/// the gateway validates at upgrade time.
const NATS_USER: &str = "machine";
const NATS_PASS: &str = "";

const FAST_RETRIES: usize = 3;
const FAST_DELAY_MS: u64 = 200;
const BASE_DELAY_MS: u64 = 1_000;
const MAX_DELAY_MS: u64 = 30_000;
const PING_INTERVAL: Duration = Duration::from_secs(10);
/// How often a parked reconnect loop looks for credentials. Matches `run`'s own
/// credential wait, so signing in costs the same wherever the connector is.
const PARK_POLL: Duration = Duration::from_secs(5);

struct Connector {
    app: AppHandle,
    /// The live client, or None before the first dial and between a tenant
    /// switch's teardown and its re-dial.
    connection: RwLock<Option<Client>>,
    /// Consecutive `auth_url_callback` invocations without a Connected in
    /// between. The forked connector resets its own attempt counter after a
    /// successful callback, so on a persistently rejected token its backoff
    /// never engages — we back off here instead.
    auth_failures: AtomicU32,
    /// Set while `run` is between "no connection" and "connection stored", so
    /// overlapping tenant switches can't start two dial loops.
    dialing: AtomicBool,
    /// Serializes [`resubscribe`] against [`reconnect`]. Without it a
    /// resubscribe that read the client just before a teardown could install a
    /// router on the dead connection afterwards, and the replacement's
    /// Connected would then see a matching subject and skip subscribing —
    /// silence until restart.
    session: tokio::sync::Mutex<()>,
}

/// Tenant gateway to dial — the notification subject is per-user on the
/// tenant's NATS, not the shared auth host. None until login learns it.
async fn read_server_url(app: &AppHandle) -> Option<String> {
    tenant_host(&load_config(app))
}

/// Bearer for the `?authorization=` query param. None = not signed in yet.
///
/// Reads what is stored and asks for a rotation in the background rather than
/// awaiting one. Awaiting it put a token rotation inside this reconnect loop,
/// which is the worst place for one: a reconnect means the network just broke,
/// and a rotation whose response is lost costs the whole session (the gateway
/// retires the presented refresh token with no grace window). A rejected dial is
/// cheap by comparison — it backs off and retries, by which time the rotation
/// this kicked off has landed.
async fn read_token(app: &AppHandle) -> Option<String> {
    tokens::refresh_soon(app, "NATS needs a bearer");
    tokens::load_tokens(app).access_token
}

/// Both halves a dial needs, each None until it is known.
///
/// One spelling of the pair for all three places that wait on it, because both
/// halves are `Option<String>` and a swapped pair therefore type-checks in
/// silence — which is exactly what had happened in one of them. Not free to
/// call: reading the token also asks for a rotation when one is due.
async fn credentials(app: &AppHandle) -> (Option<String>, Option<String>) {
    (read_server_url(app).await, read_token(app).await)
}

/// Spawn the connect/reconnect lifecycle. Call once, after
/// [`notifications::init`] — the Connected handler subscribes through it.
pub(crate) fn spawn(app: AppHandle) {
    let connector = Arc::new(Connector {
        app: app.clone(),
        connection: RwLock::new(None),
        auth_failures: AtomicU32::new(0),
        dialing: AtomicBool::new(false),
        session: tokio::sync::Mutex::new(()),
    });
    app.manage(connector.clone());
    tauri::async_runtime::spawn(async move { run(connector).await });
}

/// Re-point the plane at whoever is signed in now, on the connection we already
/// have. The signed-in user can change (sign-out, then someone else signs in on
/// the same tenant) while the connection stays up, and `Connected` is the only
/// other thing that subscribes — without this the plane would wait for a
/// reconnect that may never come. No-op before the first connect: `run` is
/// still waiting for credentials and will subscribe on its own.
pub(crate) fn resubscribe(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(connector) = app.try_state::<Arc<Connector>>().map(|s| s.inner().clone()) else {
            return;
        };
        let _session = connector.session.lock().await;
        let client = current_client(&connector).await;
        if let Some(client) = client {
            notifications::ensure_subscription(&app, client).await;
        }
    });
}

/// The tenant changed, so the connection itself is wrong — it is dialled at the
/// previous tenant's gateway and authorized with the previous tenant's bearer.
/// Tear the whole thing down and dial the new one.
pub(crate) fn reconnect(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(connector) = app.try_state::<Arc<Connector>>().map(|s| s.inner().clone()) else {
            return;
        };
        log::info!("[nats] tenant changed — dropping the connection and re-dialling");
        {
            let _session = connector.session.lock().await;
            // Order matters: a live Subscriber holds a clone of the client's
            // command sender, so the connection only shuts down once the router
            // is gone. It also has to vacate the slot, or the new connection's
            // Connected would see a matching subject and skip subscribing.
            notifications::drop_subscription(&app, "tenant changed").await;
            *connector.connection.write().await = None;
            connector.auth_failures.store(0, Ordering::Relaxed);
        }
        // Released first: run() parks until the new tenant's credentials land,
        // and holding the session lock through that would stall resubscribe.
        run(connector).await;
    });
}

async fn current_client(connector: &Arc<Connector>) -> Option<Client> {
    connector.connection.read().await.clone()
}

/// Waits for credentials, dials with retry-on-initial-connect, then hands
/// reconnects to async-nats (whose `auth_url_callback` re-asks us). Returns
/// once a connection is stored; [`reconnect`] calls it again for the next
/// tenant. At most one call is ever in flight — a second returns immediately,
/// so two overlapping tenant switches can't leave two live connections with
/// only one of them stored.
async fn run(connector: Arc<Connector>) {
    if connector.dialing.swap(true, Ordering::AcqRel) {
        log::info!("[nats] dial already in progress — not starting another");
        return;
    }
    let app = connector.app.clone();

    loop {
        let (server_url, token) = loop {
            match credentials(&app).await {
                (Some(url), Some(token)) => break (url, token),
                (url, token) => {
                    log::info!(
                        "[nats] waiting for credentials before initial connect (server_url={}, token={})",
                        url.is_some(),
                        token.is_some()
                    );
                    tokio::time::sleep(PARK_POLL).await;
                }
            }
        };

        let connect_url = build_connect_url(&server_url, &token);
        log::info!(
            "[nats] initial connect: url={} token={}",
            mask_connect_url(&connect_url),
            mask_token(&token)
        );

        let event_connector = connector.clone();
        let auth_connector = connector.clone();

        let connect_options = async_nats::ConnectOptions::new()
            .name(CLIENT_NAME)
            .user_and_password(NATS_USER.into(), NATS_PASS.into())
            .retry_on_initial_connect()
            .reconnect_delay_callback(reconnect_delay)
            .ping_interval(PING_INTERVAL)
            .event_callback(move |event| {
                let connector = event_connector.clone();
                async move {
                    handle_event(event, &connector).await;
                }
            })
            .auth_url_callback(move |()| {
                let connector = auth_connector.clone();
                async move {
                    // async-nats requires this future to be Sync; ours isn't
                    // (the token refresh runs an HTTP request). Hop through a
                    // spawned task — awaiting a JoinHandle is Sync.
                    let handle = tokio::spawn(async move { rebuild_connect_url(&connector).await });
                    handle.await.unwrap_or_else(|join_err| {
                        Err(async_nats::AuthError::new(format!(
                            "auth refresh task failed: {join_err}"
                        )))
                    })
                }
            });

        match connect_options.connect(&connect_url).await {
            Ok(client) => {
                *connector.connection.write().await = Some(client);
                log::info!(
                    "[nats] connect() returned Ok (TCP/WS handshake done; awaiting Connected event)"
                );
                // A tenant switch that landed mid-dial bounced off `dialing` and
                // has nothing left to re-trigger it — so check for one here
                // rather than sitting on a connection to the tenant just left.
                if read_server_url(&app).await.as_deref() == Some(server_url.as_str()) {
                    connector.dialing.store(false, Ordering::Release);
                    return;
                }
                log::info!("[nats] tenant changed while dialling — re-dialling");
                notifications::drop_subscription(&app, "tenant changed while dialling").await;
                *connector.connection.write().await = None;
            }
            Err(err) => {
                // With retry_on_initial_connect this only happens for
                // unrecoverable setup errors (e.g. URL parse) — but the host
                // can be corrected at runtime, so keep trying.
                log::error!("[nats] connect() failed: {err}; retrying in 10s");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

/// The body of `auth_url_callback`. The fork reaches it only from
/// `handle_auth_error`, i.e. on an AuthorizationViolation — so every invocation
/// does mean the server rejected the previous token, and from the second
/// consecutive one on we back off: a revoked token must not re-dial with full
/// TLS+WS handshakes every ~200ms forever. Asks for the token at the moment
/// async-nats needs it, so the reconnect uses the freshest rotation.
async fn rebuild_connect_url(connector: &Arc<Connector>) -> Result<String, async_nats::AuthError> {
    let failures = connector.auth_failures.fetch_add(1, Ordering::Relaxed);
    if failures > 0 {
        let delay_ms = backoff_ms(failures);
        log::warn!("[nats] {failures} consecutive auth failures — delaying reconnect {delay_ms}ms");
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    match credentials(&connector.app).await {
        (Some(url), Some(token)) => {
            log::info!(
                "[nats] auth_url_callback: supplying token for (re)connect ({})",
                mask_token(&token)
            );
            Ok(build_connect_url(&url, &token))
        }
        _ => Ok(park_until_credentials(connector).await),
    }
}

/// Hold the reconnect loop until someone is signed in again, and hand it a URL
/// when they are.
///
/// Returning `Err` from `auth_url_callback` does not stop anything: the fork
/// logs it and re-enters its connect loop, so a signed-out app kept dialling the
/// gateway — a full TLS+WS handshake every 30s, 660 rounds over two days after
/// one overnight logout, with the paired warnings for each. Nor can it be
/// stopped from outside: while the connector is between connections it is parked
/// inside its own connect loop and is not polling the client's command channel,
/// so dropping every `Client` handle is unobserved and leaves the task running.
///
/// Blocking here is what actually suspends it. `handle_auth_error` awaits this
/// callback with no timeout, and it is the task that needs stopping, so parking
/// in place needs no new state and cannot race the teardown paths. Resuming is
/// just returning, so sign-in needs no separate trigger.
async fn park_until_credentials(connector: &Arc<Connector>) -> String {
    log::info!("[nats] no credentials — parking the reconnect loop until sign-in");
    loop {
        tokio::time::sleep(PARK_POLL).await;
        if let (Some(url), Some(token)) = credentials(&connector.app).await {
            log::info!("[nats] credentials are back — resuming the reconnect loop");
            connector.auth_failures.store(0, Ordering::Relaxed);
            return build_connect_url(&url, &token);
        }
    }
}

async fn handle_event(event: Event, connector: &Arc<Connector>) {
    match event {
        Event::Connected => {
            log::info!("[nats] connected");
            connector.auth_failures.store(0, Ordering::Relaxed);
            let connector = connector.clone();
            tauri::async_runtime::spawn(async move {
                // The initial Connected can race run() storing the client;
                // wait for it so the subscribe below doesn't silently no-op.
                for _ in 0..50 {
                    if connector.connection.read().await.is_some() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let Some(client) = current_client(&connector).await else {
                    log::warn!("[nats] Connected event but no client stored — skipping subscribe");
                    return;
                };
                notifications::ensure_subscription(&connector.app, client).await;
            });
        }
        Event::Disconnected => log::info!("[nats] disconnected"),
        other => {
            // ClientError/ServerError/etc. — async-nats already logs the
            // underlying cause at warn/error.
            log::debug!("[nats] event: {other:?}");
        }
    }
}

/// Capped exponential backoff: BASE_DELAY_MS · 2^exp, at most MAX_DELAY_MS.
fn backoff_ms(exp: u32) -> u64 {
    BASE_DELAY_MS
        .saturating_mul(2u64.saturating_pow(exp.min(20)))
        .min(MAX_DELAY_MS)
}

/// `attempt` is 1-based — async-nats increments before invoking us, and the
/// returned delay precedes every dial including the first.
fn reconnect_delay(attempt: usize) -> Duration {
    let base_ms = if attempt <= FAST_RETRIES {
        FAST_DELAY_MS
    } else {
        backoff_ms((attempt - FAST_RETRIES - 1) as u32)
    };
    let jitter = 0.5 + rand::random::<f64>() * 0.5;
    Duration::from_millis((base_ms as f64 * jitter) as u64)
}

/// Everything except RFC 3986 unreserved characters gets percent-encoded — a
/// no-op for JWTs, and the same output the web client's URLSearchParams
/// produces for this query param.
fn build_connect_url(server_url: &str, token: &str) -> String {
    let (scheme, host) = match server_url.strip_prefix("http://") {
        Some(h) => ("ws", h),
        None => (
            "wss",
            server_url.strip_prefix("https://").unwrap_or(server_url),
        ),
    };
    let host = host.trim_end_matches('/');
    let token = utf8_percent_encode(token, UNRESERVED);
    format!("{scheme}://{host}{WS_PATH}?authorization={token}")
}

fn mask_token(token: &str) -> String {
    let n = token.chars().count();
    if n <= 8 {
        return "****".to_string();
    }
    let first: String = token.chars().take(4).collect();
    let last: String = token.chars().skip(n - 4).collect();
    format!("{first}...{last} (len {n})")
}

/// Masks the `authorization=` query param so the connect URL can be logged
/// without leaking the bearer token.
fn mask_connect_url(url: &str) -> String {
    match url.split_once("authorization=") {
        Some((base, token)) => format!("{base}authorization={}", mask_token(token)),
        None => url.to_string(),
    }
}
