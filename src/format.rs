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

pub fn notification_text(summary: &EmailSummary) -> String {
    let from = match (&summary.from_name, &summary.from_addr) {
        (Some(name), Some(addr)) if !name.is_empty() => format!(
            "{} <{}>",
            truncate(name, FIELD_LIMIT),
            truncate(addr, FIELD_LIMIT)
        ),
        (_, Some(addr)) => truncate(addr, FIELD_LIMIT),
        _ => "(expéditeur inconnu)".to_string(),
    };

    let when = summary
        .received_at
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.format("%d/%m/%Y %H:%M UTC").to_string())
        .unwrap_or_default();

    let subject = truncate(summary.subject.trim(), FIELD_LIMIT);
    let preview = truncate(summary.preview.trim(), PREVIEW_LIMIT);

    format!(
        "📧 *Nouveau message*\n\n*De :* {from}\n*Objet :* {subject}\n*Reçu :* {when}\n\n{preview}",
        from = escape_markdown(&from),
        subject = escape_markdown(&subject),
        when = escape_markdown(&when),
        preview = escape_markdown(&preview),
    )
}

/// Telegram rejects any inline button whose `callback_data` exceeds 64
/// bytes. JMAP does not bound the length of an email id, so a server
/// returning long opaque ids would otherwise produce a silently-broken
/// button (the message sends, the tap does nothing). Rather than fail
/// open, drop the action buttons entirely for that message when the id
/// doesn't fit; the notification text itself is unaffected.
const TELEGRAM_CALLBACK_DATA_MAX_LEN: usize = 64;

pub fn notification_keyboard(email_id: &str) -> InlineKeyboardMarkup {
    let longest_prefix = "d:"; // all action prefixes are 2 bytes, so any is representative
    if longest_prefix.len() + email_id.len() > TELEGRAM_CALLBACK_DATA_MAX_LEN {
        return InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new());
    }

    InlineKeyboardMarkup::new([
        [
            InlineKeyboardButton::callback("📖 Lire tout", format!("f:{email_id}")),
            InlineKeyboardButton::callback("✅ Lu", format!("r:{email_id}")),
        ],
        [
            InlineKeyboardButton::callback("📥 Archiver", format!("a:{email_id}")),
            InlineKeyboardButton::callback("🗑 Supprimer", format!("d:{email_id}")),
        ],
    ])
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
        let text = notification_text(&s);
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
        let text = notification_text(&s);
        assert!(text.contains("expéditeur inconnu"));
    }

    #[test]
    fn notification_text_uses_address_when_name_missing() {
        let s = summary("Hello", None, Some("bob@example.org"), "Hi");
        let text = notification_text(&s);
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
        let text = notification_text(&s);
        assert!(text.contains(r"\*bold\* \_italic\_ \[link\]\(evil\) \`code\`"));
    }

    #[test]
    fn notification_text_truncates_long_preview() {
        let long_preview = "a".repeat(1000);
        let s = summary("Subject", None, Some("a@b.c"), &long_preview);
        let text = notification_text(&s);
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
        let text = notification_text(&s);
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
        assert!(text.contains('…'));
    }

    #[test]
    fn notification_text_truncates_malicious_long_sender_name() {
        let long_name = "n".repeat(5000);
        let s = summary("Subject", Some(&long_name), Some("a@b.c"), "preview");
        let text = notification_text(&s);
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
    }

    #[test]
    fn notification_text_truncates_malicious_long_sender_address() {
        // The sender name isn't the only unbounded field JMAP hands us —
        // a crafted long From address must be capped too, whether or not
        // a display name is also present.
        let long_addr = format!("{}@example.org", "a".repeat(5000));
        let s = summary("Subject", None, Some(&long_addr), "preview");
        let text = notification_text(&s);
        assert!(text.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);

        let s_with_name = summary("Subject", Some("Alice"), Some(&long_addr), "preview");
        let text_with_name = notification_text(&s_with_name);
        assert!(text_with_name.chars().count() < TELEGRAM_MAX_MESSAGE_LEN);
    }

    #[test]
    fn notification_keyboard_has_action_buttons_for_short_id() {
        let kb = notification_keyboard("M123");
        assert_eq!(kb.inline_keyboard.len(), 2);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
    }

    #[test]
    fn notification_keyboard_drops_buttons_when_id_too_long_for_telegram() {
        // Telegram's callback_data hard limit is 64 bytes; a pathological
        // JMAP id must not produce a button Telegram will silently reject.
        let long_id = "x".repeat(100);
        let kb = notification_keyboard(&long_id);
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
