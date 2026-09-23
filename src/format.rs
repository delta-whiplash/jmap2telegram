use chrono_tz::Tz;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::jmap::EmailSummary;

/// Telegram's hard limit for a single message's text.
pub const TELEGRAM_MAX_MESSAGE_LEN: usize = 4096;

/// Every field below (subject, sender name, preview) comes straight from
/// whatever mail showed up in the mailbox, so all of them are bounded
/// before being sent: an unbounded field could push the rendered message
/// past Telegram's 4096-char limit, and `send_message` would then fail
/// for that notification alone — silently and permanently losing it,
/// since the JMAP sync cursor still advances past it.
const FIELD_LIMIT: usize = 300;
const PREVIEW_LIMIT: usize = 400;

fn truncate(input: &str, limit: usize) -> String {
    if input.chars().count() > limit {
        input.chars().take(limit).collect::<String>() + "…"
    } else {
        input.to_string()
    }
}

/// `account_label` is `Some(name)` for a notification coming from a shared
/// JMAP account rather than the chat's own mailbox, so the user can tell
/// them apart; `None` renders exactly as before (the common single-mailbox
/// case).
pub fn notification_text(summary: &EmailSummary, tz: Tz, account_label: Option<&str>) -> String {
    let from = from_field(summary.from_name.as_deref(), summary.from_addr.as_deref());

    let when = summary
        .received_at
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| {
            dt.with_timezone(&tz)
                .format("%d/%m/%Y %H:%M %Z")
                .to_string()
        })
        .unwrap_or_default();

    let subject = truncate(summary.subject.trim(), FIELD_LIMIT);
    let preview = blockquote(&escape_markdown(&truncate(
        summary.preview.trim(),
        PREVIEW_LIMIT,
    )));

    let mailbox_line = account_label
        .map(|label| {
            format!(
                "*Boîte :* {}\n",
                escape_markdown(&truncate(label, FIELD_LIMIT))
            )
        })
        .unwrap_or_default();

    let attachments_line = attachments_field(&summary.attachments);

    format!(
        "📧 *Nouveau message*\n\n{mailbox_line}*De :* {from}\n*Objet :* {subject}\n*Reçu :* {when}\n{attachments_line}\n{preview}",
        subject = escape_markdown(&subject),
        when = escape_markdown(&when),
    )
}

/// Attachments are never downloaded (this bot fetches message bodies on
/// demand only, never files), but naming them means "there's a 4 MB PDF
/// here" isn't silently invisible — just capped at a handful of names so a
/// message with hundreds of parts can't blow out the notification.
fn attachments_field(attachments: &[(String, usize)]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    const MAX_SHOWN: usize = 5;

    let shown = attachments
        .iter()
        .take(MAX_SHOWN)
        .map(|(name, size)| {
            // The parens here are literal text, not link syntax, so they
            // need the same MarkdownV2 escaping as any other reserved
            // character — an unescaped '(' or ')' would otherwise make
            // Telegram reject the whole message with a 400.
            format!(
                "{} \\({}\\)",
                escape_markdown(&truncate(name, FIELD_LIMIT)),
                // human_size can render a decimal point ("1.4 Mo"), itself
                // a reserved MarkdownV2 character.
                escape_markdown(&human_size(*size))
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    let extra = attachments.len().saturating_sub(MAX_SHOWN);
    let suffix = if extra > 0 {
        // '+', '(' and ')' are all MarkdownV2-reserved even as plain,
        // bot-generated text — same class of bug as the per-attachment
        // parens above.
        format!(" \\(\\+{extra} autres\\)")
    } else {
        String::new()
    };

    format!("📎 *Pièces jointes :* {shown}{suffix}\n")
}

fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} Mo", bytes / MB)
    } else if bytes >= KB {
        format!("{:.0} Ko", bytes / KB)
    } else {
        format!("{} o", bytes as u64)
    }
}

