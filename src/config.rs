use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono_tz::Tz;
use teloxide::types::ChatId;

/// Runtime configuration, loaded exclusively from environment variables.
///
/// Only two variables are required: `TELEGRAM_BOT_TOKEN` and
/// `AUTHORIZED_CHAT_IDS`. Everything else (JMAP server + credentials) is
/// provided live, per authorized user, through the Telegram chat itself.
#[derive(Debug)]
pub struct Config {
    pub telegram_token: String,
    pub authorized_chat_ids: HashSet<ChatId>,
    pub data_dir: PathBuf,
    /// Off by default: a `/login`'d server_url that resolves to a
    /// private/loopback/link-local address is refused, since
    /// `AUTHORIZED_CHAT_IDS` can list several mutually-untrusted chats and
    /// nothing should let one of them turn the bot's outbound HTTPS client
    /// (carrying an attacker-chosen bearer token) into an SSRF probe of
    /// the deployment's internal network. Self-hosters running their own
    /// JMAP server on an internal network (e.g. a home LAN) can opt back
    /// in explicitly.
    pub allow_private_jmap_hosts: bool,
    /// IANA timezone (e.g. `Europe/Paris`) used to render timestamps in
    /// notifications and log output. Defaults to UTC. Backed by
    /// `chrono-tz`'s bundled zone database rather than the system's
    /// (the distroless runtime image has none), so this works the same
    /// regardless of deployment target.
    pub timezone: Tz,
    /// Language used when neither a stored preference nor the Telegram
    /// `language_code` resolves to a shipped locale. Backed by
    /// `BOT_LANGUAGE`; must be one of the locales in `locales/` (see
    /// `i18n::canonical`), and an unknown value is rejected at startup
    /// rather than silently falling back, so a typo in a deployment
    /// manifest is loud. Defaults to `en`.
    pub default_language: String,
    /// Telegram threaded mode (topics in private chats).
    /// - `auto`: attempts to use forum topics if enabled in BotFather,
    ///   falling back cleanly to single-stream chat if not supported.
    /// - `enabled`: forces topic creation.
    /// - `disabled`: completely turns off forum topics on this deployment.
    pub telegram_threaded_mode: ThreadedModeConfig,
    /// Topic granularity strategy:
    /// - `account`: one topic per connected account slot (recommended).
    /// - `server`: one topic per JMAP host/server.
    pub telegram_thread_strategy: ThreadStrategyConfig,
    /// How shared/delegated mailboxes are routed into topics:
    /// - `parent`: notifications land in the parent account's topic (default).
    /// - `dedicated`: each shared mailbox receives its own dedicated topic.
    pub telegram_shared_thread_mode: SharedThreadModeConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThreadedModeConfig {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThreadStrategyConfig {
    #[default]
    Account,
    Server,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SharedThreadModeConfig {
    #[default]
    Parent,
    Dedicated,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::parse(
            std::env::var("TELEGRAM_BOT_TOKEN").ok(),
            std::env::var("AUTHORIZED_CHAT_IDS").ok(),
            std::env::var("DATA_DIR").ok(),
            std::env::var("ALLOW_PRIVATE_JMAP_HOSTS").ok(),
            std::env::var("TIMEZONE").ok(),
            std::env::var("BOT_LANGUAGE").ok(),
            std::env::var("TELEGRAM_THREADED_MODE").ok(),
            std::env::var("TELEGRAM_THREAD_STRATEGY").ok(),
            std::env::var("TELEGRAM_SHARED_THREAD_MODE").ok(),
        )
    }

    /// Pure parsing logic, kept separate from `from_env` so it can be
    /// exercised in tests without touching real process-wide environment
    /// variables (which are global mutable state and unsafe to fiddle with
    /// across parallel tests).
    #[allow(clippy::too_many_arguments)]
    fn parse(
        telegram_token: Option<String>,
        raw_ids: Option<String>,
        data_dir: Option<String>,
        allow_private_jmap_hosts: Option<String>,
        timezone: Option<String>,
        default_language: Option<String>,
        telegram_threaded_mode: Option<String>,
        telegram_thread_strategy: Option<String>,
        telegram_shared_thread_mode: Option<String>,
    ) -> Result<Self> {
        let telegram_token = telegram_token.context("TELEGRAM_BOT_TOKEN is not set")?;
        if telegram_token.trim().is_empty() {
            bail!("TELEGRAM_BOT_TOKEN is empty");
        }

        let raw_ids = raw_ids.context("AUTHORIZED_CHAT_IDS is not set")?;

        let authorized_chat_ids: HashSet<ChatId> = raw_ids
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<i64>()
                    .map(ChatId)
                    .with_context(|| format!("invalid chat id '{s}' in AUTHORIZED_CHAT_IDS"))
            })
            .collect::<Result<_>>()?;

        if authorized_chat_ids.is_empty() {
            bail!("AUTHORIZED_CHAT_IDS must contain at least one chat id");
        }

        let data_dir = data_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/data"));

        let allow_private_jmap_hosts = matches!(
            allow_private_jmap_hosts.as_deref(),
            Some("1") | Some("true") | Some("yes")
        );

        let timezone = parse_timezone(timezone.as_deref())?;

        let default_language = match default_language.as_deref() {
            None | Some("") => "en".to_string(),
            Some(name) => crate::i18n::canonical(name)
                .with_context(|| {
                    format!("unsupported BOT_LANGUAGE '{name}' (expected one of: en, fr)")
                })?
                .to_string(),
        };

        let telegram_threaded_mode = match telegram_threaded_mode.as_deref() {
            Some(v)
                if v.eq_ignore_ascii_case("disabled")
                    || v.eq_ignore_ascii_case("off")
                    || v == "0"
                    || v.eq_ignore_ascii_case("false") =>
            {
                ThreadedModeConfig::Disabled
            }
            Some(v)
                if v.eq_ignore_ascii_case("enabled")
                    || v.eq_ignore_ascii_case("on")
                    || v == "1"
                    || v.eq_ignore_ascii_case("true") =>
            {
                ThreadedModeConfig::Enabled
            }
            _ => ThreadedModeConfig::Auto,
        };

        let telegram_thread_strategy = match telegram_thread_strategy.as_deref() {
            Some(v) if v.eq_ignore_ascii_case("server") || v.eq_ignore_ascii_case("host") => {
                ThreadStrategyConfig::Server
            }
            _ => ThreadStrategyConfig::Account,
        };

        let telegram_shared_thread_mode = match telegram_shared_thread_mode.as_deref() {
            Some(v)
                if v.eq_ignore_ascii_case("dedicated") || v.eq_ignore_ascii_case("separate") =>
            {
                SharedThreadModeConfig::Dedicated
            }
            _ => SharedThreadModeConfig::Parent,
        };

        Ok(Self {
            telegram_token,
            authorized_chat_ids,
            data_dir,
            allow_private_jmap_hosts,
            timezone,
            default_language,
            telegram_threaded_mode,
            telegram_thread_strategy,
            telegram_shared_thread_mode,
        })
    }

