use chrono_tz::Tz;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::jmap::EmailSummary;

/// Telegram's hard limit for a single message's text, counted in UTF-16
/// code units (the Bot API's unit for message length), not in Rust chars
/// - see `chunk_text` for why the distinction matters.
pub const TELEGRAM_MAX_MESSAGE_LEN: usize = 4096;

/// Every field below (subject, sender name, preview) comes straight from
/// whatever mail showed up in the mailbox, so all of them are bounded
/// before being sent: an unbounded field could push the rendered message
/// past Telegram's 4096-char limit, and `send_message` would then fail
/// for that notification alone - silently and permanently losing it,
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
/// case). `lang` is the recipient chat's resolved locale.
pub fn notification_text(
    summary: &EmailSummary,
    tz: Tz,
    account_label: Option<&str>,
    lang: &str,
) -> String {
    let from = from_field(
        summary.from_name.as_deref(),
        summary.from_addr.as_deref(),
        lang,
    );

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
    let subject = if subject.is_empty() {
        // An empty subject is stored as-is (not defaulted at fetch time),
        // so this placeholder is resolved in the reader's own language.
        t!("notif.no_subject", locale = lang).to_string()
    } else {
        subject
    };
    let preview = blockquote(&escape_markdown(&truncate(
        summary.preview.trim(),
        PREVIEW_LIMIT,
    )));

    let mailbox_line = account_label
        .map(|label| {
            format!(
                "{}\n",
                t!(
                    "notif.mailbox",
                    locale = lang,
                    label = escape_markdown(&truncate(label, FIELD_LIMIT))
                )
            )
        })
        .unwrap_or_default();

    let attachments_line = attachments_field(&summary.attachments, lang);

    format!(
        "{title}\n\n{mailbox_line}{from_line}\n{subject_line}\n{received_line}\n{attachments_line}\n{preview}",
        title = t!("notif.title", locale = lang),
        from_line = t!("notif.from", locale = lang, value = from),
        subject_line = t!(
            "notif.subject",
            locale = lang,
            value = escape_markdown(&subject)
        ),
        received_line = t!(
            "notif.received",
            locale = lang,
            value = escape_markdown(&when)
        ),
    )
}

/// Attachments are never downloaded (this bot fetches message bodies on
/// demand only, never files), but naming them means "there's a 4 MB PDF
/// here" isn't silently invisible - just capped at a handful of names so a
/// message with hundreds of parts can't blow out the notification.
fn attachments_field(attachments: &[(String, usize)], lang: &str) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    const MAX_SHOWN: usize = 5;

    let shown = attachments
        .iter()
        .take(MAX_SHOWN)
        .map(|(name, size)| {
            let name = if name.trim().is_empty() {
                t!("notif.empty_attachment_name", locale = lang).to_string()
            } else {
                truncate(name, FIELD_LIMIT)
            };
            // The parens here are literal text, not link syntax, so they
            // need the same MarkdownV2 escaping as any other reserved
            // character - an unescaped '(' or ')' would otherwise make
            // Telegram reject the whole message with a 400.
            format!(
                "{} \\({}\\)",
                escape_markdown(&name),
                // human_size can render a decimal point ("1.4 Mo"), itself
                // a reserved MarkdownV2 character.
                escape_markdown(&human_size(*size, lang))
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    let extra = attachments.len().saturating_sub(MAX_SHOWN);
    let suffix = if extra > 0 {
        // '+' and the parens are MarkdownV2-reserved even as plain,
        // bot-generated text - same class of bug as the per-attachment
        // parens above.
        format!(
            " {}",
            escape_markdown(&t!("notif.attachments_more", locale = lang, count = extra))
        )
    } else {
        String::new()
    };

    format!(
        "{}\n",
        t!(
            "notif.attachments",
            locale = lang,
            value = format!("{shown}{suffix}")
        )
    )
}

fn human_size(bytes: usize, lang: &str) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        t!(
            "notif.unit_mb",
            locale = lang,
            value = format!("{:.1}", bytes / MB)
        )
        .to_string()
    } else if bytes >= KB {
        t!(
            "notif.unit_kb",
            locale = lang,
            value = format!("{:.0}", bytes / KB)
        )
        .to_string()
    } else {
        t!(
            "notif.unit_b",
            locale = lang,
            value = (bytes as u64).to_string()
        )
        .to_string()
    }
}

