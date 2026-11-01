use std::collections::HashSet;
use std::time::Instant;

use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQueryId, ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, Me, MessageId,
    ParseMode, ThreadId,
};
use teloxide::utils::command::BotCommands;

use crate::Bot;
use crate::accounts;
use crate::config::ThreadedModeConfig;
use crate::format::{self, TELEGRAM_MAX_MESSAGE_LEN, chunk_text};
use crate::i18n;
use crate::jmap;
use crate::state::{AppState, PendingUndo, UNDO_WINDOW};
use crate::store::{Account, AccountSlot};

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Chat target for outgoing messages, routing either to the chat root
/// or to a specific forum topic (thread).
#[derive(Clone, Copy, Debug)]
pub struct ChatTarget {
    pub id: ChatId,
    pub thread_id: Option<ThreadId>,
}

impl From<ChatId> for ChatTarget {
    fn from(id: ChatId) -> Self {
        Self {
            id,
            thread_id: None,
        }
    }
}

impl From<(ChatId, Option<ThreadId>)> for ChatTarget {
    fn from((id, thread_id): (ChatId, Option<ThreadId>)) -> Self {
        Self { id, thread_id }
    }
}

impl ChatTarget {
    pub fn send(
        &self,
        bot: &Bot,
        text: impl Into<String>,
    ) -> <Bot as teloxide::requests::Requester>::SendMessage {
        let mut req = bot.send_message(self.id, text);
        if let Some(tid) = self.thread_id {
            req = req.message_thread_id(tid);
        }
        req
    }
}

/// Command names are stable English identifiers (telegram's autocomplete and
/// muscle memory both key off them); the French names users already know are
/// kept as hidden aliases, so `/partages` still works. The *descriptions*
/// here are the English fallback; `/help` and the Telegram `/` menu render
/// the localized versions from `i18n::bot_commands`.
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "start / show connection status")]
    Start,
    #[command(
        description = "connect a JMAP account: /login <server_url> <token>",
        parse_with = "split"
    )]
    Login { server_url: String, token: String },
    #[command(description = "delete stored credentials (GDPR right to erasure)")]
    Logout,
    #[command(description = "status of the connected account")]
    Status,
    #[command(
        description = "manage notifications for shared JMAP mailboxes",
        alias = "partages",
        hide_aliases
    )]
    Shared,
    #[command(
        description = "additional personal account: /accounts <server_url> <token> (no argument: list connected)",
        alias = "comptes",
        hide_aliases
    )]
    Accounts { args: String },
    #[command(
        description = "filter a sender/keyword: /mute <term> (no argument: list active filters)"
    )]
    Mute { term: String },
    #[command(description = "remove a filter: /unmute <term>")]
    Unmute { term: String },
    #[command(
        description = "full-text search in the mailbox: /search <text>",
        alias = "rechercher",
        hide_aliases
    )]
    Search { query: String },
    #[command(
        description = "manage threaded mode / forum topics: /threads [on|off]",
        alias = "onglets",
        hide_aliases
    )]
    Threads { arg: String },
    #[command(
        description = "change the bot language: /lang <en|fr>",
        alias = "langue",
        hide_aliases
    )]
    Lang { lang: String },
    #[command(description = "show help")]
    Help,
}

pub fn schema() -> UpdateHandler<Box<dyn std::error::Error + Send + Sync + 'static>> {
    dptree::entry()
        .branch(Update::filter_message().endpoint(message_handler))
        .branch(Update::filter_callback_query().endpoint(callback_handler))
}

async fn message_handler(bot: Bot, msg: Message, me: Me, state: AppState) -> HandlerResult {
    let chat_id = msg.chat.id;
    let target = ChatTarget {
        id: chat_id,
        thread_id: msg.thread_id,
    };
    // Resolved once per update: the stored preference, else this client's
    // own `language_code`, else the operator's `BOT_LANGUAGE`.
    let lang = state.language_for(
        chat_id.0,
        msg.from.as_ref().and_then(|u| u.language_code.as_deref()),
    );

    // Every stored JMAP account is fully controlled (read, archive,
    // delete) by whoever can talk in its chat. That's a reasonable model
    // for a 1:1 DM, but silently extends to every member of a group if an
    // operator ever puts a group chat id in AUTHORIZED_CHAT_IDS - so
    // refuse non-private chats outright rather than let that happen.
    if !msg.chat.is_private() {
        return Ok(());
    }

    if !state.config.authorized_chat_ids.contains(&chat_id) {
        tracing::warn!(chat_id = chat_id.0, "unauthorized access attempt");
        target
            .send(&bot, t!("bot.unauthorized", locale = lang))
            .await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };

    match BotCommands::parse(text, me.username()) {
        Ok(Command::Start) => cmd_start(&bot, &state, target, lang).await?,
        Ok(Command::Help) => {
            target.send(&bot, i18n::help_text(lang)).await?;
        }
        Ok(Command::Login { server_url, token }) => {
            // The token is sensitive: remove it from the chat history as
            // soon as we've read it, regardless of outcome. If Telegram
            // refuses the deletion (e.g. the bot lacks rights in this
            // chat), the token is left sitting in plain sight, so make
            // sure the user actually finds out instead of trusting a
            // promise the bot couldn't keep.
            if bot.delete_message(chat_id, msg.id).await.is_err() {
                target
                    .send(&bot, t!("bot.delete_token_login", locale = lang))
                    .await?;
            }
            cmd_login(&bot, &state, target, server_url, token, lang).await?;
        }
        Ok(Command::Logout) => cmd_logout(&bot, &state, target, lang).await?,
        Ok(Command::Status) => cmd_status(&bot, &state, target, lang).await?,
        Ok(Command::Shared) => cmd_partages(&bot, &state, target, lang).await?,
        Ok(Command::Accounts { args }) => {
            // Same as /login: when this carries a token (i.e. it's not the
            // bare "list what's connected" form), delete it from the chat
            // history right after reading it.
            if !args.trim().is_empty() && bot.delete_message(chat_id, msg.id).await.is_err() {
                target
                    .send(&bot, t!("bot.delete_token_accounts", locale = lang))
                    .await?;
            }
            cmd_comptes(&bot, &state, target, args, lang).await?
        }
        Ok(Command::Mute { term }) => cmd_mute(&bot, &state, target, term, lang).await?,
        Ok(Command::Unmute { term }) => cmd_unmute(&bot, &state, target, term, lang).await?,
        Ok(Command::Search { query }) => cmd_rechercher(&bot, &state, target, query, lang).await?,
        Ok(Command::Threads { arg }) => cmd_threads(&bot, &state, target, &arg, lang).await?,
        Ok(Command::Lang { lang: requested }) => {
            cmd_lang(&bot, &state, target, requested, lang).await?
        }
        Err(_) => {
            target
                .send(&bot, t!("bot.unknown_command", locale = lang))
                .await?;
        }
    }

    Ok(())
}

/// `/lang [en|fr]`: shows or sets the chat's language preference. A
/// deliberate choice here always wins over the auto-detected
/// `language_code` afterwards (see [`crate::i18n::resolve`]).
async fn cmd_lang(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    requested: String,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let requested = requested.trim();
    if requested.is_empty() {
        target
            .send(bot, t!("bot.lang_usage", locale = lang, current = lang))
            .await?;
        return Ok(());
    }

    let Some(target_lang) = i18n::canonical(requested) else {
        target
            .send(bot, t!("bot.lang_invalid", locale = lang))
            .await?;
        return Ok(());
    };

    match state.store.set_language(chat_id.0, target_lang) {
        // Confirm in the newly selected language, not the one they were
        // just reading - that way the change is immediately visible.
        Ok(true) => {
            target
                .send(bot, t!("bot.lang_set", locale = target_lang))
                .await?;
        }
        Ok(false) => {
            target
                .send(bot, t!("bot.no_account", locale = lang))
                .await?;
        }
        Err(e) => {
            target
                .send(bot, t!("bot.storage_error", locale = lang, error = e))
                .await?;
        }
    }
    Ok(())
}

async fn cmd_start(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    if state.store.get(chat_id.0).is_some() {
        cmd_status(bot, state, target, lang).await?;
        return Ok(());
    }

    target.send(bot, t!("bot.welcome", locale = lang)).await?;
    Ok(())
}

async fn cmd_login(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    server_url: String,
    token: String,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let client =
        match jmap::connect(&server_url, &token, state.config.allow_private_jmap_hosts).await {
            Ok(c) => c,
            Err(e) => {
                target
                    .send(
                        bot,
                        t!("bot.login_failed", locale = lang, error = e.localized(lang)),
                    )
                    .await?;
                return Ok(());
            }
        };

    let email = jmap::account_email(&client);
    let initial_state = match jmap::current_email_state(&client).await {
        Ok(s) => s,
        Err(e) => {
            target
                .send(
                    bot,
                    t!("bot.init_failed", locale = lang, error = e.localized(lang)),
                )
                .await?;
            return Ok(());
        }
    };

    // Find if this email is already registered in an existing slot
    let existing_slot = state.store.get(chat_id.0).and_then(|acc| {
        acc.accounts
            .values()
            .find(|s| s.email.eq_ignore_ascii_case(&email))
            .map(|s| s.slot_id.clone())
    });

    let slot_id = existing_slot.unwrap_or_else(|| {
        if state.store.get(chat_id.0).is_none() {
            "default".to_string()
        } else {
            String::new() // let add_account generate a new slot id
        }
    });

    let slot = AccountSlot {
        slot_id,
        server_url,
        token,
        email: email.clone(),
        last_state: Some(initial_state),
        thread_id: None,
    };

    let saved_slot_id = match state.store.add_account(chat_id.0, slot) {
        Ok(id) => id,
        Err(e) => {
            target
                .send(bot, t!("bot.storage_error", locale = lang, error = e))
                .await?;
            return Ok(());
        }
    };
    let _ = state.store.set_language(chat_id.0, lang);
    state.refresh_accounts_gauge();

    // Re-adopting the slot: teardown previous watcher for this slot if it was running
    state.forget_account(chat_id.0, &saved_slot_id).await;
    accounts::adopt_account(bot, state, chat_id.0, saved_slot_id, email.clone(), client).await;

    target
        .send(bot, t!("bot.login_success", locale = lang, email = email))
        .await?;
    Ok(())
}

