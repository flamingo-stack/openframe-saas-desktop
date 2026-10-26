//! The download page's "Open the app" link: `openframe-console://download?id=<uuid>`.
//!
//! The id is the one the website put on the installer download, which the
//! gateway recorded together with where the visitor came from. Sent with the
//! signup as `attribution.downloadId`, it lets the backend credit that source to
//! an account made in the app, which no browser cookie reaches. The link carries
//! an id and nothing else: it brings the app up and drives nothing in it.
//!
//! The webview keeps the id until signup (in its localStorage), so the shell only
//! holds the latest one until the page takes it: on load, for a link that
//! started the app, and on [`EVENT_RECEIVED`] for one that arrives while it
//! runs. The event carries nothing; the page takes the id through the same
//! command either way, so a signal sent while the page was still loading costs
//! nothing.

use std::sync::Mutex;

use tauri::State;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tauri::{AppHandle, Emitter, Manager};

use crate::notifications::lock;

/// Host that marks a URL on [`crate::URI_SCHEME`] as an "Open the app" link.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
const HOST: &str = "download";

/// Consumed by `onDesktopDownloadLink` in the frontend's native-shell.ts; a
/// rename here silently stops the page from taking ids that arrive while it runs.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const EVENT_RECEIVED: &str = "download-link:received";

/// The latest download id, until the page takes it.
#[derive(Default)]
pub(crate) struct DownloadLinkState {
    pending: Mutex<Option<String>>,
}

/// Whether a URL is an "Open the app" link: our scheme, the `download` host
/// (compared case-insensitively, as the `auth` host is).
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn is_download_link(url: &url::Url) -> bool {
    url.scheme() == crate::URI_SCHEME
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(HOST))
}

/// The id an "Open the app" link carries, in the lowercase hyphenated form the
/// gateway records; `None` when it has none or it is not a UUID.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn download_id_of(url: &url::Url) -> Option<String> {
    let (_, id) = url.query_pairs().find(|(key, _)| key == "id")?;
    uuid::Uuid::try_parse(&id)
        .ok()
        .map(|id| id.hyphenated().to_string())
}

/// Take in a URL the OS delivered on our scheme. Returns `true` when it was an
/// "Open the app" link, usable id or not: either way the person asked for the
/// app, and the caller brings its window up.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn handle_url(app: &AppHandle, url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !is_download_link(&parsed) {
        return false;
    }
    match download_id_of(&parsed) {
        Some(id) => {
            log::info!("[download-link] download id received");
            *lock(&app.state::<DownloadLinkState>().pending) = Some(id);
            let _ = app.emit_to(crate::MAIN_LABEL, EVENT_RECEIVED, ());
        }
        None => log::info!("[download-link] link without a usable id, ignored"),
    }
    true
}

/// The download id an "Open the app" link brought, once: the page stores it
/// until signup.
#[tauri::command]
pub(crate) fn take_pending_download_id(state: State<'_, DownloadLinkState>) -> Option<String> {
    lock(&state.pending).take()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> url::Url {
        url::Url::parse(url).unwrap()
    }

    #[test]
    fn a_download_link_carries_its_id() {
        let url = parse("openframe-console://download?id=0f8fad5b-d9cb-469f-a165-70867728950e");
        assert!(is_download_link(&url));
        assert_eq!(
            download_id_of(&url).as_deref(),
            Some("0f8fad5b-d9cb-469f-a165-70867728950e")
        );
    }

    // The gateway records the id lowercased; the page sends what the shell hands it.
    #[test]
    fn the_id_comes_out_lowercase_and_hyphenated() {
        for id in [
            "0F8FAD5B-D9CB-469F-A165-70867728950E",
            "0f8fad5bd9cb469fa16570867728950e",
        ] {
            let url = parse(&format!("openframe-console://download?id={id}"));
            assert_eq!(
                download_id_of(&url).as_deref(),
                Some("0f8fad5b-d9cb-469f-a165-70867728950e"),
                "{id}"
            );
        }
    }

    #[test]
    fn a_link_without_a_uuid_has_no_id() {
        for url in [
            "openframe-console://download",
            "openframe-console://download?id=",
            "openframe-console://download?id=not-a-uuid",
            "openframe-console://download?download=0f8fad5b-d9cb-469f-a165-70867728950e",
        ] {
            let url = parse(url);
            assert!(is_download_link(&url), "{url}");
            assert_eq!(download_id_of(&url), None, "{url}");
        }
    }

    // `Url::parse` keeps a non-special scheme's host verbatim, like the `auth` host.
    #[test]
    fn the_host_is_matched_case_insensitively() {
        assert!(is_download_link(&parse(
            "openframe-console://DOWNLOAD?id=x"
        )));
    }

    // The two other hosts on the scheme, and the same host on another scheme, are not download links.
    #[test]
    fn other_urls_are_not_download_links() {
        for url in [
            "openframe-console://auth?devTicket=abc&id=0f8fad5b-d9cb-469f-a165-70867728950e",
            "openframe-console://notify?payload=x",
            "openframe://download?id=0f8fad5b-d9cb-469f-a165-70867728950e",
            "https://download/?id=0f8fad5b-d9cb-469f-a165-70867728950e",
        ] {
            assert!(!is_download_link(&parse(url)), "{url}");
        }
    }
}
