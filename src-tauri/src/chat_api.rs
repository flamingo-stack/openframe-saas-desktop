// Authenticated chat-service calls the shell makes on its own, with no webview
// involved: the macOS notification action buttons (approve/reject an AI tool
// request, reply to a Mingo message) complete here, so the decision never has
// to open a window. Same endpoints and payloads as the frontend's
// mingo-api-service, in bearer mode.
//
// Base is the **tenant** host learned at login (`learned_host`), not the shared
// auth host: /chat lives on the tenant gateway.

use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tauri::AppHandle;

use crate::{load_config, tokens};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// An error body ends up in a notification banner, which shows a few lines.
const MAX_ERROR_CHARS: usize = 160;

/// RFC 3986 unreserved set: ids are interpolated into the path, so anything
/// else is escaped rather than trusted to be a UUID.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// A live signed-in session: a usable bearer and the user it belongs to.
/// Obtained through [`active_session`] — constructing one is the shell's proof
/// that someone is actually signed in.
pub(crate) struct Session {
    access_token: String,
    pub(crate) user_id: String,
}

/// The session the shell can act as right now, or `None` when nobody is signed
/// in — no tokens, a refresh token the gateway already rejected (`ensure_fresh`
/// clears those), an access token that outlived its `exp` with no working
/// refresh, or one carrying no `userId`.
///
/// Every background action goes through this: a notification sitting in
/// Notification Center is not authority to act, the session behind it is.
pub(crate) async fn active_session(app: &AppHandle) -> Option<Session> {
    let access_token = tokens::ensure_fresh(app).await.access_token?;
    if tokens::is_expired(&access_token) {
        log::info!("[chat-api] access token is past exp and could not be refreshed");
        return None;
    }
    let user_id = tokens::jwt_claim_str(&access_token, "userId")?;
    Some(Session {
        access_token,
        user_id,
    })
}

/// Resolve a Mingo tool-approval request. One endpoint for both outcomes, same
/// as the frontend's approve/reject mutations.
pub(crate) async fn resolve_approval(
    app: &AppHandle,
    session: &Session,
    request_id: &str,
    approve: bool,
) -> Result<(), String> {
    let path = format!(
        "/chat/api/v1/approval-requests/{}/approve",
        utf8_percent_encode(request_id, PATH_SEGMENT)
    );
    post_json(
        app,
        session,
        &path,
        serde_json::json!({ "approve": approve }),
    )
    .await
}

/// Send a message to a Mingo (admin AI) dialog.
pub(crate) async fn send_message(
    app: &AppHandle,
    session: &Session,
    dialog_id: &str,
    content: &str,
) -> Result<(), String> {
    post_json(
        app,
        session,
        "/chat/api/v1/messages",
        serde_json::json!({
            "dialogId": dialog_id,
            "content": content,
            "chatType": "ADMIN_AI_CHAT",
        }),
    )
    .await
}

/// Errors are user-facing (they end up in a follow-up notification), so they
/// carry the gateway's own message where there is one.
async fn post_json(
    app: &AppHandle,
    session: &Session,
    path: &str,
    body: serde_json::Value,
) -> Result<(), String> {
    let base = load_config(app)
        .learned_host
        .filter(|host| !host.is_empty())
        .ok_or("this install has not learned a tenant host yet")?;

    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("{base}{path}"))
        .bearer_auth(&session.access_token)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        // Always a non-empty body — see the 411 note on tokens::refresh.
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| {
            log::warn!("[chat-api] POST {path} to {base} failed: {e}");
            "the gateway could not be reached".to_string()
        })?;

    let status = response.status();
    if status.is_success() {
        log::info!("[chat-api] POST {path}: HTTP {status}");
        return Ok(());
    }
    // Body only at debug: an error message can quote user content.
    let reason = response.text().await.ok().and_then(|raw| {
        log::debug!("[chat-api] error body: {raw}");
        error_message(&raw)
    });
    log::warn!("[chat-api] POST {path} rejected: HTTP {status}");
    Err(reason.unwrap_or_else(|| format!("HTTP {status}")))
}

/// The gateway's error text: `message`, else `error` — the same two fields the
/// frontend's api-client reads. Truncated; a stack trace in a banner helps
/// nobody.
fn error_message(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let text = ["message", "error"]
        .into_iter()
        .find_map(|key| parsed.get(key)?.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())?;
    if text.chars().count() <= MAX_ERROR_CHARS {
        return Some(text.to_string());
    }
    Some(
        text.chars()
            .take(MAX_ERROR_CHARS - 1)
            .chain(std::iter::once('…'))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_prefers_the_gateways_text() {
        assert_eq!(
            error_message(r#"{"message":"Approval request already resolved"}"#).as_deref(),
            Some("Approval request already resolved")
        );
        assert_eq!(
            error_message(r#"{"error":"Forbidden"}"#).as_deref(),
            Some("Forbidden")
        );
        assert_eq!(error_message(r#"{"message":"  "}"#), None);
        assert_eq!(error_message("<html>502</html>"), None);
    }

    #[test]
    fn error_message_is_bounded() {
        let long = format!(r#"{{"message":"{}"}}"#, "x".repeat(4096));
        let message = error_message(&long).unwrap();
        assert_eq!(message.chars().count(), MAX_ERROR_CHARS);
        assert!(message.ends_with('…'));
    }
}