async fn cmd_logout(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let Some(account) = state.store.get(chat_id.0) else {
        target
            .send(bot, t!("bot.logout_nothing", locale = lang))
            .await?;
        return Ok(());
    };

    if account.accounts.len() <= 1 {
        for slot in account.accounts.values() {
            crate::topics::delete_topic(bot, chat_id.0, slot.thread_id).await;
        }
        for shared in account.shared_accounts.values() {
            crate::topics::delete_topic(bot, chat_id.0, shared.thread_id).await;
        }
        state.forget(chat_id.0).await;
        let removed = state.store.remove(chat_id.0).unwrap_or(false);
        state.refresh_accounts_gauge();

        let text = if removed {
            t!("bot.logout_done", locale = lang)
        } else {
            t!("bot.logout_nothing", locale = lang)
        };
        target.send(bot, text).await?;
        return Ok(());
    }

    // Multiple accounts connected: offer a choice to disconnect individual accounts or all
    let mut buttons = Vec::new();
    for (slot_id, slot) in &account.accounts {
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("🗑 {}", slot.email),
            format!("x:{slot_id}"),
        )]);
    }
    buttons.push(vec![InlineKeyboardButton::callback(
        format!("🚪 {}", t!("bot.logout_all", locale = lang)),
        "logout:all".to_string(),
    )]);

    target
        .send(bot, t!("bot.logout_prompt", locale = lang))
        .reply_markup(InlineKeyboardMarkup::new(buttons))
        .await?;
    Ok(())
}

async fn cmd_status(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let Some(account) = state.store.get(chat_id.0) else {
        target
            .send(bot, t!("bot.no_account", locale = lang))
            .await?;
        return Ok(());
    };

    let account_watchers = state.account_watchers.read().await;
    let watching = account_watchers.contains_key(&(chat_id.0, "default".to_string()))
        || account_watchers.keys().any(|(cid, _)| *cid == chat_id.0);

    let mut text = format!(
        "{}\n{}\n{}",
        t!("bot.status_title", locale = lang, email = account.email()),
        if watching {
            t!("bot.status_watch_on", locale = lang)
        } else {
            t!("bot.status_watch_off", locale = lang)
        },
        if account.last_state().is_some() {
            t!("bot.status_state_synced", locale = lang)
        } else {
            t!("bot.status_state_init", locale = lang)
        },
    );
    if !account.shared_accounts.is_empty() {
        text.push('\n');
        text.push_str(&t!(
            "bot.status_sharing",
            locale = lang,
            count = account.shared_accounts.len()
        ));
    }
    if account.accounts.len() > 1 {
        text.push('\n');
        text.push_str(&t!(
            "bot.status_accounts",
            locale = lang,
            count = account.accounts.len() - 1
        ));
    }
    if !account.muted.is_empty() {
        text.push('\n');
        text.push_str(&t!(
            "bot.status_filters",
            locale = lang,
            count = account.muted.len()
        ));
    }

    target.send(bot, text).await?;
    Ok(())
}

/// `/comptes` (no args) lists the chat's connected accounts.
/// `/comptes <url> <token>` connects an additional account.
async fn cmd_comptes(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    args: String,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let args = args.trim();
    if args.is_empty() {
        return list_accounts(bot, state, target, lang).await;
    }

    let Some(account) = state.store.get(chat_id.0) else {
        target
            .send(bot, t!("bot.accounts_need_primary", locale = lang))
            .await?;
        return Ok(());
    };

    let Some((server_url, token)) = args.split_once(char::is_whitespace) else {
        target
            .send(bot, t!("bot.accounts_usage", locale = lang))
            .await?;
        return Ok(());
    };
    let (server_url, token) = (server_url.trim().to_string(), token.trim().to_string());

    let client =
        match jmap::connect(&server_url, &token, state.config.allow_private_jmap_hosts).await {
            Ok(c) => c,
            Err(e) => {
                target
                    .send(
                        bot,
                        t!("bot.login_failed", locale = lang, error = e.localized(lang)),
                    )
                    .await?;
                return Ok(());
            }
        };

    let email = jmap::account_email(&client);
    if account.contains_email(&email) {
        target
            .send(
                bot,
                t!("bot.accounts_already", locale = lang, email = email),
            )
            .await?;
        return Ok(());
    }

    let initial_state = match jmap::current_email_state(&client).await {
        Ok(s) => s,
        Err(e) => {
            target
                .send(
                    bot,
                    t!("bot.init_failed", locale = lang, error = e.localized(lang)),
                )
                .await?;
            return Ok(());
        }
    };

    let slot = AccountSlot {
        slot_id: String::new(),
        server_url,
        token,
        email: email.clone(),
        last_state: Some(initial_state),
        thread_id: None,
    };

    let slot_id = match state.store.add_account(chat_id.0, slot) {
        Ok(id) => id,
        Err(e) => {
            target
                .send(bot, t!("bot.storage_error", locale = lang, error = e))
                .await?;
            return Ok(());
        }
    };

    accounts::adopt_account(bot, state, chat_id.0, slot_id, email.clone(), client).await;

    target
        .send(bot, t!("bot.accounts_added", locale = lang, email = email))
        .await?;
    Ok(())
}

async fn list_accounts(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let Some(account) = state.store.get(chat_id.0) else {
        target
            .send(bot, t!("bot.no_account", locale = lang))
            .await?;
        return Ok(());
    };

    if account.accounts.is_empty() {
        target
            .send(bot, t!("bot.no_account", locale = lang))
            .await?;
        return Ok(());
    }

    let mut slots: Vec<(&String, &AccountSlot)> = account.accounts.iter().collect();
    slots.sort_by(|(id_a, a), (id_b, b)| {
        if *id_a == "default" {
            std::cmp::Ordering::Less
        } else if *id_b == "default" {
            std::cmp::Ordering::Greater
        } else {
            a.email.cmp(&b.email)
        }
    });

    let mut lines = Vec::new();
    let mut buttons = Vec::new();
    for (index, (slot_id, slot)) in slots.iter().enumerate() {
        lines.push(
            t!(
                "bot.accounts_list_extra",
                locale = lang,
                index = index + 1,
                email = slot.email
            )
            .to_string(),
        );
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("🗑 {}", slot.email),
            format!("x:{slot_id}"),
        )]);
    }

    let mut text = format!(
        "{}\n{}",
        t!("bot.accounts_list_title", locale = lang),
        lines.join("\n")
    );
    if account.accounts.len() <= 1 {
        text.push_str("\n\n");
        text.push_str(&t!("bot.accounts_no_extra", locale = lang));
    } else {
        text.push_str("\n\n");
        text.push_str(&t!("bot.accounts_legend", locale = lang));
    }

    let send = target.send(bot, text);
    let send = if !buttons.is_empty() {
        send.reply_markup(InlineKeyboardMarkup::new(buttons))
    } else {
        send
    };
    send.await?;
    Ok(())
}