/// Renders the sender as a `mailto:` link when an address is available, so
/// tapping it opens the recipient's mail app — a small but genuinely useful
/// bit of native richness GmailBot doesn't offer. Falls back to plain
/// escaped text when there's nothing to link (no address, or an address
/// that's entirely whitespace/control characters once sanitized).
fn from_field(name: Option<&str>, addr: Option<&str>) -> String {
    let addr = addr.map(|a| truncate(a, FIELD_LIMIT));
    let display = match (name, &addr) {
        (Some(name), Some(addr)) if !name.is_empty() => {
            format!("{} <{}>", truncate(name, FIELD_LIMIT), addr)
        }
        (_, Some(addr)) => addr.clone(),
        _ => return escape_markdown("(expéditeur inconnu)"),
    };

    match addr.as_deref().map(mailto_url).filter(|u| !u.is_empty()) {
        Some(url) => format!(
            "[{}](mailto:{})",
            escape_markdown(&display),
            escape_markdown_url(&url)
        ),
        None => escape_markdown(&display),
    }
}

/// A `From` header is attacker-controlled and not guaranteed to be a
/// well-formed address; a `mailto:` URL has to stay on one line, so strip
/// whitespace/control characters rather than let them break it across the
/// rendered message.
fn mailto_url(addr: &str) -> String {
    addr.chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect()
}

/// Escapes text for the `(...)` part of a MarkdownV2 inline link: per
/// Telegram's spec, only `)` and `\` need escaping there (the general
/// `escape_markdown` rules don't apply inside a URL).
fn escape_markdown_url(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if c == ')' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Renders already-escaped text as a native MarkdownV2 blockquote (each
/// line prefixed with `>`), so the message preview reads as a visually
/// distinct quoted block instead of running into the surrounding text.
fn blockquote(escaped: &str) -> String {
    escaped
        .lines()
        .map(|line| format!(">{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Telegram rejects any inline button whose `callback_data` exceeds 64
/// bytes. JMAP does not bound the length of an email id, so a server
/// returning long opaque ids would otherwise produce a silently-broken
/// button (the message sends, the tap does nothing). Rather than fail
/// open, drop the action buttons entirely for that message when the id
/// doesn't fit; the notification text itself is unaffected.
const TELEGRAM_CALLBACK_DATA_MAX_LEN: usize = 64;

/// Encodes which JMAP account an action button's email id belongs to.
/// `account_id` is `None` for the chat's own mailbox (the common case,
/// `"{action}:{email_id}"`) and `Some(id)` for a shared account
/// (`"{action}:{id}:{email_id}"`), since JMAP email ids are only unique
/// within their account and the callback handler must route the action to
/// a JMAP client connected to the right one.
fn action_callback_data(action: char, account_id: Option<&str>, email_id: &str) -> String {
    match account_id {
        Some(id) => format!("{action}:{id}:{email_id}"),
        None => format!("{action}:{email_id}"),
    }
}

/// `already_read` swaps the "✅ Lu" action button for an inert "✔️ Lu"
/// label (still a valid, tappable no-op button rather than a dead one) —
/// used to edit a notification's keyboard in place right after marking it
/// read, so the chat itself reflects that state instead of staying silent
/// about it until the next glance at the mailbox.
pub fn notification_keyboard(
    email_id: &str,
    account_id: Option<&str>,
    already_read: bool,
) -> InlineKeyboardMarkup {
    // All action letters are 1 byte, so any is representative for the
    // length check.
    if action_callback_data('d', account_id, email_id).len() > TELEGRAM_CALLBACK_DATA_MAX_LEN {
        return InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new());
    }

    let read_button = if already_read {
        InlineKeyboardButton::callback("✔️ Lu", action_callback_data('n', account_id, email_id))
    } else {
        InlineKeyboardButton::callback("✅ Lu", action_callback_data('r', account_id, email_id))
    };

    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback(
                "📖 Lire tout",
                action_callback_data('f', account_id, email_id),
            ),
            read_button,
        ],
        vec![
            InlineKeyboardButton::callback(
                "📥 Archiver",
                action_callback_data('a', account_id, email_id),
            ),
            InlineKeyboardButton::callback(
                "🚫 Spam",
                action_callback_data('j', account_id, email_id),
            ),
        ],
        vec![InlineKeyboardButton::callback(
            "🗑 Supprimer",
            action_callback_data('d', account_id, email_id),
        )],
    ])
}

/// Replaces a triage action's cleared keyboard with a single "↩️ Annuler"
/// button, so archiving/marking spam/deleting is a one-tap mistake to
/// recover from instead of a silent, final action.
pub fn undo_keyboard(email_id: &str, account_id: Option<&str>) -> InlineKeyboardMarkup {
    if action_callback_data('u', account_id, email_id).len() > TELEGRAM_CALLBACK_DATA_MAX_LEN {
        return InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new());
    }
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "↩️ Annuler",
        action_callback_data('u', account_id, email_id),
    )]])
}