    #[cfg(test)]
    pub fn for_testing(data_dir: &std::path::Path, chat_id: i64) -> Self {
        let mut authorized_chat_ids = HashSet::new();
        authorized_chat_ids.insert(ChatId(chat_id));
        Self {
            telegram_token: "123456:TEST_TOKEN".to_string(),
            authorized_chat_ids,
            data_dir: data_dir.to_path_buf(),
            allow_private_jmap_hosts: true,
            timezone: chrono_tz::Tz::UTC,
            default_language: "en".to_string(),
            telegram_threaded_mode: ThreadedModeConfig::Auto,
            telegram_thread_strategy: ThreadStrategyConfig::Account,
            telegram_shared_thread_mode: SharedThreadModeConfig::Parent,
        }
    }
}

/// Shared by `Config::parse` and `main`'s early logging setup (which needs
/// the timezone before `Config` itself is loaded, to time-stamp log lines
/// from the very first one).
pub fn parse_timezone(raw: Option<&str>) -> Result<Tz> {
    match raw {
        None | Some("") => Ok(Tz::UTC),
        Some(name) => Tz::from_str(name).map_err(|_| {
            anyhow::anyhow!(
                "invalid TIMEZONE '{name}' (expected an IANA zone name, e.g. 'Europe/Paris')"
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for the common case: only token + chat ids set, every
    /// other optional var absent.
    fn minimal(token: &str, ids: &str) -> Result<Config> {
        Config::parse(
            Some(token.to_string()),
            Some(ids.to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn rejects_missing_token() {
        let err = Config::parse(
            None,
            Some("123".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("TELEGRAM_BOT_TOKEN"));
    }

    #[test]
    fn rejects_empty_token() {
        let err = minimal("  ", "123").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn rejects_missing_chat_ids() {
        let err = Config::parse(
            Some("tok".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("AUTHORIZED_CHAT_IDS"));
    }

    #[test]
    fn rejects_empty_chat_id_list() {
        let err = minimal("tok", "  , ,").unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn rejects_non_numeric_chat_id() {
        let err = minimal("tok", "123,abc").unwrap_err();
        assert!(err.to_string().contains("invalid chat id"));
    }

    #[test]
    fn parses_multiple_chat_ids_with_whitespace() {
        let cfg = minimal("tok", " 111 , 222,333 ").unwrap();
        assert_eq!(cfg.authorized_chat_ids.len(), 3);
        assert!(cfg.authorized_chat_ids.contains(&ChatId(111)));
        assert!(cfg.authorized_chat_ids.contains(&ChatId(222)));
        assert!(cfg.authorized_chat_ids.contains(&ChatId(333)));
    }

    #[test]
    fn defaults_data_dir_when_unset() {
        let cfg = minimal("tok", "1").unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
    }

    #[test]
    fn honors_explicit_data_dir() {
        let cfg = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            Some("/custom".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/custom"));
    }

    #[test]
    fn private_jmap_hosts_disallowed_by_default() {
        let cfg = minimal("tok", "1").unwrap();
        assert!(!cfg.allow_private_jmap_hosts);
    }

    #[test]
    fn private_jmap_hosts_can_be_opted_into() {
        let cfg = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            Some("true".to_string()),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(cfg.allow_private_jmap_hosts);
    }

    #[test]
    fn private_jmap_hosts_ignores_unrecognized_values() {
        let cfg = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            Some("maybe".to_string()),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!cfg.allow_private_jmap_hosts);
    }

    #[test]
    fn timezone_defaults_to_utc() {
        let cfg = minimal("tok", "1").unwrap();
        assert_eq!(cfg.timezone, Tz::UTC);
    }

    #[test]
    fn timezone_accepts_valid_iana_name() {
        let cfg = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            None,
            Some("Europe/Paris".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(cfg.timezone, Tz::Europe__Paris);
    }

    #[test]
    fn timezone_rejects_invalid_name() {
        let err = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            None,
            Some("Not/AZone".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("TIMEZONE"));
    }

    #[test]
    fn language_defaults_to_english() {
        let cfg = minimal("tok", "1").unwrap();
        assert_eq!(cfg.default_language, "en");
    }

    #[test]
    fn language_accepts_a_shipped_locale_case_insensitively() {
        let cfg = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            None,
            None,
            Some("FR".to_string()),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(cfg.default_language, "fr");
    }

    #[test]
    fn language_rejects_an_unsupported_locale() {
        let err = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            None,
            None,
            Some("de".to_string()),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("BOT_LANGUAGE"));
    }

    #[test]
    fn threaded_mode_defaults_to_auto() {
        let cfg = minimal("tok", "1").unwrap();
        assert_eq!(cfg.telegram_threaded_mode, ThreadedModeConfig::Auto);
        assert_eq!(cfg.telegram_thread_strategy, ThreadStrategyConfig::Account);
        assert_eq!(
            cfg.telegram_shared_thread_mode,
            SharedThreadModeConfig::Parent
        );
    }

    #[test]
    fn threaded_mode_parses_explicit_values() {
        let disabled = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            None,
            None,
            None,
            Some("disabled".to_string()),
            Some("server".to_string()),
            Some("dedicated".to_string()),
        )
        .unwrap();
        assert_eq!(
            disabled.telegram_threaded_mode,
            ThreadedModeConfig::Disabled
        );
        assert_eq!(
            disabled.telegram_thread_strategy,
            ThreadStrategyConfig::Server
        );
        assert_eq!(
            disabled.telegram_shared_thread_mode,
            SharedThreadModeConfig::Dedicated
        );

        let enabled = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            None,
            None,
            None,
            None,
            Some("enabled".to_string()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(enabled.telegram_threaded_mode, ThreadedModeConfig::Enabled);
    }

    #[test]
    fn parse_timezone_treats_empty_string_as_utc() {
        assert_eq!(parse_timezone(Some("")).unwrap(), Tz::UTC);
        assert_eq!(parse_timezone(None).unwrap(), Tz::UTC);
    }
}