/// Shows the live, dynamic list of shared JMAP accounts the connected
/// tokens currently have access to (delegated/shared mailboxes, distinct
/// from the chat's own accounts), with a toggle button per account across
/// the primary account and all extra accounts. Always reconnects rather
/// than reusing the cached client, so a mailbox access grant added on the
/// server after /login or /accounts shows up without needing a restart.
async fn cmd_partages(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let Some(account) = state.store.get(chat_id.0) else {
        target
            .send(bot, t!("bot.no_account", locale = lang))
            .await?;
        return Ok(());
    };

    let (accounts, any_connected) = discover_all_shared_accounts(state, &account).await;
    if !any_connected {
        target
            .send(
                bot,
                t!(
                    "bot.login_failed",
                    locale = lang,
                    error = jmap::JmapError::Unreachable.localized(lang)
                ),
            )
            .await?;
        return Ok(());
    }

    if accounts.is_empty() {
        target
            .send(bot, t!("bot.shared_none", locale = lang))
            .await?;
        return Ok(());
    }

    let enabled: HashSet<String> = account.shared_accounts.keys().cloned().collect();
    let keyboard = format::shared_accounts_keyboard(&accounts, &enabled);
    target
        .send(bot, t!("bot.shared_title", locale = lang))
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

/// Refreshes an already-sent /partages message's keyboard in place after a
/// toggle, so the checkmarks reflect the new state without spamming a new
/// message per tap. Best-effort: if the reconnect needed to re-list shared
/// accounts fails, the toggle itself has already succeeded/failed and been
/// reported via the callback answer, so this silently leaves the old
/// keyboard in place rather than surfacing a second error for one refresh.
async fn refresh_partages_keyboard(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    message_id: MessageId,
) -> HandlerResult {
    let Some(account) = state.store.get(chat_id.0) else {
        return Ok(());
    };

    let (accounts, any_connected) = discover_all_shared_accounts(state, &account).await;
    if !any_connected {
        return Ok(());
    }

    let enabled: HashSet<String> = account.shared_accounts.keys().cloned().collect();
    let keyboard = format::shared_accounts_keyboard(&accounts, &enabled);
    let _ = bot
        .edit_message_reply_markup(chat_id, message_id)
        .reply_markup(keyboard)
        .await;
    Ok(())
}

/// Discovers every shared/delegated mailbox available across all connected
/// accounts. Returns the list of items and whether at least one account
/// connection succeeded.
async fn discover_all_shared_accounts(
    state: &AppState,
    account: &Account,
) -> (Vec<format::SharedAccountItem>, bool) {
    let mut discovered = Vec::new();
    let mut any_connected = false;

    let mut slots: Vec<(&String, &AccountSlot)> = account.accounts.iter().collect();
    slots.sort_by(|(_, a), (_, b)| a.email.cmp(&b.email));

    for (slot_id, slot) in slots {
        match jmap::connect(
            &slot.server_url,
            &slot.token,
            state.config.allow_private_jmap_hosts,
        )
        .await
        {
            Ok(client) => {
                any_connected = true;
                let parent_slot = if slot_id == "default" {
                    None
                } else {
                    Some(slot_id.clone())
                };
                for (id, name) in jmap::list_shared_accounts(&client) {
                    discovered.push(format::SharedAccountItem {
                        id,
                        name,
                        parent_slot: parent_slot.clone(),
                    });
                }
            }
            Err(e) => {
                tracing::warn!(
                    slot_id = %slot_id,
                    error = %e,
                    "failed to connect to account to list shared mailboxes"
                );
            }
        }
    }

    (discovered, any_connected)
}

/// Disconnects one personal account slot.
async fn disconnect_account(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    query_id: CallbackQueryId,
    slot_id: &str,
    lang: &str,
) -> HandlerResult {
    if let Some(slot) = state
        .store
        .get(chat_id.0)
        .and_then(|a| a.accounts.get(slot_id).cloned())
    {
        crate::topics::delete_topic(bot, chat_id.0, slot.thread_id).await;
    }
    state.forget_account(chat_id.0, slot_id).await;
    match state.store.remove_account(chat_id.0, slot_id) {
        Ok(true) => {
            state.refresh_accounts_gauge();
            bot.answer_callback_query(query_id)
                .text(t!("bot.disconnect_done", locale = lang))
                .await?;
        }
        Ok(false) => {
            bot.answer_callback_query(query_id)
                .text(t!("bot.disconnect_already", locale = lang))
                .await?;
        }
        Err(e) => {
            bot.answer_callback_query(query_id)
                .text(t!("bot.error_generic", locale = lang, error = e))
                .show_alert(true)
                .await?;
        }
    }
    Ok(())
}

struct SharedToggleTarget<'a> {
    account_id: &'a str,
    parent_slot: Option<String>,
}

async fn toggle_shared_account(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    message_id: MessageId,
    query_id: CallbackQueryId,
    target: SharedToggleTarget<'_>,
    lang: &str,
) -> HandlerResult {
    let Some(account) = state.store.get(chat_id.0) else {
        bot.answer_callback_query(query_id)
            .text(t!("bot.account_not_connected", locale = lang))
            .await?;
        return Ok(());
    };

    let SharedToggleTarget {
        account_id,
        parent_slot,
    } = target;

    if account.shared_accounts.contains_key(account_id) {
        if let Some(shared) = account.shared_accounts.get(account_id) {
            crate::topics::delete_topic(bot, chat_id.0, shared.thread_id).await;
        }
        state.forget_shared(chat_id.0, account_id).await;
        let _ = state.store.remove_shared_account(chat_id.0, account_id);
        bot.answer_callback_query(query_id)
            .text(t!("bot.shared_off", locale = lang))
            .await?;
    } else {
        let (server_url, token, parent_slot_to_save) = match &parent_slot {
            Some(slot) => match account.accounts.get(slot) {
                Some(extra) => (
                    extra.server_url.as_str(),
                    extra.token.as_str(),
                    Some(slot.clone()),
                ),
                None => {
                    bot.answer_callback_query(query_id)
                        .text(t!("bot.account_not_connected", locale = lang))
                        .await?;
                    return Ok(());
                }
            },
            None => match account.primary_slot() {
                Some(primary) => (
                    primary.server_url.as_str(),
                    primary.token.as_str(),
                    Some(primary.slot_id.clone()),
                ),
                None => {
                    bot.answer_callback_query(query_id)
                        .text(t!("bot.account_not_connected", locale = lang))
                        .await?;
                    return Ok(());
                }
            },
        };

        let client = match jmap::connect_shared(
            server_url,
            token,
            state.config.allow_private_jmap_hosts,
            account_id,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                bot.answer_callback_query(query_id)
                    .text(t!(
                        "bot.login_failed",
                        locale = lang,
                        error = e.localized(lang)
                    ))
                    .show_alert(true)
                    .await?;
                return Ok(());
            }
        };

        let display_name = client
            .session()
            .account(account_id)
            .map(|a| a.name().to_string())
            .unwrap_or_else(|| account_id.to_string());

        let initial_state = match jmap::current_email_state(&client).await {
            Ok(s) => s,
            Err(e) => {
                bot.answer_callback_query(query_id)
                    .text(t!(
                        "bot.init_failed",
                        locale = lang,
                        error = e.localized(lang)
                    ))
                    .show_alert(true)
                    .await?;
                return Ok(());
            }
        };

        if let Err(e) = state.store.set_shared_account(
            chat_id.0,
            account_id.to_string(),
            display_name.clone(),
            Some(initial_state),
            parent_slot_to_save,
        ) {
            bot.answer_callback_query(query_id)
                .text(t!("bot.storage_error", locale = lang, error = e))
                .show_alert(true)
                .await?;
            return Ok(());
        }

        accounts::adopt_shared(
            bot,
            state,
            chat_id.0,
            account_id.to_string(),
            display_name,
            client,
        )
        .await;

        bot.answer_callback_query(query_id)
            .text(t!("bot.shared_on", locale = lang))
            .await?;
    }

    refresh_partages_keyboard(bot, state, chat_id, message_id).await
}

/// `/mute <terme>` adds a filter; `/mute` with no argument lists the
/// chat's active filters instead of erroring, since "what's currently
/// muted" is something a user reasonably wants to check without having to
/// remember what they typed weeks ago.
async fn cmd_mute(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    term: String,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let term = term.trim();
    if term.is_empty() {
        let Some(account) = state.store.get(chat_id.0) else {
            target
                .send(bot, t!("bot.no_account", locale = lang))
                .await?;
            return Ok(());
        };
        let text = if account.muted.is_empty() {
            t!("bot.mute_none", locale = lang).to_string()
        } else {
            let list = account
                .muted
                .iter()
                .map(|m| format!("• {m}"))
                .collect::<Vec<_>>()
                .join("\n");
            t!("bot.mute_list", locale = lang, list = list).to_string()
        };
        target.send(bot, text).await?;
        return Ok(());
    }

    match state.store.mute(chat_id.0, term) {
        Ok(true) => {
            target
                .send(bot, t!("bot.mute_added", locale = lang, term = term))
                .await?;
        }
        Ok(false) if state.store.get(chat_id.0).is_none() => {
            target
                .send(bot, t!("bot.no_account", locale = lang))
                .await?;
        }
        Ok(false) => {
            target
                .send(bot, t!("bot.mute_already", locale = lang, term = term))
                .await?;
        }
        Err(e) => {
            target
                .send(bot, t!("bot.error_generic", locale = lang, error = e))
                .await?;
        }
    }
    Ok(())
}

async fn cmd_unmute(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    term: String,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let term = term.trim();
    if term.is_empty() {
        target
            .send(bot, t!("bot.unmute_usage", locale = lang))
            .await?;
        return Ok(());
    }

    match state.store.unmute(chat_id.0, term) {
        Ok(true) => {
            target
                .send(bot, t!("bot.unmute_done", locale = lang, term = term))
                .await?;
        }
        Ok(false) => {
            target
                .send(
                    bot,
                    t!("bot.unmute_not_filtered", locale = lang, term = term),
                )
                .await?;
        }
        Err(e) => {
            target
                .send(bot, t!("bot.error_generic", locale = lang, error = e))
                .await?;
        }
    }
    Ok(())
}

/// Read-only full-text search across all connected accounts. Results are
/// rendered as ordinary notification messages, complete with action buttons,
/// tagged with [account] when multiple accounts are connected.
async fn cmd_rechercher(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    query: String,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let query = query.trim();
    if query.is_empty() {
        target
            .send(bot, t!("bot.search_usage", locale = lang))
            .await?;
        return Ok(());
    }

    let all_clients = match state.all_clients_for_chat(chat_id.0).await {
        Ok(c) if !c.is_empty() => c,
        Ok(_) => {
            target
                .send(bot, t!("bot.no_account", locale = lang))
                .await?;
            return Ok(());
        }
        Err(e) => {
            target
                .send(
                    bot,
                    t!(
                        "bot.connection_error",
                        locale = lang,
                        error = jmap::localized_error(&e, lang)
                    ),
                )
                .await?;
            return Ok(());
        }
    };

    // If search is executed inside a specific topic, prioritize that account
    let clients = if let Some(tid) = target.thread_id
        && let Some(account) = state.store.get(chat_id.0)
        && let Some(matched_slot) = account
            .accounts
            .values()
            .find(|s| s.thread_id == Some(tid.0.0))
    {
        if let Ok(Some(client)) = state
            .client_for_account(chat_id.0, &matched_slot.slot_id)
            .await
        {
            vec![(matched_slot.clone(), client)]
        } else {
            all_clients
        }
    } else {
        all_clients
    };

    let mut action_req = bot.send_chat_action(chat_id, ChatAction::Typing);
    if let Some(tid) = target.thread_id {
        action_req = action_req.message_thread_id(tid);
    }
    let _ = action_req.await;

    let multiple_accounts = clients.len() > 1;
    let mut all_results = Vec::new();
    let mut last_err = None;

    for (slot, client) in &clients {
        match jmap::search(client, query).await {
            Ok(results) => {
                for summary in results {
                    all_results.push((slot.slot_id.clone(), slot.email.clone(), summary));
                }
            }
            Err(e) => {
                tracing::warn!(
                    slot_id = %slot.slot_id,
                    error = %e,
                    "search failed on account slot"
                );
                last_err = Some(e);
            }
        }
    }

    if all_results.is_empty() {
        if let Some(e) = last_err
            && clients.len() == 1
        {
            target
                .send(bot, format!("❌ {}", e.localized(lang)))
                .await?;
            return Ok(());
        }
        target
            .send(
                bot,
                t!("bot.search_no_result", locale = lang, query = query),
            )
            .await?;
        return Ok(());
    }

    target
        .send(
            bot,
            t!(
                "bot.search_results",
                locale = lang,
                count = all_results.len(),
                query = query
            ),
        )
        .await?;

    for (slot_id, email, summary) in &all_results {
        let account_label = if multiple_accounts {
            Some(email.as_str())
        } else {
            None
        };
        let btn_account_id = if slot_id == "default" {
            None
        } else {
            Some(slot_id.as_str())
        };
        let text = format::notification_text(summary, state.config.timezone, account_label, lang);
        let keyboard =
            format::notification_keyboard(&summary.id, btn_account_id, false, false, lang);
        target
            .send(bot, text)
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboard)
            .await?;
    }
    Ok(())
}