/// Renders the sender as a `mailto:` link when an address is available, so
/// tapping it opens the recipient's mail app - a small but genuinely useful
/// bit of native richness GmailBot doesn't offer. Falls back to plain
/// escaped text when there's nothing to link (no address, or an address
/// that's entirely whitespace/control characters once sanitized).
fn from_field(name: Option<&str>, addr: Option<&str>, lang: &str) -> String {
    let addr = addr.map(|a| truncate(a, FIELD_LIMIT));
    let display = match (name, &addr) {
        (Some(name), Some(addr)) if !name.is_empty() => {
            format!("{} <{}>", truncate(name, FIELD_LIMIT), addr)
        }
        (_, Some(addr)) => addr.clone(),
        _ => return escape_markdown(&t!("notif.unknown_sender", locale = lang)),
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

/// Which JMAP account a notification's action buttons act on, so the
/// callback handler can route a tap to the right `Client` (email ids are
/// only unique within one account).
pub type AccountRef<'a> = Option<&'a str>;

/// Builds a notification's action keyboard.
///
/// Two states, so the common case stays uncluttered while every action
/// remains one tap away:
///
/// - **collapsed** (the default a notification arrives in):
///   `[ 📥 Archiver ] [ 🗑 Supprimer ] [ ⋯ Plus ]`
/// - **expanded** (after tapping `⋯ Plus`):
///   `[ 📖 Lire tout ] [ ✅ Lu ]` / `[ 📥 Archiver ] [ 🚫 Spam ]` /
///   `[ 🗑 Supprimer ] [ ⌃ Moins ]`
///
/// The expand/collapse toggle lives in `callback_data` as one of `e`/`E`
/// (expand) or `c`/`C` (collapse); the case carries the message's read
/// state, since the collapsed view has no room for the read button and
/// the expanded one needs to know whether to render an actionable `✅ Lu`
/// (`r`) or the inert `✔️ Lu` (`n`). `already_read` therefore swaps that
/// button's label and action, and `expanded` picks which of the two
/// layouts to render - both are pure presentation, so tapping the toggle
/// never needs a JMAP round-trip.
pub fn notification_keyboard(
    email_id: &str,
    account_id: AccountRef<'_>,
    already_read: bool,
    expanded: bool,
    lang: &str,
) -> InlineKeyboardMarkup {
    // All action letters are 1 byte, so any is representative for the
    // length check.
    if action_callback_data('d', account_id, email_id).len() > TELEGRAM_CALLBACK_DATA_MAX_LEN {
        return InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new());
    }

    let btn = |label: String, action: char| {
        InlineKeyboardButton::callback(label, action_callback_data(action, account_id, email_id))
    };

    if expanded {
        let read_button = if already_read {
            btn(t!("button.read_done", locale = lang).to_string(), 'n')
        } else {
            btn(t!("button.read", locale = lang).to_string(), 'r')
        };
        // Case carries read state across the toggle, so expanding again
        // later still renders the right read button.
        let collapse = if already_read { 'C' } else { 'c' };
        InlineKeyboardMarkup::new(vec![
            vec![
                btn(t!("button.read_all", locale = lang).to_string(), 'f'),
                read_button,
            ],
            vec![
                btn(t!("button.archive", locale = lang).to_string(), 'a'),
                btn(t!("button.spam", locale = lang).to_string(), 'j'),
            ],
            vec![
                btn(t!("button.delete", locale = lang).to_string(), 'd'),
                btn(t!("button.less", locale = lang).to_string(), collapse),
            ],
        ])
    } else {
        let expand = if already_read { 'E' } else { 'e' };
        InlineKeyboardMarkup::new(vec![vec![
            btn(t!("button.archive", locale = lang).to_string(), 'a'),
            btn(t!("button.delete", locale = lang).to_string(), 'd'),
            btn(t!("button.more", locale = lang).to_string(), expand),
        ]])
    }
}

/// Replaces a triage action's cleared keyboard with a single "↩️ Annuler"
/// button, so archiving/marking spam/deleting is a one-tap mistake to
/// recover from instead of a silent, final action.
pub fn undo_keyboard(email_id: &str, account_id: Option<&str>, lang: &str) -> InlineKeyboardMarkup {
    if action_callback_data('u', account_id, email_id).len() > TELEGRAM_CALLBACK_DATA_MAX_LEN {
        return InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new());
    }
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        t!("button.undo", locale = lang).to_string(),
        action_callback_data('u', account_id, email_id),
    )]])
}

