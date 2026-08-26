// Windows toast backend.
//
// Two activation paths, because no single one covers both halves of a toast:
//
//   - The body and "Open" activate the `openframe-console://notify` URI. Hand-
//     built rather than taken from a toast crate because those only support
//     foreground activation (in-process `Activated` handlers), which dies with
//     the process: clicks on toasts left in the Action Center by a previous app
//     session went nowhere. A warm click reaches the running instance through
//     single-instance argv forwarding, a cold click launches the app with the
//     URI in argv; both land in `notifications::handle_notification_uri`.
//
//   - Approve/Reject and the inline Reply activate the COM activator in
//     `windows_activator` (`activationType="background"`). Protocol activation
//     is not an option for them: it cannot carry a toast's text input at all,
//     and it cannot run in the process that is already running — so a decision
//     made from the banner would have to foreground the app to complete.
//
// Everything above the WinRT call is compiled for a macOS test run too, so the
// XML and the argument codec — the two things that fail silently, inside the
// notification platform rather than in a stack trace — stay testable on the host
// the rest of CI already uses.

use percent_encoding::utf8_percent_encode;
#[cfg(target_os = "windows")]
use tauri::{AppHandle, Manager};

#[cfg(target_os = "windows")]
use crate::notifications::Delivery;
use crate::{
    notification_actions::{Action, ActionContext, ActionKind},
    UNRESERVED,
};

/// The inline reply field's id. Windows keys the typed text by it in the
/// activator's `NOTIFICATION_USER_INPUT_DATA`, so the XML and `windows_activator`
/// have to agree.
const REPLY_INPUT_ID: &str = "reply";

const APPROVE_ACTION: &str = "approve";
const REJECT_ACTION: &str = "reject";
const REPLY_ACTION: &str = "reply";

/// Character budgets for the two fields the shell does not bound upstream: a
/// failed reply's body carries the user's own words back (once Windows clears
/// the inline field that banner is the only copy left, so `maybe_notify`'s
/// `BODY_CHARS` clamp deliberately does not apply), and the envelope's title is
/// never truncated at all.
///
/// Both are budgeted in characters against a document measured in bytes, so the
/// limits carry the worst-case expansion rather than the nominal length: a title
/// lands in the document once escaped **and** once percent-encoded per button,
/// which for an approval toast is three copies at up to ~12 bytes per character,
/// and `escape_xml` turns a single `'` into six. The one test that matters here
/// is `a_toast_stays_well_inside_the_payload_cap`, which feeds the worst case
/// rather than the nominal one — a toast over the platform's ~5 KB cap is not
/// rejected loudly, it simply never appears.
const TITLE_LIMIT: usize = 50;
const BODY_LIMIT: usize = 300;

/// Groups every toast the shell posts, so a tag only has to be unique within the
/// app. Windows matches tag **and** group, so both have to be set for a
/// replacement to land.
#[cfg(target_os = "windows")]
const TOAST_GROUP: &str = "openframe";
/// Windows caps a tag at 64 characters.
const TAG_LIMIT: usize = 64;

/// The logo behind a toast's **header icon**, which Windows reads through
/// `IconUri` on the AUMID registration (`windows_activator::register`).
///
/// The other place a toast can wear a logo — `<image placement="appLogoOverride">`
/// in the XML, the large image beside the text — is deliberately not used while
/// the notification layout is being designed.
///
/// Embedded rather than read out of the bundle so a dev build — no installer, no
/// resources laid down — wears the same logo as a shipped one.
#[cfg(target_os = "windows")]
const TOAST_LOGO: &[u8] = include_bytes!("../icons/128x128.png");
#[cfg(target_os = "windows")]
const TOAST_LOGO_FILE: &str = "notification-logo.png";