/// `/threads [on|off]` manages Telegram Threaded Conversations (forum topics).
async fn cmd_threads(
    bot: &Bot,
    state: &AppState,
    target: impl Into<ChatTarget>,
    arg: &str,
    lang: &str,
) -> HandlerResult {
    let target = target.into();
    let chat_id = target.id;
    let Some(account) = state.store.get(chat_id.0) else {
        target
            .send(bot, t!("bot.no_account", locale = lang))
            .await?;
        return Ok(());
    };

    let trimmed = arg.trim().to_lowercase();
    match trimmed.as_str() {
        "on" | "enable" | "activer" => {
            state.store.set_threads_enabled(chat_id.0, Some(true))?;
            let sync_results = crate::topics::sync_all_topics(bot, state, chat_id.0).await?;
            let created_count = sync_results.iter().filter(|(_, tid)| tid.is_some()).count();
            if created_count > 0 {
                target
                    .send(
                        bot,
                        t!(
                            "bot.threads_enabled_success",
                            locale = lang,
                            count = created_count
                        ),
                    )
                    .await?;
            } else {
                target
                    .send(bot, t!("bot.threads_not_forum", locale = lang))
                    .await?;
            }
        }
        "off" | "disable" | "desactiver" => {
            state.store.set_threads_enabled(chat_id.0, Some(false))?;
            target
                .send(bot, t!("bot.threads_disabled_success", locale = lang))
                .await?;
        }
        _ => {
            let is_enabled = crate::topics::is_threaded_mode_enabled(&state.config, Some(&account));
            let status_label = if is_enabled {
                t!("bot.threads_status_on", locale = lang)
            } else {
                t!("bot.threads_status_off", locale = lang)
            };

            let server_mode = match state.config.telegram_threaded_mode {
                ThreadedModeConfig::Auto => "auto",
                ThreadedModeConfig::Enabled => "enabled",
                ThreadedModeConfig::Disabled => "disabled",
            };

            let text = t!(
                "bot.threads_overview",
                locale = lang,
                status = status_label,
                server_mode = server_mode,
                count = account.accounts.len()
            );

            let toggle_btn = if is_enabled {
                InlineKeyboardButton::callback(
                    t!("bot.threads_btn_disable", locale = lang),
                    "th:off",
                )
            } else {
                InlineKeyboardButton::callback(t!("bot.threads_btn_enable", locale = lang), "th:on")
            };

            let sync_btn = InlineKeyboardButton::callback(
                t!("bot.threads_btn_sync", locale = lang),
                "th:sync",
            );

            let keyboard = InlineKeyboardMarkup::default()
                .append_row(vec![toggle_btn])
                .append_row(vec![sync_btn]);

            target.send(bot, text).reply_markup(keyboard).await?;
        }
    }

    Ok(())
}

enum TriageAction {
    Archive,
    Junk,
    Delete,
}

impl TriageAction {
    /// Bounded metric label for `triage_actions_total` (never user-derived).
    fn metric_label(&self) -> &'static str {
        match self {
            TriageAction::Archive => "archive",
            TriageAction::Junk => "junk",
            TriageAction::Delete => "delete",
        }
    }
}

/// Which chat/message a callback tap came from - bundled together purely
/// to keep `perform_triage_action`'s argument count sane.
struct CallbackCtx {
    chat_id: ChatId,
    message_id: MessageId,
    query_id: CallbackQueryId,
}

/// Which email an action applies to, and in which JMAP account.
struct EmailRef<'a> {
    account_id: Option<&'a str>,
    email_id: &'a str,
}

/// Archives/marks spam/moves-to-trash a message, snapshotting its current
/// mailboxes first so `/undo` can put it back exactly where it was, then
/// leaves a time-limited "↩️ Annuler" button in place of the usual cleared
/// keyboard. Shared by all three triage actions since they only differ in
/// which JMAP call to make and which mailbox the message ends up in.
async fn perform_triage_action(
    bot: &Bot,
    state: &AppState,
    ctx: CallbackCtx,
    client: &jmap_client::client::Client,
    target: EmailRef<'_>,
    action: TriageAction,
    lang: &str,
) -> HandlerResult {
    let CallbackCtx {
        chat_id,
        message_id,
        query_id,
    } = ctx;
    let EmailRef {
        account_id,
        email_id,
    } = target;

    let restore_mailbox_ids = match jmap::mailbox_ids_of(client, email_id).await {
        Ok(ids) => ids,
        Err(e) => {
            bot.answer_callback_query(query_id)
                .text(format!("❌ {}", e.localized(lang)))
                .show_alert(true)
                .await?;
            return Ok(());
        }
    };

    let action_label = action.metric_label();
    let (result, toast) = match action {
        TriageAction::Archive => (
            jmap::archive(client, email_id).await,
            t!("triage.archived", locale = lang),
        ),
        TriageAction::Junk => (
            jmap::junk(client, email_id).await,
            t!("triage.spam", locale = lang),
        ),
        TriageAction::Delete => (
            jmap::delete(client, email_id).await,
            t!("triage.deleted", locale = lang),
        ),
    };

    match result {
        Ok(()) => {
            state.metrics.triage_action(action_label, "ok");
            bot.answer_callback_query(query_id).text(toast).await?;
            state.pending_undo.write().await.insert(
                chat_id.0,
                PendingUndo {
                    account_id: account_id.map(str::to_string),
                    email_id: email_id.to_string(),
                    restore_mailbox_ids,
                    expires_at: Instant::now() + UNDO_WINDOW,
                },
            );
            state.refresh_undo_gauge().await;
            let _ = bot
                .edit_message_reply_markup(chat_id, message_id)
                .reply_markup(format::undo_keyboard(email_id, account_id, lang))
                .await;
        }
        Err(e) => {
            state.metrics.triage_action(action_label, "error");
            bot.answer_callback_query(query_id)
                .text(format!("❌ {}", e.localized(lang)))
                .show_alert(true)
                .await?;
        }
    }
    Ok(())
}

