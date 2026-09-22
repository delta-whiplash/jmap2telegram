use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
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
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::parse(
            std::env::var("TELEGRAM_BOT_TOKEN").ok(),
            std::env::var("AUTHORIZED_CHAT_IDS").ok(),
            std::env::var("DATA_DIR").ok(),
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

        Ok(Self {
            telegram_token,
            authorized_chat_ids,
            data_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_token() {
        let err = Config::parse(None, Some("123".to_string()), None).unwrap_err();
        assert!(err.to_string().contains("TELEGRAM_BOT_TOKEN"));
    }

    #[test]
    fn rejects_empty_token() {
        let err = Config::parse(Some("  ".to_string()), Some("123".to_string()), None).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn rejects_missing_chat_ids() {
        let err = Config::parse(Some("tok".to_string()), None, None).unwrap_err();
        assert!(err.to_string().contains("AUTHORIZED_CHAT_IDS"));
    }

    #[test]
    fn rejects_empty_chat_id_list() {
        let err =
            Config::parse(Some("tok".to_string()), Some("  , ,".to_string()), None).unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn rejects_non_numeric_chat_id() {
        let err =
            Config::parse(Some("tok".to_string()), Some("123,abc".to_string()), None).unwrap_err();
        assert!(err.to_string().contains("invalid chat id"));
    }

    #[test]
    fn parses_multiple_chat_ids_with_whitespace() {
        let cfg = Config::parse(
            Some("tok".to_string()),
            Some(" 111 , 222,333 ".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(cfg.authorized_chat_ids.len(), 3);
        assert!(cfg.authorized_chat_ids.contains(&ChatId(111)));
        assert!(cfg.authorized_chat_ids.contains(&ChatId(222)));
        assert!(cfg.authorized_chat_ids.contains(&ChatId(333)));
    }

    #[test]
    fn defaults_data_dir_when_unset() {
        let cfg = Config::parse(Some("tok".to_string()), Some("1".to_string()), None).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
    }

    #[test]
    fn honors_explicit_data_dir() {
        let cfg = Config::parse(
            Some("tok".to_string()),
            Some("1".to_string()),
            Some("/custom".to_string()),
        )
        .unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/custom"));
    }
}