/// Post a notification. `user_id` is the user it is delivered to, encoded into
/// each button's arguments so a press hours later can check it still matches the
/// signed-in session; a follow-up that reports an action's outcome carries none.
#[cfg(target_os = "windows")]
pub(crate) fn post(
    app: &AppHandle,
    title: String,
    body: String,
    click: Option<serde_json::Value>,
    user_id: Option<String>,
    kind: ActionKind,
    delivery: Delivery,
) {
    // Both dev and release post under the app identifier: the installer stamps
    // it onto the Start Menu shortcut, and `windows_activator::init` registers
    // it under HKCU as well, so a dev build no longer has to borrow
    // PowerShell's AUMID — which could never carry our activator's CLSID, and
    // so could never have had working buttons.
    let app_id = app.config().identifier.clone();
    let xml = toast_xml(&title, &body, click.as_ref(), user_id.as_deref(), kind);
    // The toast's identity, and so what a later one replaces. Only approval
    // requests have one — they are the only kind the gateway republishes, and
    // the only kind whose second copy corrects the first rather than saying
    // something new. Keyed on the request rather than the notification id
    // because the two are 1:1 for an approval and the request id is already
    // carried on every path, including the follow-up posted after a press, which
    // lands on the toast it resolved for free.
    //
    // `maybe_notify` only marks an envelope `Update` when this is present — an
    // update with nothing to land on would be silenced into invisibility.
    let tag =
        crate::notification_actions::approval_request_id(click.as_ref()).map(|key| toast_tag(&key));

    std::thread::spawn(
        move || match show(&app_id, &xml, tag.as_deref(), delivery) {
            Ok(()) => log::info!("[notifications] toast fired: {title} ({delivery:?})"),
            Err(err) => log::warn!("[notifications] toast show failed: {err:?}"),
        },
    );
}

#[cfg(target_os = "windows")]
fn show(
    app_id: &str,
    xml: &str,
    tag: Option<&str>,
    delivery: Delivery,
) -> windows::core::Result<()> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    // Both of these refine a notification that is already worth showing, so
    // neither is allowed to `?` and take it down with it — the same trade
    // `toast_tag` makes: a stacked or noisy banner beats a lost one.
    //
    // A tagged toast *replaces* the one already carrying that tag instead of
    // stacking beside it. That is how a superseding envelope lands on the banner
    // it supersedes, and how the follow-up the shell posts after a press lands
    // on the banner it just resolved.
    let tagged = match tag {
        Some(tag) => toast
            .SetTag(&HSTRING::from(tag))
            .and_then(|()| toast.SetGroup(&HSTRING::from(TOAST_GROUP)))
            .inspect_err(|err| {
                log::warn!("[notifications] toast not tagged ({err:?}) — it will stack instead");
            })
            .is_ok(),
        None => false,
    };
    // An update goes straight to the Action Center with no banner: the user has
    // already been interrupted once for this notification, and what changed is
    // the outcome of a decision. Only worth doing if the tag landed, though —
    // silencing a toast that cannot replace anything files it where nobody will
    // look, so an untagged correction is better shown than hidden.
    // `SuppressPopup` lives on a later interface than the toast itself, so it
    // can fail by cast rather than by argument.
    if delivery == Delivery::Update && tagged {
        if let Err(err) = toast.SetSuppressPopup(true) {
            log::warn!("[notifications] update could not be silenced ({err:?})");
        }
    }
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id))?.Show(&toast)
}

/// Where the logo lives on disk, laying it down on first use — hence `ensure`,
/// as elsewhere in the crate. Called from the AUMID registration, which is the
/// only thing that reads it while the toast layout carries no image of its own.
///
/// It has to be a file because the notification platform resolves it out of
/// process, and it lives in the app-config directory rather than the install
/// directory because a toast sitting in the Action Center still resolves it after
/// the app has exited, or been uninstalled.
///
/// Resolved once per process. `None` costs the logo, never the notification.
#[cfg(target_os = "windows")]
pub(crate) fn ensure_logo(app: &AppHandle) -> Option<&'static std::path::Path> {
    use std::sync::OnceLock;
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = app.path().app_config_dir().ok()?;
        let path = dir.join(TOAST_LOGO_FILE);
        // Rewritten only when missing or a different size, and then through a
        // temporary that is renamed into place: a plain write truncates first,
        // and Windows caches whatever it read of the identity until the
        // notification service restarts — so being caught mid-write is not an
        // error that corrects itself on the next toast.
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.len() == TOAST_LOGO.len() as u64) {
            if let Err(err) = write_logo(&dir, &path) {
                log::warn!("[notifications] toast logo not written ({err}) — toasts go without");
                return None;
            }
            log::info!("[notifications] toast logo written to {}", path.display());
        }
        Some(path)
    })
    .as_deref()
}

