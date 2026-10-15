//! Language resolution and localized command metadata.
//!
//! Two locales are shipped, `en` and `fr` (see `locales/`). Every
//! user-facing string is looked up through `rust_i18n`'s `t!` macro with an
//! *explicit* `locale = …` rather than a process-global locale: the bot is
//! multi-tenant (one chat per user, several mutually-untrusted chats in
//! `AUTHORIZED_CHAT_IDS`), so a global `set_locale` would be wrong the
//! moment two chats with different languages are served concurrently.

use teloxide::types::BotCommand;

/// The locales this bot ships, matched best-effort from a Telegram
/// `language_code` or a stored preference.
const SUPPORTED: [&str; 2] = ["en", "fr"];

/// Normalizes an exact locale string (a `/lang` argument, a stored
/// preference, or `BOT_LANGUAGE`) to one of ours, case-insensitively.
pub fn canonical(locale: &str) -> Option<&'static str> {
    SUPPORTED
        .iter()
        .copied()
        .find(|supported| supported.eq_ignore_ascii_case(locale))
}

/// Maps a Telegram `language_code` (e.g. `fr`, `fr-FR`, `en-US`) to one of
/// our locales by its primary subtag. Returns `None` for a language we
/// don't ship (e.g. `de`) so the operator's `BOT_LANGUAGE` still applies,
/// rather than forcing English on every non-French user.
fn detect(code: &str) -> Option<&'static str> {
    let primary = code.split(['-', '_']).next().unwrap_or("");
    canonical(primary)
}

/// A user's effective locale, in decreasing priority:
///
/// 1. the stored preference (set by `/lang`, or seeded from the detected
///    language at login);
/// 2. the current message's Telegram `language_code`;
/// 3. the operator's `BOT_LANGUAGE`;
/// 4. `en`.
///
/// Never fails: an unknown value at any level just falls through to the
/// next one.
pub fn resolve(stored: Option<&str>, detected: Option<&str>, default: &str) -> &'static str {
    stored
        .and_then(canonical)
        .or_else(|| detected.and_then(detect))
        .or_else(|| canonical(default))
        .unwrap_or("en")
}

/// The `/help` body: every command with its language-appropriate
/// description, in the same order Telegram's `/` menu shows them.
pub fn help_text(lang: &str) -> String {
    bot_commands(lang)
        .iter()
        .map(|c| format!("/{} - {}", c.command, c.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The command list registered with Telegram via `set_my_commands`, in the
/// requested language. Kept in lockstep with `bot::Command` by the
/// `every_advertised_english_command_name_parses_to_a_variant` test.
pub fn bot_commands(lang: &str) -> Vec<BotCommand> {
    vec![
        BotCommand::new("start", t!("cmd.start", locale = lang)),
        BotCommand::new("login", t!("cmd.login", locale = lang)),
        BotCommand::new("logout", t!("cmd.logout", locale = lang)),
        BotCommand::new("status", t!("cmd.status", locale = lang)),
        BotCommand::new("shared", t!("cmd.shared", locale = lang)),
        BotCommand::new("accounts", t!("cmd.accounts", locale = lang)),
        BotCommand::new("mute", t!("cmd.mute", locale = lang)),
        BotCommand::new("unmute", t!("cmd.unmute", locale = lang)),
        BotCommand::new("search", t!("cmd.search", locale = lang)),
        BotCommand::new("lang", t!("cmd.lang", locale = lang)),
        BotCommand::new("help", t!("cmd.help", locale = lang)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_preference_wins_over_detection_and_default() {
        assert_eq!(resolve(Some("fr"), Some("en-US"), "en"), "fr");
        assert_eq!(resolve(Some("EN"), None, "fr"), "en");
    }

    #[test]
    fn detection_wins_over_default_but_only_for_shipped_locales() {
        assert_eq!(resolve(None, Some("fr-FR"), "en"), "fr");
        assert_eq!(resolve(None, Some("en-GB"), "fr"), "en");
        // A language we don't ship must not shadow BOT_LANGUAGE.
        assert_eq!(resolve(None, Some("de-DE"), "fr"), "fr");
    }

    #[test]
    fn default_and_hard_fallback_apply_last() {
        assert_eq!(resolve(None, None, "fr"), "fr");
        // An unsupported BOT_LANGUAGE falls all the way back to English.
        assert_eq!(resolve(None, None, "xx"), "en");
        assert_eq!(resolve(Some("nope"), None, "nope"), "en");
    }

    #[test]
    fn help_text_lists_every_registered_command_in_both_locales() {
        for lang in SUPPORTED {
            let help = help_text(lang);
            for command in bot_commands(lang) {
                assert!(
                    help.contains(&format!("/{} - {}", command.command, command.description)),
                    "missing /{} in {lang} help: {help}",
                    command.command
                );
            }
        }
        // Descriptions are actually translated, not the English fallback.
        assert_ne!(
            bot_commands("fr")[0].description,
            bot_commands("en")[0].description
        );
    }
}