/// Builds the `/partages` toggle menu: one button per shared JMAP account
/// visible to the token right now, checked when the chat is currently
/// opted into notifications for it. An account whose id is too long to fit
/// Telegram's callback_data limit is left out rather than shown as a
/// button that can never be tapped successfully.
pub fn shared_accounts_keyboard(
    accounts: &[(String, String)],
    enabled: &std::collections::HashSet<String>,
) -> InlineKeyboardMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = accounts
        .iter()
        .filter(|(id, _)| format!("s:{id}").len() <= TELEGRAM_CALLBACK_DATA_MAX_LEN)
        .map(|(id, name)| {
            let mark = if enabled.contains(id) { "✅" } else { "⬜" };
            vec![InlineKeyboardButton::callback(
                format!("{mark} {name}"),
                format!("s:{id}"),
            )]
        })
        .collect();
    InlineKeyboardMarkup::new(rows)
}

/// Escapes text for Telegram's `MarkdownV2` parser. All content here
/// (subject, sender, preview) is attacker-controlled (it's whatever showed
/// up in the mailbox), so every special character in Telegram's reserved
/// set must be escaped to avoid parse errors or formatting injection.
fn escape_markdown(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(
            c,
            '_' | '*'
                | '['
                | ']'
                | '('
                | ')'
                | '~'
                | '`'
                | '>'
                | '#'
                | '+'
                | '-'
                | '='
                | '|'
                | '{'
                | '}'
                | '.'
                | '!'
                | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Splits a long plain-text body into chunks that fit within Telegram's
/// message size limit, breaking on line boundaries where possible.
pub fn chunk_text(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() {
        return vec!["(message vide)".to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        if current.chars().count() + line.chars().count() + 1 > max_len {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            if line.chars().count() > max_len {
                for part in line
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(max_len)
                    .map(|c| c.iter().collect::<String>())
                {
                    chunks.push(part);
                }
                continue;
            }
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(
        subject: &str,
        from_name: Option<&str>,
        from_addr: Option<&str>,
        preview: &str,
    ) -> EmailSummary {
        EmailSummary {
            id: "M123".to_string(),
            subject: subject.to_string(),
            from_name: from_name.map(str::to_string),
            from_addr: from_addr.map(str::to_string),
            preview: preview.to_string(),
            received_at: Some(1_700_000_000),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn notification_text_includes_sender_and_subject() {
        let s = summary(
            "Hello",
            Some("Alice"),
            Some("alice@example.org"),
            "Hi there",
        );
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains("Alice"));
        // '.' is a MarkdownV2 special char, so the address is expected
        // escaped in the rendered text, not verbatim.
        assert!(text.contains(r"alice@example\.org"));
        assert!(text.contains("Hello"));
        assert!(text.contains("Hi there"));
    }

    #[test]
    fn notification_text_falls_back_when_sender_unknown() {
        let s = summary("Hello", None, None, "Hi there");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains("expéditeur inconnu"));
    }

    #[test]
    fn notification_text_uses_address_when_name_missing() {
        let s = summary("Hello", None, Some("bob@example.org"), "Hi");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains(r"bob@example\.org"));
    }

    #[test]
    fn notification_text_escapes_markdown_special_chars_in_subject() {
        // A malicious/unlucky subject line must never be able to inject
        // Markdown formatting or break message parsing.
        let s = summary(
            "*bold* _italic_ [link](evil) `code`",
            None,
            Some("a@b.c"),
            "",
        );
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains(r"\*bold\* \_italic\_ \[link\]\(evil\) \`code\`"));
    }

    #[test]
    fn notification_text_truncates_long_preview() {
        let long_preview = "a".repeat(1000);
        let s = summary("Subject", None, Some("a@b.c"), &long_preview);
        let text = notification_text(&s, Tz::UTC, None);
        // 400 chars kept + ellipsis marker, well short of the original 1000.
        assert!(text.len() < 700);
        assert!(text.contains('…'));
    }

    #[test]
    fn notification_text_truncates_malicious_long_subject() {
        // A hostile sender could otherwise push the rendered message past
        // Telegram's 4096-char cap and get that notification silently
        // dropped (send_message fails, but the JMAP cursor still advances).
        let long_subject = "s".repeat(5000);
        let s = summary(&long_subject, None, Some("a@b.c"), "preview");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
        assert!(text.contains('…'));
    }

    #[test]
    fn notification_text_truncates_malicious_long_sender_name() {
        let long_name = "n".repeat(5000);
        let s = summary("Subject", Some(&long_name), Some("a@b.c"), "preview");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
    }

    #[test]
    fn notification_text_truncates_malicious_long_sender_address() {
        // The sender name isn't the only unbounded field JMAP hands us —
        // a crafted long From address must be capped too, whether or not
        // a display name is also present.
        let long_addr = format!("{}@example.org", "a".repeat(5000));
        let s = summary("Subject", None, Some(&long_addr), "preview");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);

        let s_with_name = summary("Subject", Some("Alice"), Some(&long_addr), "preview");
        let text_with_name = notification_text(&s_with_name, Tz::UTC, None);
        assert!(text_with_name.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
    }

    #[test]
    fn notification_text_renders_received_at_in_the_configured_timezone() {
        // 1_700_000_000 is 2023-11-14T22:13:20Z; Europe/Paris was on
        // CET (UTC+1) at that instant, so the rendered hour must differ
        // from the UTC rendering rather than silently always being UTC.
        let s = summary("Subject", None, Some("a@b.c"), "preview");
        let utc_text = notification_text(&s, Tz::UTC, None);
        let paris_text = notification_text(&s, chrono_tz::Europe::Paris, None);
        assert!(utc_text.contains("22:13"));
        assert!(paris_text.contains("23:13"));
        assert_ne!(utc_text, paris_text);
    }

    #[test]
    fn notification_keyboard_has_action_buttons_for_short_id() {
        let kb = notification_keyboard("M123", None, false);
        assert_eq!(kb.inline_keyboard.len(), 3);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
        assert_eq!(kb.inline_keyboard[1].len(), 2);
        assert_eq!(kb.inline_keyboard[2].len(), 1);
    }

    #[test]
    fn notification_keyboard_spam_button_uses_junk_action() {
        let kb = notification_keyboard("M123", None, false);
        let InlineKeyboardButton {
            kind: teloxide::types::InlineKeyboardButtonKind::CallbackData(data),
            ..
        } = &kb.inline_keyboard[1][1]
        else {
            panic!("expected a callback button");
        };
        assert_eq!(data, "j:M123");
    }

    #[test]
    fn notification_keyboard_already_read_shows_inert_check() {
        let unread = notification_keyboard("M123", None, false);
        let read = notification_keyboard("M123", None, true);

        let read_button_data = |kb: &InlineKeyboardMarkup| {
            let InlineKeyboardButton {
                text,
                kind: teloxide::types::InlineKeyboardButtonKind::CallbackData(data),
            } = &kb.inline_keyboard[0][1]
            else {
                panic!("expected a callback button");
            };
            (text.clone(), data.clone())
        };

        let (unread_text, unread_data) = read_button_data(&unread);
        assert_eq!(unread_text, "✅ Lu");
        assert_eq!(unread_data, "r:M123");

        let (read_text, read_data) = read_button_data(&read);
        assert_eq!(read_text, "✔️ Lu");
        assert_eq!(read_data, "n:M123");
    }

    #[test]
    fn notification_keyboard_drops_buttons_when_id_too_long_for_telegram() {
        // Telegram's callback_data hard limit is 64 bytes; a pathological
        // JMAP id must not produce a button Telegram will silently reject.
        let long_id = "x".repeat(100);
        let kb = notification_keyboard(&long_id, None, false);
        assert!(kb.inline_keyboard.is_empty());
    }

    #[test]
    fn notification_text_renders_sender_as_a_mailto_link() {
        let s = summary(
            "Hello",
            Some("Alice"),
            Some("alice@example.org"),
            "Hi there",
        );
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains("(mailto:alice@example.org)"));
    }

    #[test]
    fn notification_text_mailto_link_survives_a_hostile_address() {
        // A From header is attacker-controlled; ')' and '\' in the address
        // must not be able to break out of the MarkdownV2 link's URL part.
        let hostile_addr = "a)\\evil@example.org";
        let s = summary("Hello", None, Some(hostile_addr), "Hi");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains(r"(mailto:a\)\\evil@example.org)"));
    }

    #[test]
    fn notification_text_falls_back_to_plain_text_when_no_address() {
        let s = summary("Hello", None, None, "Hi there");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(!text.contains("mailto:"));
    }

    #[test]
    fn notification_text_renders_preview_as_a_blockquote() {
        let s = summary("Hello", None, Some("a@b.c"), "line one\nline two");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains(">line one"));
        assert!(text.contains(">line two"));
    }

    #[test]
    fn notification_text_lists_attachment_names_and_human_readable_sizes() {
        let mut s = summary("Hello", None, Some("a@b.c"), "Hi");
        s.attachments = vec![
            ("rapport.pdf".to_string(), 250_000),
            ("photo.jpg".to_string(), 1_500_000),
        ];
        let text = notification_text(&s, Tz::UTC, None);
        assert!(text.contains("📎"));
        // '.' and the parens are all MarkdownV2-reserved, so they're
        // expected escaped in the rendered text.
        assert!(text.contains(r"rapport\.pdf \(244 Ko\)"));
        assert!(text.contains(r"photo\.jpg \(1\.4 Mo\)"));
    }

    #[test]
    fn notification_text_omits_attachments_line_when_none() {
        let s = summary("Hello", None, Some("a@b.c"), "Hi");
        let text = notification_text(&s, Tz::UTC, None);
        assert!(!text.contains("📎"));
    }

    #[test]
    fn notification_text_caps_attachment_list_and_escapes_hostile_names() {
        let mut s = summary("Hello", None, Some("a@b.c"), "Hi");
        s.attachments = (0..8).map(|i| (format!("*evil{i}*.txt"), 100)).collect();
        let text = notification_text(&s, Tz::UTC, None);
        // Only the first 5 are listed, with the rest summarized.
        assert!(text.contains(r"\(\+3 autres\)"));
        // A malicious attachment name must not inject Markdown formatting.
        assert!(text.contains(r"\*evil0\*\.txt"));
    }

    #[test]
    fn notification_text_includes_mailbox_line_for_shared_account() {
        let s = summary("Hello", Some("Alice"), Some("alice@example.org"), "Hi");
        let text = notification_text(&s, Tz::UTC, Some("contact@delta-net.ovh"));
        assert!(text.contains(r"contact@delta\-net\.ovh"));
        // The common (no shared account) case must stay unaffected.
        let plain = notification_text(&s, Tz::UTC, None);
        assert!(!plain.contains("Boîte"));
    }

    #[test]
    fn notification_keyboard_encodes_account_id_for_shared_mailbox_actions() {
        // Email ids are only unique within their JMAP account, so a shared
        // mailbox's action buttons must carry the account id, not just the
        // email id, or a tap would be routed to the wrong client.
        let kb = notification_keyboard("M123", Some("acc7"), false);
        let InlineKeyboardButton {
            kind: teloxide::types::InlineKeyboardButtonKind::CallbackData(data),
            ..
        } = &kb.inline_keyboard[0][1]
        else {
            panic!("expected a callback button");
        };
        assert_eq!(data, "r:acc7:M123");
    }

    #[test]
    fn notification_keyboard_drops_buttons_when_account_and_id_together_too_long() {
        let long_account = "a".repeat(60);
        let kb = notification_keyboard("M123", Some(&long_account), false);
        assert!(kb.inline_keyboard.is_empty());
    }

    #[test]
    fn undo_keyboard_has_a_single_cancel_button() {
        let kb = undo_keyboard("M123", None);
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 1);
        let InlineKeyboardButton {
            kind: teloxide::types::InlineKeyboardButtonKind::CallbackData(data),
            ..
        } = &kb.inline_keyboard[0][0]
        else {
            panic!("expected a callback button");
        };
        assert_eq!(data, "u:M123");
    }

    #[test]
    fn shared_accounts_keyboard_marks_enabled_accounts() {
        let accounts = vec![
            ("acc7".to_string(), "contact@delta-net.ovh".to_string()),
            ("acc8".to_string(), "contact@cardinalcodes.com".to_string()),
        ];
        let mut enabled = std::collections::HashSet::new();
        enabled.insert("acc7".to_string());

        let kb = shared_accounts_keyboard(&accounts, &enabled);
        assert_eq!(kb.inline_keyboard.len(), 2);

        let button_text = |row: usize| kb.inline_keyboard[row][0].text.clone();
        assert!(button_text(0).starts_with("✅"));
        assert!(button_text(1).starts_with("⬜"));

        let InlineKeyboardButton {
            kind: teloxide::types::InlineKeyboardButtonKind::CallbackData(data),
            ..
        } = &kb.inline_keyboard[0][0]
        else {
            panic!("expected a callback button");
        };
        assert_eq!(data, "s:acc7");
    }

    #[test]
    fn shared_accounts_keyboard_omits_accounts_with_ids_too_long_for_telegram() {
        let accounts = vec![("x".repeat(100), "unreachable".to_string())];
        let kb = shared_accounts_keyboard(&accounts, &std::collections::HashSet::new());
        assert!(kb.inline_keyboard.is_empty());
    }

    #[test]
    fn chunk_text_splits_on_line_boundaries_under_limit() {
        let text = "line one\nline two\nline three";
        let chunks = chunk_text(text, 15);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 15, "chunk too long: {chunk:?}");
        }
        assert_eq!(chunks.join("\n"), text);
    }

    #[test]
    fn chunk_text_splits_a_single_line_longer_than_limit() {
        let text = "a".repeat(50);
        let chunks = chunk_text(&text, 10);
        assert_eq!(chunks.len(), 5);
        for chunk in &chunks {
            assert_eq!(chunk.chars().count(), 10);
        }
    }

    #[test]
    fn chunk_text_handles_empty_input() {
        let chunks = chunk_text("", 100);
        assert_eq!(chunks, vec!["(message vide)".to_string()]);
    }
}