/// Write the logo through a temporary and rename it into place, so a reader
/// never sees a partial file — Windows caches whatever it read of the identity
/// until the notification service restarts, so being caught mid-write is not a
/// mistake the next toast corrects.
///
/// Like `crate::write_atomic`, but the staging path carries the process id: two
/// instances laying the same file down would otherwise share one temporary, and
/// the second's write would truncate what the first is about to rename into
/// place — reintroducing the torn file this exists to prevent.
#[cfg(target_os = "windows")]
fn write_logo(dir: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    // Cleaned up on either failure: the name carries a fresh process id every
    // launch, so a staging file left behind is one nothing will ever reclaim.
    let staged = std::fs::write(&tmp, TOAST_LOGO)
        // Windows renames over an existing file here (MOVEFILE_REPLACE_EXISTING).
        .and_then(|()| std::fs::rename(&tmp, path));
    staged.inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Windows matches a tag verbatim and rejects `SetTag` for one it does not
/// accept, which would fail the whole `show` — so an id is folded to fit rather
/// than passed through. Losing the replacement costs a duplicate toast; losing
/// the toast costs the notification.
fn toast_tag(key: &str) -> String {
    key.chars()
        .take(TAG_LIMIT)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Silent to match the previously shipped toasts. `useButtonStyle` is what lets
/// Reject render red — the closest Windows has to macOS's Destructive, and worth
/// having for the same reason: denying a tool execution is the fail-safe
/// direction, so it must not read as the heavier choice.
///
/// Unlike macOS, which folds every action behind the banner's "Options" chevron,
/// Windows renders these inline — so the approval decision is actually visible
/// on the banner here.
fn toast_xml(
    title: &str,
    body: &str,
    click: Option<&serde_json::Value>,
    user_id: Option<&str>,
    kind: ActionKind,
) -> String {
    use crate::notifications::truncate_for_notification;

    // Escaped once: the body click's target and the "Open" button's target are
    // the same URI and must stay identical.
    let uri = escape_xml(&crate::notifications::click_uri(click));
    // Clamped before anything copies it: the title goes into the visual text and
    // into every button's arguments.
    let title = truncate_for_notification(title, TITLE_LIMIT);
    let body_element = if body.is_empty() {
        String::new()
    } else {
        format!(
            "<text>{}</text>",
            escape_xml(&truncate_for_notification(body, BODY_LIMIT))
        )
    };
    let open = format!(r#"<action content="Open" activationType="protocol" arguments="{uri}"/>"#);
    let button = |action: &str, content: &str, style: &str| {
        format!(
            r#"<action content="{content}" activationType="background" arguments="{}" {style}/>"#,
            escape_xml(&encode_action_args(action, &title, user_id, click))
        )
    };
    // No "Open" on the approval toast: the body click already opens the app, so
    // the buttons are left to carry the decision and nothing else.
    let actions = match kind {
        ActionKind::Default => open,
        ActionKind::Approval => format!(
            "{}{}",
            button(APPROVE_ACTION, "Approve", r#"hint-buttonStyle="Success""#),
            button(REJECT_ACTION, "Reject", r#"hint-buttonStyle="Critical""#),
        ),
        ActionKind::Message => {
            let send = button(
                REPLY_ACTION,
                "Send",
                &format!(r#"hint-inputId="{REPLY_INPUT_ID}""#),
            );
            format!(
                r#"<input id="{REPLY_INPUT_ID}" type="text" placeHolderContent="Reply to Mingo"/>{send}{open}"#
            )
        }
    };

    format!(
        r#"<toast duration="short" activationType="protocol" launch="{uri}" useButtonStyle="true">
    <visual><binding template="ToastGeneric"><text>{title}</text>{body_element}</binding></visual>
    <audio silent="true"/>
    <actions>{actions}</actions>
</toast>"#,
        title = escape_xml(&title),
    )
}

// ---------------------------------------------------------------------------
// Button arguments
// ---------------------------------------------------------------------------

/// What a pressed button carries back through the COM activator: which button it
/// was, and the context the action runs under.
///
/// Windows has no per-notification sidecar like macOS's `userInfo`, so
/// everything an action needs rides in the button's own `arguments` string —
/// including the user the notification was delivered to, which is the value the
/// session gate compares against, and the original title, which a follow-up
/// notification quotes so the outcome names what it resolved.
pub(crate) struct ToastAction {
    pub(crate) action: String,
    pub(crate) context: ActionContext,
}

fn encode_action_args(
    action: &str,
    title: &str,
    user_id: Option<&str>,
    click: Option<&serde_json::Value>,
) -> String {
    let encode = |value: &str| utf8_percent_encode(value, UNRESERVED).to_string();
    let mut args = format!("action={action}&title={}", encode(title));
    if let Some(user_id) = user_id {
        args.push_str(&format!("&user={}", encode(user_id)));
    }
    if let Some(context) = click.and_then(|click| click.get("context")) {
        args.push_str(&format!("&context={}", encode(&context.to_string())));
    }
    args
}

/// Inverse of [`encode_action_args`]. `None` when the string names no action —
/// a body click or "Open" activates the URI instead and never reaches here, so
/// that means a toast from a build that encoded its buttons differently.
/// Unparsable pairs are skipped rather than failing the whole string: losing the
/// title costs a worse follow-up, losing the action costs the press.
pub(crate) fn parse_action_args(args: &str) -> Option<ToastAction> {
    let mut parsed = ToastAction {
        action: String::new(),
        context: ActionContext {
            title: String::new(),
            user_id: None,
            payload: None,
        },
    };
    for (key, value) in args.split('&').filter_map(|pair| pair.split_once('=')) {
        let Some(value) = percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .ok()
            .map(|value| value.into_owned())
        else {
            continue;
        };
        match key {
            "action" => parsed.action = value,
            "title" => parsed.context.title = value,
            "user" => parsed.context.user_id = Some(value).filter(|user| !user.is_empty()),
            "context" => {
                parsed.context.payload = crate::notifications::payload_from_context_json(&value)
            }
            _ => {}
        }
    }
    (!parsed.action.is_empty()).then_some(parsed)
}

/// What a press means. Windows hands the activator an argument string and a bag
/// of input fields; these are the only three things they can add up to.
pub(crate) enum Press {
    Run(Action),
    /// Nothing typed in the reply field — not a failure worth a notification.
    Ignore,
    /// An action string this build does not know. The safe answer is the window,
    /// not a guess at which decision the user meant.
    Unknown,
}

pub(crate) fn press_for(activation: &ToastAction, inputs: &[(String, String)]) -> Press {
    match activation.action.as_str() {
        APPROVE_ACTION => Press::Run(Action::Approve),
        REJECT_ACTION => Press::Run(Action::Reject),
        REPLY_ACTION => {
            // Windows keys the typed text by the input's id. Anything else means
            // the toast and this build disagree about that id, and the field is
            // already cleared by the time we are here — so there is no second
            // copy of the user's words to fall back on.
            let Some((_, text)) = inputs.iter().find(|(key, _)| key == REPLY_INPUT_ID) else {
                log::warn!("[notifications] reply activation carried no text input");
                return Press::Ignore;
            };
            match text.trim() {
                "" => Press::Ignore,
                text => Press::Run(Action::Reply(text.to_string())),
            }
        }
        _ => Press::Unknown,
    }
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 has no representation for the other C0 controls, not even
            // a numeric reference — a description quoting raw tool output can
            // carry one, and it would make `LoadXml` reject the whole document,
            // losing the notification rather than the character.
            c if c < '\x20' && !matches!(c, '\t' | '\n' | '\r') => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_click() -> serde_json::Value {
        serde_json::json!({ "context": {
            "type": "ADMIN_APPROVAL_REQUEST",
            "approvalRequestId": "0a2a0b3c-9d1e-4f5a-8b7c-6d5e4f3a2b1c",
        } })
    }

    #[test]
    fn action_args_roundtrip() {
        let click = approval_click();
        let args = encode_action_args(APPROVE_ACTION, "Mingo", Some("user-1"), Some(&click));
        let parsed = parse_action_args(&args).unwrap();
        assert_eq!(parsed.action, APPROVE_ACTION);
        assert_eq!(parsed.context.title, "Mingo");
        assert_eq!(parsed.context.user_id.as_deref(), Some("user-1"));
        assert_eq!(parsed.context.payload.as_ref(), Some(&click));
    }

    /// The separators the codec itself uses are exactly what a title or an id
    /// can contain, so this is the case that decides whether a press resolves
    /// the right request.
    #[test]
    fn action_args_survive_reserved_chars() {
        let click = serde_json::json!({ "context": {
            "type": "ADMIN_AI_MESSAGE", "dialogId": "abc/д ф&x=1",
        } });
        let args = encode_action_args(REPLY_ACTION, "a&b=c д", Some("u&1"), Some(&click));
        let parsed = parse_action_args(&args).unwrap();
        assert_eq!(parsed.context.title, "a&b=c д");
        assert_eq!(parsed.context.user_id.as_deref(), Some("u&1"));
        assert_eq!(parsed.context.payload.as_ref(), Some(&click));
    }

    /// A follow-up that reports an outcome carries no user, and so cannot be
    /// used to act again — its buttons would fail the session gate anyway, but
    /// the value is simply absent rather than empty.
    #[test]
    fn action_args_without_a_user_or_context_still_parse() {
        let parsed = parse_action_args(&encode_action_args(REJECT_ACTION, "Mingo", None, None));
        let parsed = parsed.unwrap();
        assert_eq!(parsed.context.user_id, None);
        assert_eq!(parsed.context.payload, None);
    }

    #[test]
    fn strings_that_name_no_action_are_rejected() {
        assert!(parse_action_args("").is_none());
        assert!(parse_action_args("title=Mingo&user=u1").is_none());
        assert!(parse_action_args("openframe-console://notify?context=%7B%7D").is_none());
        // A malformed pair is skipped, not fatal.
        assert_eq!(
            parse_action_args("garbage&action=approve").unwrap().action,
            APPROVE_ACTION
        );
    }

    /// The whole toast payload is capped at ~5 KB by the notification platform,
    /// and the arguments are repeated once per button. 4096 is that cap with
    /// enough headroom left that a regression trips here rather than there,
    /// where the only symptom is a toast that never appears.
    ///
    /// Fed the worst case rather than the nominal one, because every clamp in
    /// this file counts characters while the cap counts bytes: `'` sextuples
    /// under `escape_xml`, and a 4-byte character becomes 12 under
    /// percent-encoding. ASCII input would pass this test with limits three
    /// times too generous.
    #[test]
    fn a_toast_stays_well_inside_the_payload_cap() {
        for filler in ["'", "🔒", "д", "x"] {
            let xml = toast_xml(
                &filler.repeat(4096),
                &filler.repeat(64 * 1024),
                Some(&approval_click()),
                Some("0a2a0b3c-9d1e-4f5a-8b7c-6d5e4f3a2b1c"),
                ActionKind::Approval,
            );
            assert!(
                xml.len() < 4096,
                "toast xml of {filler} was {} bytes",
                xml.len()
            );
        }
    }

    /// Activation type is the difference between a button that completes the
    /// decision and one that silently does nothing: protocol activation carries
    /// no text input and cannot run in-process.
    #[test]
    fn actionable_toasts_activate_the_com_activator() {
        let approval = toast_xml(
            "t",
            "b",
            Some(&approval_click()),
            Some("u1"),
            ActionKind::Approval,
        );
        assert_eq!(
            approval.matches(r#"activationType="background""#).count(),
            2
        );
        assert!(approval.contains(r#"hint-buttonStyle="Critical""#));
        assert!(!approval.contains(r#"content="Open""#));

        let message = toast_xml(
            "t",
            "b",
            Some(&serde_json::json!({ "context": {
                "type": "ADMIN_AI_MESSAGE", "dialogId": "d1",
            } })),
            Some("u1"),
            ActionKind::Message,
        );
        assert!(message.contains(&format!(r#"<input id="{REPLY_INPUT_ID}""#)));
        assert!(message.contains(&format!(r#"hint-inputId="{REPLY_INPUT_ID}""#)));
        assert!(message.contains(r#"content="Open""#));
    }

    fn press(action: &str, inputs: &[(&str, &str)]) -> Press {
        let activation = parse_action_args(&encode_action_args(action, "t", None, None)).unwrap();
        let inputs: Vec<(String, String)> = inputs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        press_for(&activation, &inputs)
    }

    #[test]
    fn each_button_runs_its_own_action() {
        assert!(matches!(
            press(APPROVE_ACTION, &[]),
            Press::Run(Action::Approve)
        ));
        assert!(matches!(
            press(REJECT_ACTION, &[]),
            Press::Run(Action::Reject)
        ));
        match press(REPLY_ACTION, &[(REPLY_INPUT_ID, "  ship it  ")]) {
            Press::Run(Action::Reply(text)) => assert_eq!(text, "ship it"),
            _ => panic!("a typed reply must run"),
        }
    }

    /// An empty reply is not a failure — reporting one would put a notification
    /// on screen for a user who typed nothing and dismissed the field.
    #[test]
    fn a_reply_with_nothing_in_it_does_nothing() {
        assert!(matches!(
            press(REPLY_ACTION, &[(REPLY_INPUT_ID, "   ")]),
            Press::Ignore
        ));
        assert!(matches!(
            press(REPLY_ACTION, &[("other-field", "ship it")]),
            Press::Ignore
        ));
    }

    /// A gateway description quoting raw tool output can carry a control
    /// character, and XML 1.0 has no way to represent one — the document would
    /// be rejected whole, so the notification would be lost rather than the
    /// character.
    #[test]
    fn control_characters_cannot_reject_the_document() {
        let xml = toast_xml(
            "Tool output\x1b[0m",
            "ran \x07alert\x0b",
            None,
            Some("u1"),
            ActionKind::Default,
        );
        assert!(!xml.contains('\x1b') && !xml.contains('\x07') && !xml.contains('\x0b'));
        // The three XML 1.0 allows are left alone.
        assert!(escape_xml("a\tb\nc\rd").contains("a\tb\nc\rd"));
    }

    /// `SetTag` rejecting a value fails the whole `show`, so the tag is folded
    /// to something Windows takes rather than handed the id verbatim.
    #[test]
    fn a_tag_is_always_something_windows_accepts() {
        assert_eq!(
            toast_tag("6a706b66d453f877ae41f42c"),
            "6a706b66d453f877ae41f42c"
        );
        assert_eq!(toast_tag("a/b c&д"), "a-b-c--");
        let long = toast_tag(&"x".repeat(4096));
        assert_eq!(long.chars().count(), TAG_LIMIT);
    }

    /// The layout is being designed and the large image is out of it for now, so
    /// the only logo a toast wears comes from the AUMID registration's `IconUri`
    /// — which is not part of this document.
    #[test]
    fn the_toast_carries_no_image_of_its_own() {
        for kind in [
            ActionKind::Default,
            ActionKind::Approval,
            ActionKind::Message,
        ] {
            let xml = toast_xml("t", "b", Some(&approval_click()), Some("u1"), kind);
            assert!(!xml.contains("<image"), "{kind:?} toast carried an image");
        }
    }

    #[test]
    fn an_unrecognized_action_falls_back_to_the_window() {
        assert!(matches!(press("detonate", &[]), Press::Unknown));
    }

    /// Everything else keeps the one-button protocol toast it has always had.
    #[test]
    fn plain_toasts_keep_protocol_activation_only() {
        let xml = toast_xml("Ticket updated", "", None, Some("u1"), ActionKind::Default);
        assert!(!xml.contains("background"));
        assert_eq!(xml.matches(r#"activationType="protocol""#).count(), 2);
    }
}
