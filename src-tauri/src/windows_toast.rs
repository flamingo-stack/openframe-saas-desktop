// Windows toast backend.
//
// Two activation paths, because no single one covers both halves of a toast:
//
//   - The body and "Open" activate the `openframe-desktop://notify` URI. Hand-
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
) {
    // Both dev and release post under the app identifier: the installer stamps
    // it onto the Start Menu shortcut, and `windows_activator::init` registers
    // it under HKCU as well, so a dev build no longer has to borrow
    // PowerShell's AUMID — which could never carry our activator's CLSID, and
    // so could never have had working buttons.
    let app_id = app.config().identifier.clone();
    let xml = toast_xml(&title, &body, click.as_ref(), user_id.as_deref(), kind);

    std::thread::spawn(move || match show(&app_id, &xml) {
        Ok(()) => log::info!("[notifications] toast fired: {title}"),
        Err(err) => log::warn!("[notifications] toast show failed: {err:?}"),
    });
}

#[cfg(target_os = "windows")]
fn show(app_id: &str, xml: &str) -> windows::core::Result<()> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id))?.Show(&toast)
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
        assert!(parse_action_args("openframe-desktop://notify?context=%7B%7D").is_none());
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