async fn callback_handler(bot: Bot, q: CallbackQuery, state: AppState) -> HandlerResult {
    let Some(message) = q.regular_message() else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };
    let chat_id = message.chat.id;
    let message_id = message.id;
    // Same resolution as `message_handler`, using the tapping user's own
    // client language.
    let lang = state.language_for(chat_id.0, q.from.language_code.as_deref());

    if !message.chat.is_private() || !state.config.authorized_chat_ids.contains(&chat_id) {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    }

    let Some(data) = q.data.clone() else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };

    // Three shapes: "s:{account_id}" or "s:{slot}:{account_id}" (toggle a
    // shared account from /partages), "x:{slot_id}" (disconnect an extra
    // account from /accounts), or an email action, either "{action}:{email_id}"
    // (the chat's own mailbox) or "{action}:{account_id}:{email_id}" (a shared
    // mailbox - JMAP email ids are only unique within their account, so
    // the account has to travel with the id to route to the right client).
    let mut parts = data.splitn(3, ':');
    let (Some(action), Some(second)) = (parts.next(), parts.next()) else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };
    let third = parts.next();

    if action == "s" {
        let (account_id, parent_slot) = match third {
            Some(id) => {
                let slot = if second == "p" {
                    None
                } else {
                    Some(second.to_string())
                };
                (id, slot)
            }
            None => (second, None),
        };
        return toggle_shared_account(
            &bot,
            &state,
            chat_id,
            message_id,
            q.id,
            SharedToggleTarget {
                account_id,
                parent_slot,
            },
            lang,
        )
        .await;
    }
    if action == "x" {
        return disconnect_account(&bot, &state, chat_id, q.id, second, lang).await;
    }
    if action == "th" {
        match second {
            "on" => {
                let _ = state.store.set_threads_enabled(chat_id.0, Some(true));
                let sync_results = crate::topics::sync_all_topics(&bot, &state, chat_id.0).await?;
                let created_count = sync_results.iter().filter(|(_, tid)| tid.is_some()).count();
                let toast = if created_count > 0 {
                    t!("bot.threads_toast_enabled", locale = lang)
                } else {
                    t!("bot.threads_toast_not_forum", locale = lang)
                };
                bot.answer_callback_query(q.id).text(toast).await?;
                let _ = bot.delete_message(chat_id, message_id).await;
                cmd_threads(&bot, &state, chat_id, "", lang).await?;
            }
            "off" => {
                let _ = state.store.set_threads_enabled(chat_id.0, Some(false));
                bot.answer_callback_query(q.id)
                    .text(t!("bot.threads_toast_disabled", locale = lang))
                    .await?;
                let _ = bot.delete_message(chat_id, message_id).await;
                cmd_threads(&bot, &state, chat_id, "", lang).await?;
            }
            "sync" => {
                let sync_results = crate::topics::sync_all_topics(&bot, &state, chat_id.0).await?;
                let created_count = sync_results.iter().filter(|(_, tid)| tid.is_some()).count();
                let toast = format!(
                    "{} ({})",
                    t!("bot.threads_toast_synced", locale = lang),
                    created_count
                );
                bot.answer_callback_query(q.id).text(toast).await?;
                let _ = bot.delete_message(chat_id, message_id).await;
                cmd_threads(&bot, &state, chat_id, "", lang).await?;
            }
            _ => {}
        }
        return Ok(());
    }
    if action == "logout" && second == "all" {
        if let Some(account) = state.store.get(chat_id.0) {
            for slot in account.accounts.values() {
                crate::topics::delete_topic(&bot, chat_id.0, slot.thread_id).await;
            }
            for shared in account.shared_accounts.values() {
                crate::topics::delete_topic(&bot, chat_id.0, shared.thread_id).await;
            }
        }
        state.forget(chat_id.0).await;
        let _ = state.store.remove(chat_id.0);
        state.refresh_accounts_gauge();
        let _ = bot
            .edit_message_reply_markup(chat_id, message_id)
            .reply_markup(InlineKeyboardMarkup::default())
            .await;
        bot.answer_callback_query(q.id)
            .text(t!("bot.logout_done", locale = lang))
            .await?;
        bot.send_message(chat_id, t!("bot.logout_done", locale = lang))
            .await?;
        return Ok(());
    }

    let (account_id, email_id): (Option<&str>, &str) = match third {
        Some(email_id) => (Some(second), email_id),
        None => (None, second),
    };

    // Expand/collapse of the notification's action menu is pure
    // presentation - the message's read state rides in the callback_data's
    // case - so it's handled here, before any JMAP client is resolved, and
    // keeps working even when the underlying account is offline.
    if let Some((already_read, expanded)) = match action {
        "e" => Some((false, true)),
        "E" => Some((true, true)),
        "c" => Some((false, false)),
        "C" => Some((true, false)),
        _ => None,
    } {
        let _ = bot
            .edit_message_reply_markup(chat_id, message_id)
            .reply_markup(format::notification_keyboard(
                email_id,
                account_id,
                already_read,
                expanded,
                lang,
            ))
            .await;
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    }

    // "n" ("already read") is an intentionally inert label button: it still
    // has to answer the callback so Telegram stops showing a loading spinner
    // on the tap, but there is nothing to do. Like the expand/collapse
    // toggle it is pure presentation, so it is handled here, before any JMAP
    // client is resolved, and keeps working when the account is offline.
    if action == "n" {
        bot.answer_callback_query(q.id)
            .text(t!("bot.already_read", locale = lang))
            .await?;
        return Ok(());
    }

    // account_id names either a shared JMAP account or an account slot id.
    // Try shared first, then account slot.
    let client_result = match account_id {
        Some(acc) => match state.client_for_shared(chat_id.0, acc).await {
            Ok(Some(c)) => Ok(Some(c)),
            Ok(None) => state.client_for_account(chat_id.0, acc).await,
            Err(e) => Err(e),
        },
        None => state.client_for(chat_id.0).await,
    };
    let client = match client_result {
        Ok(Some(c)) => c,
        Ok(None) => {
            bot.answer_callback_query(q.id)
                .text(t!("bot.no_account_or_disabled", locale = lang))
                .await?;
            return Ok(());
        }
        Err(e) => {
            bot.answer_callback_query(q.id)
                .text(t!(
                    "bot.connection_error",
                    locale = lang,
                    error = jmap::localized_error(&e, lang)
                ))
                .show_alert(true)
                .await?;
            return Ok(());
        }
    };

    match action {
        "r" => match jmap::mark_read(&client, email_id).await {
            Ok(()) => {
                state.metrics.mark_read("ok");
                bot.answer_callback_query(q.id)
                    .text(t!("bot.mark_read_done", locale = lang))
                    .await?;
                // Reflect the new state on the notification itself (the
                // "Lu" button becomes an inert checkmark) instead of
                // leaving the chat looking exactly as before the tap. The
                // menu stays expanded, since that's where the read button
                // the user just tapped lives.
                let _ = bot
                    .edit_message_reply_markup(chat_id, message_id)
                    .reply_markup(format::notification_keyboard(
                        email_id, account_id, true, true, lang,
                    ))
                    .await;
            }
            Err(e) => {
                state.metrics.mark_read("error");
                bot.answer_callback_query(q.id)
                    .text(format!("❌ {}", e.localized(lang)))
                    .show_alert(true)
                    .await?;
            }
        },
        "a" => {
            return perform_triage_action(
                &bot,
                &state,
                CallbackCtx {
                    chat_id,
                    message_id,
                    query_id: q.id,
                },
                &client,
                EmailRef {
                    account_id,
                    email_id,
                },
                TriageAction::Archive,
                lang,
            )
            .await;
        }
        "j" => {
            return perform_triage_action(
                &bot,
                &state,
                CallbackCtx {
                    chat_id,
                    message_id,
                    query_id: q.id,
                },
                &client,
                EmailRef {
                    account_id,
                    email_id,
                },
                TriageAction::Junk,
                lang,
            )
            .await;
        }
        "d" => {
            return perform_triage_action(
                &bot,
                &state,
                CallbackCtx {
                    chat_id,
                    message_id,
                    query_id: q.id,
                },
                &client,
                EmailRef {
                    account_id,
                    email_id,
                },
                TriageAction::Delete,
                lang,
            )
            .await;
        }
        "u" => {
            let pending = state.pending_undo.write().await.remove(&chat_id.0);
            state.refresh_undo_gauge().await;
            match pending {
                Some(p)
                    if !p.is_expired()
                        && p.account_id.as_deref() == account_id
                        && p.email_id == email_id =>
                {
                    match jmap::restore_mailboxes(&client, email_id, p.restore_mailbox_ids).await {
                        Ok(()) => {
                            state.metrics.undo("restored");
                            bot.answer_callback_query(q.id)
                                .text(t!("bot.undo_done", locale = lang))
                                .await?;
                            let _ = bot
                                .edit_message_reply_markup(chat_id, message_id)
                                .reply_markup(format::notification_keyboard(
                                    email_id, account_id, false, false, lang,
                                ))
                                .await;
                        }
                        Err(e) => {
                            state.metrics.undo("error");
                            bot.answer_callback_query(q.id)
                                .text(format!("❌ {}", e.localized(lang)))
                                .show_alert(true)
                                .await?;
                        }
                    }
                }
                _ => {
                    state.metrics.undo("expired");
                    bot.answer_callback_query(q.id)
                        .text(t!("bot.undo_expired", locale = lang))
                        .show_alert(true)
                        .await?;
                }
            }
        }
        "f" => {
            let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;
            match jmap::fetch_full_text(&client, email_id).await {
                Ok(text) => {
                    bot.answer_callback_query(q.id).await?;
                    // An empty body (no text part, no preview) still needs
                    // *something* readable; the placeholder is localized
                    // here rather than in the JMAP layer.
                    let text = if text.trim().is_empty() {
                        t!("notif.empty_message", locale = lang).to_string()
                    } else {
                        text
                    };
                    // One chunk failing to send (a flood limit, a network
                    // blip, Telegram hiccup) must not silently abort every
                    // chunk after it - the reader would get the first
                    // pages of a long mail and then silence, with nothing
                    // to hint that content is missing. So: log each
                    // failure, keep sending the rest, and close with a
                    // message listing which parts never arrived.
                    let chunks = chunk_text(&text, TELEGRAM_MAX_MESSAGE_LEN - 16);
                    let total = chunks.len();
                    let mut failed: Vec<usize> = Vec::new();
                    for (index, chunk) in chunks.into_iter().enumerate() {
                        if let Err(e) = bot.send_message(chat_id, chunk).await {
                            tracing::error!(
                                chat_id = chat_id.0,
                                email_id,
                                part = index + 1,
                                total_parts = total,
                                error = %e,
                                "failed to send a chunk of the full message"
                            );
                            failed.push(index + 1);
                        }
                    }
                    if !failed.is_empty() {
                        // Best-effort: if Telegram is unreachable wholesale,
                        // this summary fails too and we only lose the
                        // in-chat notice - the per-chunk errors are already
                        // in the logs. Retrying "Lire tout" resends the
                        // whole body, which is why the hint says so.
                        let listing = failed
                            .iter()
                            .map(|p| format!("{p}/{total}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let _ = bot
                            .send_message(
                                chat_id,
                                t!(
                                    "bot.full_text_partial",
                                    locale = lang,
                                    count = failed.len(),
                                    listing = listing
                                ),
                            )
                            .await;
                    }
                }
                Err(e) => {
                    bot.answer_callback_query(q.id)
                        .text(format!("❌ {}", e.localized(lang)))
                        .show_alert(true)
                        .await?;
                }
            }
        }
        // "n" is handled up front, as pure presentation.
        _ => {
            bot.answer_callback_query(q.id).await?;
        }
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors how teloxide dispatches: the raw text including the leading
    /// slash, plus the bot's own username.
    fn parse(text: &str) -> Result<Command, teloxide::utils::command::ParseError> {
        BotCommands::parse(text, "testbot")
    }

    #[test]
    fn every_advertised_english_command_name_parses_to_a_variant() {
        // Guards the invariant that `i18n::bot_commands` (what we register
        // with Telegram) and the `Command` enum (what we actually parse)
        // can't drift apart: a rename on one side without the other would
        // leave a menu entry that answers with "unknown command".
        for c in i18n::bot_commands("en") {
            // Some commands take required arguments (`/login <url> <token>`),
            // so a bare name isn't expected to parse for those; either form
            // succeeding proves the name is one we actually handle.
            let bare = parse(&format!("/{}", c.command));
            let with_args = parse(&format!("/{} a b", c.command));
            assert!(
                bare.is_ok() || with_args.is_ok(),
                "/{} is advertised in the Telegram menu but doesn't parse",
                c.command
            );
        }
    }

    #[test]
    fn the_renamed_english_commands_parse() {
        assert!(matches!(parse("/shared"), Ok(Command::Shared)));
        assert!(matches!(parse("/accounts"), Ok(Command::Accounts { .. })));
        assert!(matches!(parse("/search hello"), Ok(Command::Search { .. })));
        assert!(matches!(parse("/lang fr"), Ok(Command::Lang { .. })));
    }

    #[test]
    fn the_old_french_command_names_keep_working_as_aliases() {
        // Existing users' muscle memory (and any pinned command they typed
        // before the rename) must not break.
        assert!(matches!(parse("/partages"), Ok(Command::Shared)));
        assert!(matches!(parse("/comptes"), Ok(Command::Accounts { .. })));
        assert!(matches!(
            parse("/rechercher hello"),
            Ok(Command::Search { .. })
        ));
        assert!(matches!(parse("/langue fr"), Ok(Command::Lang { .. })));
    }

    #[test]
    fn every_advertised_command_has_a_translated_description_in_both_locales() {
        for lang in ["en", "fr"] {
            for c in i18n::bot_commands(lang) {
                assert!(
                    !c.description.is_empty(),
                    "/{} has an empty description in {lang}",
                    c.command
                );
                // A missing key would surface as the raw key itself.
                assert!(
                    !c.description.starts_with("cmd."),
                    "/{} fell back to its key in {lang}: {}",
                    c.command,
                    c.description
                );
            }
        }
        // The locales are genuinely distinct, not the English fallback.
        assert_ne!(
            i18n::bot_commands("en")[0].description,
            i18n::bot_commands("fr")[0].description
        );
    }

    // ---- Handler-level tests against a hand-rolled mock Bot API ---------
    //
    // The handlers do real Telegram I/O, so exercising them end-to-end
    // needs a server. Rather than pull an HTTP-server dependency
    // (actix-web & co.) into a deliberately dependency-light project, this
    // reuses the same trick as `src/health.rs`: a small tokio `TcpListener`
    // that speaks just enough HTTP/1.1 to record requests and answer them.

    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use teloxide::adaptors::throttle::Limits;
    use teloxide::requests::RequesterExt;
    use teloxide::types::{CallbackQuery, CallbackQueryId, ChatId, MaybeInaccessibleMessage, User};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct MockTelegram {
        base_url: String,
        requests: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }

    impl MockTelegram {
        /// Every `sendMessage` body, in order.
        fn sent_texts(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(method, _)| method == "sendmessage")
                .filter_map(|(_, body)| {
                    body.get("text")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                })
                .collect()
        }

        fn last_text(&self) -> String {
            self.sent_texts()
                .pop()
                .expect("the handler under test must send at least one message")
        }

        fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(method, _)| method.clone())
                .collect()
        }

        fn last_body(&self, method: &str) -> Option<serde_json::Value> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(m, _)| m == method)
                .map(|(_, body)| body.clone())
        }

        /// Forgets everything recorded so far - used to isolate a phase of a
        /// test from its setup traffic.
        fn clear(&self) {
            self.requests.lock().unwrap().clear();
        }

        fn bot(&self) -> Bot {
            teloxide::Bot::new("123456:TEST_TOKEN")
                .set_api_url(url::Url::parse(&self.base_url).unwrap())
                .throttle(Limits::default())
        }
    }

    async fn spawn_mock_telegram() -> MockTelegram {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let sink = sink.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let mut header_end = None;
                    loop {
                        let n = match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&chunk[..n]);
                        if header_end.is_none() {
                            header_end =
                                raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                        }
                        if let Some(end) = header_end
                            && raw.len() >= end + content_length(&raw[..end])
                        {
                            break;
                        }
                    }

                    let (method, body) = parse_request(&raw);
                    let json: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    sink.lock().unwrap().push((method.clone(), json));

                    // `SendMessage`/`EditMessageReplyMarkup` must answer with
                    // a deserializable Message; every other method the
                    // handlers call only checks the response is `ok`.
                    let result = if method == "sendmessage" || method == "editmessagereplymarkup" {
                        serde_json::json!({
                            "message_id": 1,
                            "date": 0,
                            "chat": {"id": 42, "type": "private"},
                            "text": "echo",
                        })
                    } else {
                        serde_json::json!(true)
                    };
                    let payload = serde_json::json!({"ok": true, "result": result}).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        MockTelegram {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    fn content_length(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }

    /// Pulls the method out of `POST /bot<token>/<Method> HTTP/1.1` and the
    /// body out of everything after the header block. teloxide names the
    /// methods in PascalCase (`SendMessage`), so they're lowercased here for
    /// easy comparison against the recorded request.
    fn parse_request(raw: &[u8]) -> (String, Vec<u8>) {
        let text = String::from_utf8_lossy(raw);
        let method = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|path| path.rsplit('/').next())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let body = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(pos) => raw[pos + 4..].to_vec(),
            None => Vec::new(),
        };
        (method.to_string(), body)
    }

    /// A stored account pointing at a loopback port nothing listens on, so
    /// any JMAP call fails fast (connection refused) instead of hanging on
    /// DNS/network - the error paths are what several of these tests exercise.
    fn account(email: &str, last_state: Option<&str>) -> Account {
        let mut accounts = std::collections::BTreeMap::new();
        accounts.insert(
            "default".to_string(),
            AccountSlot {
                slot_id: "default".to_string(),
                server_url: "https://127.0.0.1:9".to_string(),
                token: "tok".to_string(),
                email: email.to_string(),
                last_state: last_state.map(str::to_string),
                thread_id: None,
            },
        );
        Account {
            language: None,
            shared_accounts: HashMap::new(),
            accounts,
            muted: Vec::new(),
            threads_enabled: None,
        }
    }

    fn state(data_dir: &Path, chat_id: i64, with_account: bool) -> AppState {
        let config = Arc::new(crate::config::Config::for_testing(data_dir, chat_id));
        let store = Arc::new(crate::store::Store::open(data_dir).unwrap());
        if with_account {
            store
                .set(chat_id, account("user@example.org", Some("s1")))
                .unwrap();
        }
        AppState::new(config, store)
    }

    #[tokio::test]
    async fn cmd_lang_persists_the_choice_and_confirms_in_the_new_language() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_lang(&bot, &state, ChatId(42), "fr".to_string(), "en")
            .await
            .unwrap();
        assert_eq!(state.store.get(42).unwrap().language.as_deref(), Some("fr"));
        assert_eq!(mock.last_text(), "✅ Langue mise à jour.");

        // An unsupported value is refused and leaves the stored choice alone.
        cmd_lang(&bot, &state, ChatId(42), "de".to_string(), "fr")
            .await
            .unwrap();
        assert_eq!(
            mock.last_text(),
            "Langue non prise en charge. Disponibles : en, fr."
        );
        assert_eq!(state.store.get(42).unwrap().language.as_deref(), Some("fr"));
    }

    #[tokio::test]
    async fn cmd_lang_without_a_choice_shows_usage_in_the_current_language() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_lang(&bot, &state, ChatId(42), "  ".to_string(), "en")
            .await
            .unwrap();
        assert_eq!(mock.last_text(), "Usage: /lang <en|fr> (current: en)");
    }

    #[tokio::test]
    async fn cmd_start_without_an_account_sends_the_localized_welcome() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 7, false);

        cmd_start(&bot, &state, ChatId(7), "en").await.unwrap();
        let text = mock.last_text();
        assert!(text.contains("Welcome"), "got: {text}");
        assert!(text.contains("/login <server_url> <token>"));

        // With no account, status is unavailable - the same helper reports
        // it in the requested locale.
        cmd_status(&bot, &state, ChatId(7), "fr").await.unwrap();
        assert_eq!(
            mock.last_text(),
            "Aucun compte connecté. Tape /login pour commencer."
        );
    }

    #[tokio::test]
    async fn cmd_status_reports_the_localized_header_and_state() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        // No watcher registered -> "inactive", and last_state is set.
        cmd_status(&bot, &state, ChatId(42), "en").await.unwrap();
        let text = mock.last_text();
        assert!(
            text.contains("Connected as user@example.org"),
            "got: {text}"
        );
        assert!(text.contains("Watching: inactive"), "got: {text}");
        assert!(text.contains("State: in sync"), "got: {text}");
        // No shared/extra accounts or filters -> no extra lines.
        assert!(!text.contains("/shared"));
        assert!(!text.contains("/accounts"));
    }

    #[tokio::test]
    async fn cmd_mute_adds_lists_and_reports_duplicates() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_mute(&bot, &state, ChatId(42), String::new(), "en")
            .await
            .unwrap();
        assert_eq!(
            mock.last_text(),
            "No active filter. /mute <sender or keyword> to add one."
        );

        cmd_mute(&bot, &state, ChatId(42), "newsletter".to_string(), "en")
            .await
            .unwrap();
        assert!(mock.last_text().contains("is now filtered"));

        cmd_mute(&bot, &state, ChatId(42), "newsletter".to_string(), "en")
            .await
            .unwrap();
        assert_eq!(mock.last_text(), "« newsletter » is already filtered.");

        cmd_mute(&bot, &state, ChatId(42), String::new(), "fr")
            .await
            .unwrap();
        assert!(mock.last_text().contains("newsletter"));
    }

    #[tokio::test]
    async fn cmd_unmute_removes_and_reports() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_unmute(&bot, &state, ChatId(42), String::new(), "en")
            .await
            .unwrap();
        assert!(mock.last_text().starts_with("Usage: /unmute"));

        cmd_unmute(&bot, &state, ChatId(42), "ghost".to_string(), "en")
            .await
            .unwrap();
        assert_eq!(mock.last_text(), "« ghost » was not filtered.");

        state.store.mute(42, "keep").unwrap();
        cmd_unmute(&bot, &state, ChatId(42), "keep".to_string(), "en")
            .await
            .unwrap();
        assert_eq!(mock.last_text(), "🔔 « keep » is no longer filtered.");
    }

    #[tokio::test]
    async fn cmd_logout_reports_then_reports_nothing_left() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_logout(&bot, &state, ChatId(42), "fr").await.unwrap();
        assert_eq!(
            mock.last_text(),
            "🗑 Identifiants et données supprimés. Tape /login pour te reconnecter."
        );
        assert!(state.store.get(42).is_none());

        cmd_logout(&bot, &state, ChatId(42), "fr").await.unwrap();
        assert_eq!(mock.last_text(), "Aucun compte connecté.");
    }

    /// A `CallbackQuery` as Telegram would deliver it, wrapping a real
    /// `Message` obtained from the mock. Built as a struct literal (not from
    /// JSON): `MaybeInaccessibleMessage` is an untagged enum that a minimal
    /// JSON body always resolves to the date-less `Inaccessible` variant,
    /// so `regular_message()` would spuriously return `None`.
    async fn callback_query(bot: &Bot, data: &str, language_code: &str) -> CallbackQuery {
        let message = bot.send_message(ChatId(42), "seed").await.unwrap();
        let from: User = serde_json::from_value(serde_json::json!({
            "id": 1, "is_bot": false, "first_name": "T", "language_code": language_code,
        }))
        .unwrap();
        CallbackQuery {
            id: CallbackQueryId("1".to_string()),
            from,
            message: Some(MaybeInaccessibleMessage::Regular(Box::new(message))),
            inline_message_id: None,
            chat_instance: "ci".to_string(),
            data: Some(data.to_string()),
            game_short_name: None,
        }
    }

    fn first_row_labels(markup: &serde_json::Value) -> Vec<String> {
        markup["reply_markup"]["inline_keyboard"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn callback_expand_is_pure_presentation_and_follows_the_locale() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        // "e" = expand an unread message; the reader's client says fr.
        let q = callback_query(&bot, "e:M123", "fr").await;
        mock.clear();
        callback_handler(bot.clone(), q, state).await.unwrap();

        let methods = mock.methods();
        assert!(methods.contains(&"answercallbackquery".to_string()));
        assert!(methods.contains(&"editmessagereplymarkup".to_string()));
        // Presentation only: no JMAP client is resolved, so nothing is sent
        // as a new message and the account's connectivity is irrelevant.
        assert!(!methods.contains(&"sendmessage".to_string()));

        let markup = mock.last_body("editmessagereplymarkup").unwrap();
        assert_eq!(
            first_row_labels(&markup),
            vec!["📖 Lire tout", "✅ Lu"],
            "expanded menu should be in French"
        );
    }

    #[tokio::test]
    async fn callback_collapse_sends_the_collapsed_english_menu() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        // "C" = collapse a message already marked read; English client.
        let q = callback_query(&bot, "C:M123", "en").await;
        mock.clear();
        callback_handler(bot, q, state).await.unwrap();

        let markup = mock.last_body("editmessagereplymarkup").unwrap();
        assert_eq!(
            first_row_labels(&markup),
            vec!["📥 Archive", "🗑 Delete", "⋯ More"]
        );
        // The toggle case must carry the read state back, else re-expanding
        // would render an actionable "Read" for a message already read.
        let data = markup["reply_markup"]["inline_keyboard"][0][2]["callback_data"]
            .as_str()
            .unwrap();
        assert_eq!(data, "E:M123");
    }

    fn text_message(chat_id: i64, text: &str, language_code: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "message_id": 9,
            "date": 0,
            "chat": {"id": chat_id, "type": "private"},
            "from": {"id": 1, "is_bot": false, "first_name": "T", "language_code": language_code},
            "text": text,
        }))
        .unwrap()
    }

    fn me() -> Me {
        serde_json::from_value(serde_json::json!({
            "id": 1, "is_bot": true, "first_name": "bot", "username": "testbot",
            "can_join_groups": false, "can_read_all_group_messages": false,
            "supports_inline_queries": false, "has_main_web_app": false,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn message_handler_answers_unknown_command_with_a_localized_hint() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        message_handler(
            bot,
            text_message(42, "just some text, not a command", "en"),
            me(),
            state,
        )
        .await
        .unwrap();
        assert_eq!(
            mock.last_text(),
            "Unknown command. Type /help for the list of commands."
        );
    }

    #[tokio::test]
    async fn message_handler_answers_help_in_the_readers_language() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        // No stored preference and a French client => French help.
        message_handler(bot, text_message(42, "/help", "fr"), me(), state)
            .await
            .unwrap();
        let text = mock.last_text();
        assert!(text.contains("/login"), "got: {text}");
        assert!(text.contains("/lang"), "got: {text}");
        assert!(text.contains("/search"), "got: {text}");
        // French, not the English fallback.
        assert!(text.contains("démarrer"), "got: {text}");
    }

    #[tokio::test]
    async fn message_handler_refuses_an_unauthorized_chat_in_its_language() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        // Authorized list only contains 42.
        let state = state(dir.path(), 42, true);

        message_handler(bot, text_message(99, "/status", "fr"), me(), state)
            .await
            .unwrap();
        assert_eq!(
            mock.last_text(),
            "⛔ Accès non autorisé. Ce bot est réservé à des utilisateurs autorisés."
        );
    }

    #[tokio::test]
    async fn message_handler_routes_lang_and_persists_the_preference() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        message_handler(bot, text_message(42, "/lang fr", "en"), me(), state.clone())
            .await
            .unwrap();
        assert_eq!(state.store.get(42).unwrap().language.as_deref(), Some("fr"));
        assert_eq!(mock.last_text(), "✅ Langue mise à jour.");
    }

    #[tokio::test]
    async fn message_handler_start_onboards_then_reports_status() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();

        // No account yet -> onboarding text.
        let fresh = state(dir.path(), 7, false);
        message_handler(bot.clone(), text_message(7, "/start", "en"), me(), fresh)
            .await
            .unwrap();
        assert!(mock.last_text().contains("Welcome"), "{}", mock.last_text());

        // Already connected -> /start delegates to /status.
        let connected = state(dir.path(), 42, true);
        message_handler(bot, text_message(42, "/start", "fr"), me(), connected)
            .await
            .unwrap();
        assert!(
            mock.last_text()
                .contains("Connecté en tant que user@example.org"),
            "{}",
            mock.last_text()
        );
    }

    #[tokio::test]
    async fn message_handler_routes_status_mute_and_unmute() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        message_handler(
            bot.clone(),
            text_message(42, "/status", "en"),
            me(),
            state.clone(),
        )
        .await
        .unwrap();
        assert!(mock.last_text().contains("Connected as user@example.org"));

        message_handler(
            bot.clone(),
            text_message(42, "/mute", "en"),
            me(),
            state.clone(),
        )
        .await
        .unwrap();
        assert!(mock.last_text().contains("No active filter"));

        message_handler(bot, text_message(42, "/unmute ghost", "en"), me(), state)
            .await
            .unwrap();
        assert_eq!(mock.last_text(), "« ghost » was not filtered.");
    }

    #[tokio::test]
    async fn message_handler_routes_search_to_a_localized_connection_error() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        // The account is unreachable, so /search fails at client resolution
        // and reports the localized, generic connection error.
        message_handler(bot, text_message(42, "/search invoice", "fr"), me(), state)
            .await
            .unwrap();
        let text = mock.last_text();
        assert!(text.starts_with("Erreur de connexion"), "got: {text}");
        assert!(text.contains("injoignable"), "got: {text}");
    }

    #[tokio::test]
    async fn cmd_status_summarizes_shared_extra_accounts_and_filters() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        state
            .store
            .set_shared_account(
                42,
                "acc7".to_string(),
                "Shared".to_string(),
                Some("s".to_string()),
                None,
            )
            .unwrap();
        state
            .store
            .add_account(
                42,
                AccountSlot {
                    slot_id: "slot1".to_string(),
                    server_url: "https://127.0.0.1:9".to_string(),
                    token: "t".to_string(),
                    email: "extra@b.c".to_string(),
                    last_state: Some("s".to_string()),
                    thread_id: None,
                },
            )
            .unwrap();
        state.store.mute(42, "newsletter").unwrap();
        // Drop the sync marker to exercise the "initializing" branch too.
        let mut stored = state.store.get(42).unwrap();
        stored.primary_slot_mut().unwrap().last_state = None;
        state.store.set(42, stored).unwrap();

        cmd_status(&bot, &state, ChatId(42), "en").await.unwrap();
        let text = mock.last_text();
        assert!(text.contains("State: initializing"), "got: {text}");
        assert!(text.contains("Active shared mailboxes: 1"), "got: {text}");
        assert!(text.contains("Additional accounts: 1"), "got: {text}");
        assert!(text.contains("Active filters: 1"), "got: {text}");
    }

    #[tokio::test]
    async fn list_extra_accounts_numbers_them_and_offers_a_disconnect_button() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);
        for email in ["b@example.org", "a@example.org"] {
            state
                .store
                .add_account(
                    42,
                    AccountSlot {
                        slot_id: String::new(),
                        server_url: "https://127.0.0.1:9".to_string(),
                        token: "t".to_string(),
                        email: email.to_string(),
                        last_state: Some("s".to_string()),
                        thread_id: None,
                    },
                )
                .unwrap();
        }

        // Bare /accounts lists rather than erroring.
        cmd_comptes(&bot, &state, ChatId(42), String::new(), "en")
            .await
            .unwrap();
        let text = mock.last_text();
        assert!(text.contains("Connected accounts:"), "got: {text}");
        // Primary first, extras address-sorted - stable numbering.
        assert!(text.contains("1. user@example.org"), "got: {text}");
        assert!(text.contains("2. a@example.org"), "got: {text}");
        assert!(text.contains("3. b@example.org"), "got: {text}");
        assert!(text.contains("🗑 = disconnect this account"), "got: {text}");

        let body = mock.last_body("sendmessage").unwrap();
        let labels: Vec<String> = body["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[0]["text"].as_str().unwrap().to_string())
            .collect();
        assert!(
            labels.contains(&"🗑 a@example.org".to_string()),
            "{labels:?}"
        );
        assert!(
            labels.contains(&"🗑 b@example.org".to_string()),
            "{labels:?}"
        );
    }

    #[tokio::test]
    async fn cmd_comptes_guides_the_user_through_its_error_cases() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();

        // Missing primary account.
        let fresh = state(dir.path(), 7, false);
        cmd_comptes(&bot, &fresh, ChatId(7), "a b".to_string(), "en")
            .await
            .unwrap();
        assert_eq!(
            mock.last_text(),
            "Connect your account with /login before adding another one."
        );

        // Malformed arguments (no token).
        let connected = state(dir.path(), 42, true);
        cmd_comptes(
            &bot,
            &connected,
            ChatId(42),
            "onlyoneword".to_string(),
            "en",
        )
        .await
        .unwrap();
        assert!(
            mock.last_text().starts_with("Usage: /accounts"),
            "{}",
            mock.last_text()
        );

        // Unreachable server -> localized, generic connection error.
        cmd_comptes(
            &bot,
            &connected,
            ChatId(42),
            "https://127.0.0.1:9 tok".to_string(),
            "fr",
        )
        .await
        .unwrap();
        assert!(
            mock.last_text().starts_with("❌ Connexion impossible"),
            "{}",
            mock.last_text()
        );
    }

    #[tokio::test]
    async fn cmd_partages_reports_an_unreachable_account() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_partages(&bot, &state, ChatId(42), "en").await.unwrap();
        assert!(
            mock.last_text().starts_with("❌ Connection failed"),
            "{}",
            mock.last_text()
        );
    }

    #[tokio::test]
    async fn cmd_rechercher_requires_a_query_before_touching_the_network() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_rechercher(&bot, &state, ChatId(42), "   ".to_string(), "en")
            .await
            .unwrap();
        assert_eq!(mock.last_text(), "Usage: /search <text>");
    }

    #[tokio::test]
    async fn callback_email_action_reports_the_localized_connection_error() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        let q = callback_query(&bot, "a:M123", "en").await;
        mock.clear();
        callback_handler(bot, q, state).await.unwrap();

        // Account unreachable -> the tap is answered with an alert, not left
        // spinning.
        let body = mock.last_body("answercallbackquery").unwrap();
        assert!(
            body["text"]
                .as_str()
                .unwrap()
                .starts_with("Connection error"),
            "{body}"
        );
        assert_eq!(body["show_alert"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn disconnect_extra_account_removes_child_shared_accounts() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        let slot_id = state
            .store
            .add_account(
                42,
                AccountSlot {
                    slot_id: "extra_slot".to_string(),
                    server_url: "https://127.0.0.1:9".to_string(),
                    token: "extra-tok".to_string(),
                    email: "extra@example.org".to_string(),
                    last_state: Some("s".to_string()),
                    thread_id: None,
                },
            )
            .unwrap();

        state
            .store
            .set_shared_account(
                42,
                "shared_extra_1".to_string(),
                "Shared Extra".to_string(),
                Some("s".to_string()),
                Some(slot_id.clone()),
            )
            .unwrap();

        assert_eq!(state.store.get(42).unwrap().shared_accounts.len(), 1);

        let q = callback_query(&bot, &format!("x:{slot_id}"), "en").await;
        mock.clear();
        callback_handler(bot, q, state.clone()).await.unwrap();

        let account = state.store.get(42).unwrap();
        assert_eq!(account.accounts.len(), 1);
        assert!(!account.accounts.contains_key(&slot_id));
        assert!(
            account.shared_accounts.is_empty(),
            "child shared account should be removed on extra account disconnect"
        );
    }

    #[tokio::test]
    async fn toggle_shared_account_disables_active_extra_shared_account() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        state
            .store
            .set_shared_account(
                42,
                "shared_extra_2".to_string(),
                "Shared 2".to_string(),
                Some("s".to_string()),
                Some("slot99".to_string()),
            )
            .unwrap();

        assert_eq!(state.store.get(42).unwrap().shared_accounts.len(), 1);

        let q = callback_query(&bot, "s:slot99:shared_extra_2", "en").await;
        mock.clear();
        callback_handler(bot, q, state.clone()).await.unwrap();

        let account = state.store.get(42).unwrap();
        assert!(
            account.shared_accounts.is_empty(),
            "tapping active shared account should disable and remove it"
        );
        let body = mock.last_body("answercallbackquery").unwrap();
        assert_eq!(
            body["text"],
            serde_json::json!("🔕 Notifications disabled.")
        );
    }

    #[tokio::test]
    async fn disconnect_primary_account_preserves_secondary_account() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true); // has "default" slot user@example.org

        state
            .store
            .add_account(
                42,
                AccountSlot {
                    slot_id: "second_slot".to_string(),
                    server_url: "https://127.0.0.1:9".to_string(),
                    token: "tok2".to_string(),
                    email: "second@example.org".to_string(),
                    last_state: Some("s2".to_string()),
                    thread_id: None,
                },
            )
            .unwrap();

        state
            .store
            .set_shared_account(
                42,
                "shared_primary".to_string(),
                "shared@example.org".to_string(),
                None,
                None, // parent_slot None belongs to default
            )
            .unwrap();

        assert_eq!(state.store.get(42).unwrap().accounts.len(), 2);
        assert_eq!(state.store.get(42).unwrap().shared_accounts.len(), 1);

        // Disconnecting the first ("primary") account via x:default
        let q = callback_query(&bot, "x:default", "en").await;
        mock.clear();
        callback_handler(bot, q, state.clone()).await.unwrap();

        let account = state.store.get(42).expect("chat must survive");
        assert_eq!(account.accounts.len(), 1);
        assert!(!account.accounts.contains_key("default"));
        assert!(account.accounts.contains_key("second_slot"));
        assert_eq!(account.email(), "second@example.org");
        assert!(
            account.shared_accounts.is_empty(),
            "child shared mailbox of default account must be removed"
        );
    }

    #[tokio::test]
    async fn cmd_logout_with_multiple_accounts_prompts_choice_and_disconnects_all() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        state
            .store
            .add_account(
                42,
                AccountSlot {
                    slot_id: "second_slot".to_string(),
                    server_url: "https://127.0.0.1:9".to_string(),
                    token: "tok2".to_string(),
                    email: "second@example.org".to_string(),
                    last_state: Some("s2".to_string()),
                    thread_id: None,
                },
            )
            .unwrap();

        // /logout when multiple accounts are connected prompts with inline keyboard
        cmd_logout(&bot, &state, ChatId(42), "en").await.unwrap();
        let text = mock.last_text();
        assert!(
            text.contains("Choose an account to disconnect:"),
            "got: {text}"
        );

        let body = mock.last_body("sendmessage").unwrap();
        let markup = &body["reply_markup"]["inline_keyboard"];
        assert_eq!(markup.as_array().unwrap().len(), 3); // 2 accounts + Disconnect all

        // Tap "Disconnect all"
        let q = callback_query(&bot, "logout:all", "en").await;
        mock.clear();
        callback_handler(bot, q, state.clone()).await.unwrap();

        assert!(
            state.store.get(42).is_none(),
            "store should be cleared after logout all"
        );
        let text = mock.last_text();
        assert!(text.contains("Credentials and data deleted"), "got: {text}");
    }

    #[tokio::test]
    async fn cmd_threads_without_account_reports_no_account() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, false);

        cmd_threads(&bot, &state, ChatId(42), "", "en")
            .await
            .unwrap();
        let text = mock.last_text();
        assert!(text.contains("No account connected"), "got: {text}");
    }

    #[tokio::test]
    async fn cmd_threads_bare_shows_overview_and_buttons() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_threads(&bot, &state, ChatId(42), "", "en")
            .await
            .unwrap();
        let text = mock.last_text();
        assert!(text.contains("Threaded Conversations"), "got: {text}");
        let body = mock.last_body("sendmessage").unwrap();
        let markup = &body["reply_markup"]["inline_keyboard"];
        assert_eq!(markup.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cmd_threads_off_disables_threads_mode() {
        let mock = spawn_mock_telegram().await;
        let bot = mock.bot();
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path(), 42, true);

        cmd_threads(&bot, &state, ChatId(42), "off", "en")
            .await
            .unwrap();
        assert_eq!(state.store.get(42).unwrap().threads_enabled, Some(false));
        let text = mock.last_text();
        assert!(text.contains("Threaded mode disabled"), "got: {text}");
    }
}
