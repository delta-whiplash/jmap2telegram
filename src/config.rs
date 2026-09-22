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
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::parse(
            std::env::var("TELEGRAM_BOT_TOKEN").ok(),
            std::env::var("AUTHORIZED_CHAT_IDS").ok(),
            std::env::var("DATA_DIR").ok(),
            std::env::var("ALLOW_PRIVATE_JMAP_HOSTS").ok(),
            std::env::var("TIMEZONE").ok(),
        )
    }

    /// Pure parsing logic, kept separate from `from_env` so it can be
    /// exercised in tests without touching real process-wide environment
    /// variables (which are global mutable state and unsafe to fiddle with
    /// across parallel tests).
    fn parse(
        telegram_token: Option<String>,
        raw_ids: Option<String>,
        data_dir: Option<String>,
        allow_private_jmap_hosts: Option<String>,
        timezone: Option<String>,
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

        Ok(Self {
            telegram_token,
            authorized_chat_ids,
            data_dir,
            allow_private_jmap_hosts,
            timezone,
        })
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
        )
    }

    #[test]
    fn rejects_missing_token() {
        let err = Config::parse(None, Some("123".to_string()), None, None, None).unwrap_err();
        assert!(err.to_string().contains("TELEGRAM_BOT_TOKEN"));
    }

    #[test]
    fn rejects_empty_token() {
        let err = minimal("  ", "123").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn rejects_missing_chat_ids() {
        let err = Config::parse(Some("tok".to_string()), None, None, None, None).unwrap_err();
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
        )
        .unwrap_err();
        assert!(err.to_string().contains("TIMEZONE"));
    }

    #[test]
    fn parse_timezone_treats_empty_string_as_utc() {
        assert_eq!(parse_timezone(Some("")).unwrap(), Tz::UTC);
        assert_eq!(parse_timezone(None).unwrap(), Tz::UTC);
    }
}