/// Builds the `/partages` toggle menu: one button per shared JMAP account
/// visible to the token right now, rung (🔔) when the chat is currently
/// opted into notifications for it and silenced (🔕) otherwise. Bells
/// rather than checkboxes because the menu's own text is a reminder that a
/// tap toggles notifications, not a multi-select form. An account whose id
/// is too long to fit Telegram's callback_data limit is left out rather
/// than shown as a button that can never be tapped successfully.
pub fn shared_accounts_keyboard(
    accounts: &[(String, String)],
    enabled: &std::collections::HashSet<String>,
) -> InlineKeyboardMarkup {
    let rows: Vec<Vec<InlineKeyboardButton>> = accounts
        .iter()
        .filter(|(id, _)| format!("s:{id}").len() <= TELEGRAM_CALLBACK_DATA_MAX_LEN)
        .map(|(id, name)| {
            let mark = if enabled.contains(id) { "🔔" } else { "🔕" };
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
///
/// Telegram measures message length in UTF-16 code units, not in Rust
/// chars (Unicode scalar values): an emoji outside the BMP (😀, U+1F600)
/// costs one `char` but two UTF-16 units, so budgeting in `chars()`
/// would let an emoji-heavy body produce chunks Telegram rejects as
/// "message is too long" despite the char count saying they fit. Every
/// budget below is therefore maintained in the same unit Telegram
/// counts in.
pub fn chunk_text(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() {
        return vec!["(message vide)".to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for line in text.lines() {
        let line_len = line.encode_utf16().count();
        // +1 is the '\n' that would join `line` onto `current`.
        if current_len + line_len + 1 > max_len && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if line_len > max_len {
            // A single line longer than a whole message: split it at
            // char boundaries, never mid-surrogate-pair - accumulating
            // one char at a time guarantees that, and a lone half-pair
            // would render as U+FFFD even if Telegram accepted it.
            for c in line.chars() {
                let units = c.len_utf16();
                if current_len + units > max_len {
                    chunks.push(std::mem::take(&mut current));
                    current_len = 0;
                }
                current.push(c);
                current_len += units;
            }
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
            current_len += 1;
        }
        current.push_str(line);
        current_len += line_len;
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
        let text = notification_text(&s, Tz::UTC, None, "fr");
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
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.contains("expéditeur inconnu"));
    }

    #[test]
    fn notification_text_uses_address_when_name_missing() {
        let s = summary("Hello", None, Some("bob@example.org"), "Hi");
        let text = notification_text(&s, Tz::UTC, None, "fr");
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
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.contains(r"\*bold\* \_italic\_ \[link\]\(evil\) \`code\`"));
    }

    #[test]
    fn notification_text_truncates_long_preview() {
        let long_preview = "a".repeat(1000);
        let s = summary("Subject", None, Some("a@b.c"), &long_preview);
        let text = notification_text(&s, Tz::UTC, None, "fr");
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
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
        assert!(text.contains('…'));
    }

    #[test]
    fn notification_text_truncates_malicious_long_sender_name() {
        let long_name = "n".repeat(5000);
        let s = summary("Subject", Some(&long_name), Some("a@b.c"), "preview");
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
    }

    #[test]
    fn notification_text_truncates_malicious_long_sender_address() {
        // The sender name isn't the only unbounded field JMAP hands us -
        // a crafted long From address must be capped too, whether or not
        // a display name is also present.
        let long_addr = format!("{}@example.org", "a".repeat(5000));
        let s = summary("Subject", None, Some(&long_addr), "preview");
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);

        let s_with_name = summary("Subject", Some("Alice"), Some(&long_addr), "preview");
        let text_with_name = notification_text(&s_with_name, Tz::UTC, None, "fr");
        assert!(text_with_name.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
    }

    #[test]
    fn notification_text_renders_received_at_in_the_configured_timezone() {
        // 1_700_000_000 is 2023-11-14T22:13:20Z; Europe/Paris was on
        // CET (UTC+1) at that instant, so the rendered hour must differ
        // from the UTC rendering rather than silently always being UTC.
        let s = summary("Subject", None, Some("a@b.c"), "preview");
        let utc_text = notification_text(&s, Tz::UTC, None, "fr");
        let paris_text = notification_text(&s, chrono_tz::Europe::Paris, None, "fr");
        assert!(utc_text.contains("22:13"));
        assert!(paris_text.contains("23:13"));
        assert_ne!(utc_text, paris_text);
    }

    /// Extracts a callback button's `callback_data`, or fails loudly if the
    /// button isn't a callback one.
    fn callback_data(button: &InlineKeyboardButton) -> &str {
        match &button.kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(data) => data,
            _ => panic!("expected a callback button"),
        }
    }

    #[test]
    fn notification_keyboard_collapses_to_one_row_of_three() {
        let kb = notification_keyboard("M123", None, false, false, "fr");
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 3);
        let labels: Vec<&str> = kb.inline_keyboard[0]
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(labels, vec!["📥 Archiver", "🗑 Supprimer", "⋯ Plus"]);
    }

    #[test]
    fn notification_keyboard_expands_to_the_full_action_set() {
        let kb = notification_keyboard("M123", None, false, true, "fr");
        assert_eq!(kb.inline_keyboard.len(), 3);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
        assert_eq!(kb.inline_keyboard[1].len(), 2);
        assert_eq!(kb.inline_keyboard[2].len(), 2);
        let row1: Vec<&str> = kb.inline_keyboard[0]
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(row1, vec!["📖 Lire tout", "✅ Lu"]);
        let row3: Vec<&str> = kb.inline_keyboard[2]
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(row3, vec!["🗑 Supprimer", "⌃ Moins"]);
    }

    #[test]
    fn button_labels_follow_the_requested_locale() {
        let en = notification_keyboard("M123", None, false, false, "en");
        let labels: Vec<&str> = en.inline_keyboard[0]
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(labels, vec!["📥 Archive", "🗑 Delete", "⋯ More"]);

        let en_undo = undo_keyboard("M123", None, "en");
        assert_eq!(en_undo.inline_keyboard[0][0].text, "↩️ Undo");
        let fr_undo = undo_keyboard("M123", None, "fr");
        assert_eq!(fr_undo.inline_keyboard[0][0].text, "↩️ Annuler");

        // The action letters (callback_data) are locale-independent: a
        // tap means the same thing whatever language the button is
        // rendered in (and even if the reader switches language between
        // the notification and the tap).
        let fr = notification_keyboard("M123", None, false, false, "fr");
        assert_eq!(
            callback_data(&fr.inline_keyboard[0][2]),
            callback_data(&en.inline_keyboard[0][2])
        );
    }

    #[test]
    fn notification_keyboard_toggle_callback_carries_read_state() {
        // Expanding a still-unread message ("⋯ Plus") sends `e`, and `E`
        // once the message is known to be read - the case is the only
        // place the read state survives while collapsed.
        let unread = notification_keyboard("M123", None, false, false, "fr");
        assert_eq!(callback_data(&unread.inline_keyboard[0][2]), "e:M123");
        let read = notification_keyboard("M123", None, true, false, "fr");
        assert_eq!(callback_data(&read.inline_keyboard[0][2]), "E:M123");

        // Collapsing sends the matching lower/upper `c`/`C`.
        let unread_expanded = notification_keyboard("M123", None, false, true, "fr");
        assert_eq!(
            callback_data(&unread_expanded.inline_keyboard[2][1]),
            "c:M123"
        );
        let read_expanded = notification_keyboard("M123", None, true, true, "fr");
        assert_eq!(
            callback_data(&read_expanded.inline_keyboard[2][1]),
            "C:M123"
        );
    }

    #[test]
    fn notification_keyboard_spam_button_uses_junk_action() {
        let kb = notification_keyboard("M123", None, false, true, "fr");
        assert_eq!(callback_data(&kb.inline_keyboard[1][1]), "j:M123");
    }

    #[test]
    fn notification_keyboard_already_read_shows_inert_check() {
        let unread = notification_keyboard("M123", None, false, true, "fr");
        let read = notification_keyboard("M123", None, true, true, "fr");

        assert_eq!(unread.inline_keyboard[0][1].text, "✅ Lu");
        assert_eq!(callback_data(&unread.inline_keyboard[0][1]), "r:M123");

        assert_eq!(read.inline_keyboard[0][1].text, "✔️ Lu");
        assert_eq!(callback_data(&read.inline_keyboard[0][1]), "n:M123");
    }

    #[test]
    fn notification_keyboard_drops_buttons_when_id_too_long_for_telegram() {
        // Telegram's callback_data hard limit is 64 bytes; a pathological
        // JMAP id must not produce a button Telegram will silently reject.
        let long_id = "x".repeat(100);
        let kb = notification_keyboard(&long_id, None, false, false, "fr");
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
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.contains("(mailto:alice@example.org)"));
    }

    #[test]
    fn notification_text_mailto_link_survives_a_hostile_address() {
        // A From header is attacker-controlled; ')' and '\' in the address
        // must not be able to break out of the MarkdownV2 link's URL part.
        let hostile_addr = "a)\\evil@example.org";
        let s = summary("Hello", None, Some(hostile_addr), "Hi");
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.contains(r"(mailto:a\)\\evil@example.org)"));
    }

    #[test]
    fn notification_text_falls_back_to_plain_text_when_no_address() {
        let s = summary("Hello", None, None, "Hi there");
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(!text.contains("mailto:"));
    }

    #[test]
    fn notification_text_renders_preview_as_a_blockquote() {
        let s = summary("Hello", None, Some("a@b.c"), "line one\nline two");
        let text = notification_text(&s, Tz::UTC, None, "fr");
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
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(text.contains("📎"));
        // '.' and the parens are all MarkdownV2-reserved, so they're
        // expected escaped in the rendered text.
        assert!(text.contains(r"rapport\.pdf \(244 Ko\)"));
        assert!(text.contains(r"photo\.jpg \(1\.4 Mo\)"));
    }

    #[test]
    fn notification_text_omits_attachments_line_when_none() {
        let s = summary("Hello", None, Some("a@b.c"), "Hi");
        let text = notification_text(&s, Tz::UTC, None, "fr");
        assert!(!text.contains("📎"));
    }

    #[test]
    fn notification_text_caps_attachment_list_and_escapes_hostile_names() {
        let mut s = summary("Hello", None, Some("a@b.c"), "Hi");
        s.attachments = (0..8).map(|i| (format!("*evil{i}*.txt"), 100)).collect();
        let text = notification_text(&s, Tz::UTC, None, "fr");
        // Only the first 5 are listed, with the rest summarized.
        assert!(text.contains(r"\(\+3 autres\)"));
        // A malicious attachment name must not inject Markdown formatting.
        assert!(text.contains(r"\*evil0\*\.txt"));
    }

    #[test]
    fn notification_text_includes_mailbox_line_for_shared_account() {
        let s = summary("Hello", Some("Alice"), Some("alice@example.org"), "Hi");
        let text = notification_text(&s, Tz::UTC, Some("contact@delta-net.ovh"), "fr");
        assert!(text.contains(r"contact@delta\-net\.ovh"));
        // The common (no shared account) case must stay unaffected.
        let plain = notification_text(&s, Tz::UTC, None, "fr");
        assert!(!plain.contains("Boîte"));
    }

    #[test]
    fn notification_text_renders_in_english_when_asked() {
        // Same message, English renderer: the header, the sender fallback,
        // the empty-subject placeholder and the size unit all switch, while
        // nothing French leaks through.
        let mut s = summary("", None, None, "");
        s.attachments = vec![("report.pdf".to_string(), 1_500_000)];
        let text = notification_text(&s, Tz::UTC, None, "en");
        assert!(text.contains("*New message*"));
        assert!(text.contains(r"\(unknown sender\)"));
        assert!(text.contains(r"\(no subject\)"));
        assert!(text.contains(r"report\.pdf \(1\.4 MB\)"));
        assert!(!text.contains("expéditeur inconnu"));
        assert!(!text.contains("Mo"));
    }

    #[test]
    fn notification_keyboard_encodes_account_id_for_shared_mailbox_actions() {
        // Email ids are only unique within their JMAP account, so a shared
        // mailbox's action buttons must carry the account id, not just the
        // email id, or a tap would be routed to the wrong client.
        let kb = notification_keyboard("M123", Some("acc7"), false, true, "fr");
        assert_eq!(callback_data(&kb.inline_keyboard[0][1]), "r:acc7:M123");
    }

    #[test]
    fn notification_keyboard_drops_buttons_when_account_and_id_together_too_long() {
        let long_account = "a".repeat(60);
        let kb = notification_keyboard("M123", Some(&long_account), false, false, "fr");
        assert!(kb.inline_keyboard.is_empty());
    }

    #[test]
    fn undo_keyboard_has_a_single_cancel_button() {
        let kb = undo_keyboard("M123", None, "fr");
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
    fn shared_accounts_keyboard_rings_enabled_accounts() {
        let accounts = vec![
            ("acc7".to_string(), "contact@delta-net.ovh".to_string()),
            ("acc8".to_string(), "contact@cardinalcodes.com".to_string()),
        ];
        let mut enabled = std::collections::HashSet::new();
        enabled.insert("acc7".to_string());

        let kb = shared_accounts_keyboard(&accounts, &enabled);
        assert_eq!(kb.inline_keyboard.len(), 2);

        let button_text = |row: usize| kb.inline_keyboard[row][0].text.clone();
        assert!(button_text(0).starts_with("🔔"));
        assert!(button_text(1).starts_with("🔕"));

        assert_eq!(callback_data(&kb.inline_keyboard[0][0]), "s:acc7");
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

    #[test]
    fn chunk_text_budgets_in_utf16_units_for_astral_emoji() {
        // Telegram measures message length in UTF-16 code units, where
        // an astral emoji costs two - a body that fits "4096 chars" can
        // still be twice the real budget. This one is 10 000 chars but
        // 20 000 UTF-16 units, so under the old char-based chunker it
        // would have produced five "fits in 4096" chunks that Telegram
        // rejected wholesale.
        let emoji = "😀".repeat(10_000);
        // 4080 is what bot.rs passes (4096 minus headroom); the budget
        // must hold in the unit Telegram counts in.
        let max = 4080;
        let chunks = chunk_text(&emoji, max);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                chunk.encode_utf16().count() <= max,
                "chunk of {} UTF-16 units exceeds the {max}-unit budget",
                chunk.encode_utf16().count()
            );
        }
        // Astral code points must never be split mid-pair: a lone
        // surrogate half would render as U+FFFD even if Telegram
        // accepted the message. Reassembling the chunks without any
        // separator must give the body back exactly.
        assert_eq!(chunks.concat(), emoji);
    }

    #[test]
    fn chunk_text_with_emoji_lines_respects_the_utf16_budget() {
        // Lines short enough to join several per chunk, but each still
        // two units per char: the '\n'-joining budget has to account
        // for the doubled line lengths or lines silently overflow past
        // the limit one '\n' at a time.
        let lines: Vec<String> = (0..20).map(|_| "🚀".repeat(30)).collect(); // 60 units each
        let text = lines.join("\n");
        let max = 250;
        let chunks = chunk_text(&text, max);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                chunk.encode_utf16().count() <= max,
                "chunk of {} UTF-16 units exceeds the {max}-unit budget",
                chunk.encode_utf16().count()
            );
        }
        // Line-boundary chunking must never lose or reorder a line:
        // joining the chunks back with '\n' is the original body.
        assert_eq!(chunks.join("\n"), text);
    }

    #[test]
    fn chunk_text_allows_a_chunk_exactly_at_the_limit() {
        // 2040 astral emoji = 4080 UTF-16 units = exactly the budget;
        // the overflow check must be `>` and not `>=`, or the chunker
        // would needlessly split messages Telegram would have accepted.
        let emoji = "😀".repeat(2040);
        let chunks = chunk_text(&emoji, 4080);
        assert_eq!(chunks, vec![emoji]);
    }

    #[test]
    fn truncate_keeps_a_string_of_exactly_the_limit_intact() {
        // The bound is inclusive: a field of exactly `FIELD_LIMIT` chars
        // is representable as-is, only one char more gets an ellipsis.
        let exact = "a".repeat(FIELD_LIMIT);
        assert_eq!(truncate(&exact, FIELD_LIMIT), exact);

        let over = "a".repeat(FIELD_LIMIT + 1);
        let cut = truncate(&over, FIELD_LIMIT);
        assert!(cut.ends_with('…'), "got: {cut:?}");
        assert_eq!(cut.chars().count(), FIELD_LIMIT + 1);
        assert_eq!(cut, format!("{}…", "a".repeat(FIELD_LIMIT)));
    }

    #[test]
    fn attachments_field_unfolds_the_overflow_count_only_past_the_shown_cap() {
        // Five names fill the visible list exactly, so nothing is folded
        // away; the sixth is the first to be summarized.
        let at_cap: Vec<(String, usize)> = (0..5).map(|i| (format!("f{i}.pdf"), 10)).collect();
        let field = attachments_field(&at_cap, "en");
        assert!(
            !field.contains("more"),
            "nothing to fold at the cap: {field}"
        );

        let mut over = at_cap.clone();
        over.push(("sixth.pdf".to_string(), 10));
        let field = attachments_field(&over, "en");
        // The parentheses are MarkdownV2-escaped, so match the inner text.
        assert!(field.contains("+1 more"), "one past the cap: {field}");
        assert!(
            !field.contains("sixth.pdf"),
            "the sixth is only counted: {field}"
        );
    }

    #[test]
    fn from_field_drops_an_empty_display_name_rather_than_rendering_angle_brackets() {
        // A present-but-empty name must not render as "<>" around the
        // address: it is equivalent to having no name at all.
        let with_empty = from_field(Some(""), Some("a@b.c"), "en");
        assert!(!with_empty.contains('<'), "no empty <> pair: {with_empty}");
        assert!(with_empty.contains("a@b\\.c"), "address kept: {with_empty}");
        assert_eq!(with_empty, from_field(None, Some("a@b.c"), "en"));

        // A whitespace-only name is not empty, so it is kept (bounded).
        let with_blank = from_field(Some(" "), Some("a@b.c"), "en");
        assert!(with_blank.contains(' '), "blank name kept: {with_blank}");
    }

    #[test]
    fn mailto_url_strips_both_whitespace_and_non_whitespace_control_characters() {
        // Space, tab and newline are whitespace...
        assert_eq!(mailto_url("a b\tc\nd"), "abcd");
        // ...but U+0007 (bell) is a control char that is *not*
        // whitespace, and must be stripped just the same.
        assert_eq!(mailto_url("a\u{7}b"), "ab");
        assert_eq!(mailto_url("\u{0}\u{1f}"), "");
    }

    #[test]
    fn keyboards_drop_every_button_once_the_callback_data_would_be_one_byte_too_long() {
        // "d:{id}" is action + ':' + id, so the longest id that still
        // fits occupies 64 - 2 bytes; one byte more drops all buttons.
        let at_limit = "x".repeat(TELEGRAM_CALLBACK_DATA_MAX_LEN - 2);
        let over_limit = "x".repeat(TELEGRAM_CALLBACK_DATA_MAX_LEN - 1);

        assert!(
            !notification_keyboard(&at_limit, None, false, false, "en")
                .inline_keyboard
                .is_empty()
        );
        assert!(
            !notification_keyboard(&at_limit, None, false, true, "en")
                .inline_keyboard
                .is_empty()
        );
        assert!(
            notification_keyboard(&over_limit, None, false, false, "en")
                .inline_keyboard
                .is_empty()
        );
        assert!(
            notification_keyboard(&over_limit, None, false, true, "en")
                .inline_keyboard
                .is_empty()
        );

        assert!(
            !undo_keyboard(&at_limit, None, "en")
                .inline_keyboard
                .is_empty()
        );
        assert!(
            undo_keyboard(&over_limit, None, "en")
                .inline_keyboard
                .is_empty()
        );

        // The shared-account form adds ":acc" (four bytes), so the same
        // id no longer fits.
        let shared_at_limit = "x".repeat(TELEGRAM_CALLBACK_DATA_MAX_LEN - 2 - 4);
        assert!(
            !undo_keyboard(&shared_at_limit, Some("acc"), "en")
                .inline_keyboard
                .is_empty()
        );
        assert!(
            undo_keyboard(&at_limit, Some("acc"), "en")
                .inline_keyboard
                .is_empty()
        );
    }

    #[test]
    fn chunk_text_lets_two_lines_share_a_chunk_at_the_exact_fit() {
        // "aa" + '\n' + "bbb" is exactly six UTF-16 units: the join
        // fits, so the two lines stay together.
        assert_eq!(chunk_text("aa\nbbb", 6), vec!["aa\nbbb".to_string()]);
    }

    #[test]
    fn chunk_text_splits_two_lines_when_the_joining_newline_would_overflow() {
        // The same two lines no longer fit at five units: the '\n' that
        // would join them is the sixth.
        assert_eq!(
            chunk_text("aa\nbbb", 5),
            vec!["aa".to_string(), "bbb".to_string()]
        );
    }

    #[test]
    fn chunk_text_hard_splits_a_line_longer_than_the_budget() {
        // One line, ten units, a four-unit budget: it must come out as
        // three chunks of four, four and two.
        assert_eq!(
            chunk_text("aaaaaaaaaa", 4),
            vec!["aaaa".to_string(), "aaaa".to_string(), "aa".to_string()]
        );
    }

    #[test]
    fn chunk_text_counts_each_joining_newline_towards_the_budget() {
        // Three four-unit lines joined by two newlines are fourteen
        // units, so only the first two fit a thirteen-unit budget - the
        // split point depends on each '\n' being counted, not just the
        // line contents.
        let chunks = chunk_text("aaaa\nbbbb\ncccc", 13);
        assert_eq!(chunks, vec!["aaaa\nbbbb".to_string(), "cccc".to_string()]);
        for chunk in &chunks {
            assert!(
                chunk.encode_utf16().count() <= 13,
                "chunk {chunk:?} overflows the budget"
            );
        }
    }
}
