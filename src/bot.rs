use std::collections::{HashMap, HashSet};
use std::time::Instant;

use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQueryId, ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, Me, MessageId,
    ParseMode,
};
use teloxide::utils::command::BotCommands;

use crate::Bot;
use crate::accounts;
use crate::format::{self, TELEGRAM_MAX_MESSAGE_LEN, chunk_text};
use crate::jmap;
use crate::state::{AppState, PendingUndo, UNDO_WINDOW};
use crate::store::{Account, ExtraAccount};

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Repeated verbatim across every command that needs a primary account and
/// doesn't have one — one string, so it can't drift between call sites.
const NO_ACCOUNT_MSG: &str = "Aucun compte connecté. Tape /login pour commencer.";

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "démarrer / voir le statut de connexion")]
    Start,
    #[command(
        description = "connecter un compte JMAP : /login <server_url> <token>",
        parse_with = "split"
    )]
    Login { server_url: String, token: String },
    #[command(description = "supprimer les identifiants stockés (droit à l'effacement RGPD)")]
    Logout,
    #[command(description = "statut du compte connecté")]
    Status,
    #[command(description = "gérer les notifications des boîtes JMAP partagées")]
    Partages,
    #[command(
        description = "compte perso supplémentaire : /comptes <server_url> <token> pour en connecter un (sans argument : liste ceux déjà connectés)"
    )]
    Comptes { args: String },
    #[command(
        description = "filtrer un expéditeur/mot-clé : /mute <terme> (sans argument : liste les filtres actifs)"
    )]
    Mute { term: String },
    #[command(description = "annule un filtre : /unmute <terme>")]
    Unmute { term: String },
    #[command(description = "recherche plein texte dans la boîte : /rechercher <texte>")]
    Rechercher { query: String },
    #[command(description = "afficher l'aide")]
    Help,
}

pub fn schema() -> UpdateHandler<Box<dyn std::error::Error + Send + Sync + 'static>> {
    dptree::entry()
        .branch(Update::filter_message().endpoint(message_handler))
        .branch(Update::filter_callback_query().endpoint(callback_handler))
}

async fn message_handler(bot: Bot, msg: Message, me: Me, state: AppState) -> HandlerResult {
    let chat_id = msg.chat.id;

    // Every stored JMAP account is fully controlled (read, archive,
    // delete) by whoever can talk in its chat. That's a reasonable model
    // for a 1:1 DM, but silently extends to every member of a group if an
    // operator ever puts a group chat id in AUTHORIZED_CHAT_IDS — so
    // refuse non-private chats outright rather than let that happen.
    if !msg.chat.is_private() {
        return Ok(());
    }

    if !state.config.authorized_chat_ids.contains(&chat_id) {
        tracing::warn!(chat_id = chat_id.0, "unauthorized access attempt");
        bot.send_message(
            chat_id,
            "⛔ Accès non autorisé. Ce bot est réservé à des utilisateurs autorisés.",
        )
        .await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };

    match BotCommands::parse(text, me.username()) {
        Ok(Command::Start) => cmd_start(&bot, &state, chat_id).await?,
        Ok(Command::Help) => {
            bot.send_message(chat_id, Command::descriptions().to_string())
                .await?;
        }
        Ok(Command::Login { server_url, token }) => {
            // The token is sensitive: remove it from the chat history as
            // soon as we've read it, regardless of outcome. If Telegram
            // refuses the deletion (e.g. the bot lacks rights in this
            // chat), the token is left sitting in plain sight, so make
            // sure the user actually finds out instead of trusting a
            // promise the bot couldn't keep.
            if bot.delete_message(chat_id, msg.id).await.is_err() {
                bot.send_message(
                    chat_id,
                    "⚠️ Je n'ai pas pu supprimer ton message /login. Le jeton reste visible \
                     dans l'historique : supprime-le manuellement et envisage de le révoquer \
                     et d'en générer un nouveau chez ton fournisseur JMAP.",
                )
                .await?;
            }
            cmd_login(&bot, &state, chat_id, server_url, token).await?;
        }
        Ok(Command::Logout) => cmd_logout(&bot, &state, chat_id).await?,
        Ok(Command::Status) => cmd_status(&bot, &state, chat_id).await?,
        Ok(Command::Partages) => cmd_partages(&bot, &state, chat_id).await?,
        Ok(Command::Comptes { args }) => {
            // Same as /login: when this carries a token (i.e. it's not the
            // bare "list what's connected" form), delete it from the chat
            // history right after reading it.
            if !args.trim().is_empty() && bot.delete_message(chat_id, msg.id).await.is_err() {
                bot.send_message(
                    chat_id,
                    "⚠️ Je n'ai pas pu supprimer ton message /comptes. Le jeton reste visible \
                     dans l'historique : supprime-le manuellement et envisage de le révoquer \
                     et d'en générer un nouveau chez ton fournisseur JMAP.",
                )
                .await?;
            }
            cmd_comptes(&bot, &state, chat_id, args).await?
        }
        Ok(Command::Mute { term }) => cmd_mute(&bot, &state, chat_id, term).await?,
        Ok(Command::Unmute { term }) => cmd_unmute(&bot, &state, chat_id, term).await?,
        Ok(Command::Rechercher { query }) => cmd_rechercher(&bot, &state, chat_id, query).await?,
        Err(_) => {
            bot.send_message(
                chat_id,
                "Commande inconnue. Tape /help pour la liste des commandes.",
            )
            .await?;
        }
    }

    Ok(())
}

async fn cmd_start(bot: &Bot, state: &AppState, chat_id: ChatId) -> HandlerResult {
    if state.store.get(chat_id.0).is_some() {
        cmd_status(bot, state, chat_id).await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        "👋 Bienvenue.\n\n\
         Pour recevoir tes mails ici, connecte ton compte JMAP avec :\n\
         /login <server_url> <token>\n\n\
         • server_url est l'URL racine du serveur JMAP de ton fournisseur \
           (ex. Fastmail : https://jmap.fastmail.com) — le bot découvre \
           automatiquement le point d'entrée via /.well-known/jmap.\n\
         • token est un jeton d'API (Bearer), pas ton mot de passe — \
           génère-le dans les paramètres de sécurité de ton fournisseur.\n\n\
         Ce message /login sera automatiquement supprimé du chat juste après, \
         pour ne pas laisser le jeton traîner dans l'historique.",
    )
    .await?;
    Ok(())
}

async fn cmd_login(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    server_url: String,
    token: String,
) -> HandlerResult {
    let client =
        match jmap::connect(&server_url, &token, state.config.allow_private_jmap_hosts).await {
            Ok(c) => c,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Connexion impossible : {e}"))
                    .await?;
                return Ok(());
            }
        };

    let email = jmap::account_email(&client);
    let initial_state = match jmap::current_email_state(&client).await {
        Ok(s) => s,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Erreur lors de l'initialisation : {e}"))
                .await?;
            return Ok(());
        }
    };

    let account = Account {
        server_url,
        token,
        email: email.clone(),
        last_state: Some(initial_state),
        shared_accounts: HashMap::new(),
        extra_accounts: HashMap::new(),
        muted: Vec::new(),
    };

    if let Err(e) = state.store.set(chat_id.0, account) {
        bot.send_message(chat_id, format!("❌ Erreur de stockage : {e}"))
            .await?;
        return Ok(());
    }

    // Re-login while already connected must not leak the previous
    // watcher: without this, the old task's JoinHandle is silently
    // overwritten below and keeps running forever with its own JMAP
    // connection, since its only exit check (the store entry existing)
    // stays true after a reconnect. This is safe against a concurrent
    // /login (or a button tap racing this one) for the *same* chat only
    // because teloxide's default distribution function serializes all
    // updates for a given chat id onto one worker (see
    // `teloxide::dispatching::distribution`); this handler never sets
    // `.distribution_function(...)` on the dispatcher, so don't add one
    // without re-checking this invariant.
    state.forget(chat_id.0).await;
    accounts::adopt_primary(bot, state, chat_id.0, client).await;

    bot.send_message(
        chat_id,
        format!(
            "✅ Connecté en tant que {email}. Tu recevras une notification à chaque nouveau mail."
        ),
    )
    .await?;
    Ok(())
}

async fn cmd_logout(bot: &Bot, state: &AppState, chat_id: ChatId) -> HandlerResult {
    state.forget(chat_id.0).await;
    let removed = state.store.remove(chat_id.0).unwrap_or(false);

    let text = if removed {
        "🗑 Identifiants et données supprimés. Tape /login pour te reconnecter."
    } else {
        "Aucun compte connecté."
    };
    bot.send_message(chat_id, text).await?;
    Ok(())
}

async fn cmd_status(bot: &Bot, state: &AppState, chat_id: ChatId) -> HandlerResult {
    let Some(account) = state.store.get(chat_id.0) else {
        bot.send_message(chat_id, NO_ACCOUNT_MSG).await?;
        return Ok(());
    };

    let watching = state.watchers.read().await.contains_key(&chat_id.0);
    let sync_state = if account.last_state.is_some() {
        "synchronisé"
    } else {
        "initialisation…"
    };

    let mut text = format!(
        "📬 Connecté en tant que {}\nSurveillance : {}\nÉtat : {}",
        account.email,
        if watching {
            "active ✅"
        } else {
            "inactive ⚠️ (tape /login à nouveau)"
        },
        sync_state,
    );
    if !account.shared_accounts.is_empty() {
        text.push_str(&format!(
            "\nBoîtes partagées actives : {} (/partages)",
            account.shared_accounts.len()
        ));
    }
    if !account.extra_accounts.is_empty() {
        text.push_str(&format!(
            "\nComptes supplémentaires : {} (/comptes)",
            account.extra_accounts.len()
        ));
    }
    if !account.muted.is_empty() {
        text.push_str(&format!(
            "\nFiltres actifs : {} (/mute)",
            account.muted.len()
        ));
    }

    bot.send_message(chat_id, text).await?;
    Ok(())
}

/// `/comptes` (no args) lists the chat's primary account plus every extra,
/// fully independent personal account connected via `/comptes <url>
/// <token>` — distinct from `/partages`' delegated/shared accounts, which
/// live under the primary token rather than having their own credentials.
async fn cmd_comptes(bot: &Bot, state: &AppState, chat_id: ChatId, args: String) -> HandlerResult {
    let args = args.trim();
    if args.is_empty() {
        return list_extra_accounts(bot, state, chat_id).await;
    }

    let Some(account) = state.store.get(chat_id.0) else {
        bot.send_message(
            chat_id,
            "Connecte d'abord ton compte principal avec /login avant d'en ajouter un second.",
        )
        .await?;
        return Ok(());
    };

    let Some((server_url, token)) = args.split_once(char::is_whitespace) else {
        bot.send_message(
            chat_id,
            "Usage : /comptes <server_url> <token> — comme /login, mais pour un compte \
             supplémentaire plutôt que de remplacer le principal.",
        )
        .await?;
        return Ok(());
    };
    let (server_url, token) = (server_url.trim().to_string(), token.trim().to_string());

    let client =
        match jmap::connect(&server_url, &token, state.config.allow_private_jmap_hosts).await {
            Ok(c) => c,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Connexion impossible : {e}"))
                    .await?;
                return Ok(());
            }
        };

    let email = jmap::account_email(&client);
    if email == account.email
        || account
            .extra_accounts
            .values()
            .any(|extra| extra.email == email)
    {
        bot.send_message(chat_id, format!("{email} est déjà connecté à ce chat."))
            .await?;
        return Ok(());
    }

    let initial_state = match jmap::current_email_state(&client).await {
        Ok(s) => s,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Erreur lors de l'initialisation : {e}"))
                .await?;
            return Ok(());
        }
    };

    let slot_id = match state.store.add_extra_account(
        chat_id.0,
        ExtraAccount {
            server_url,
            token,
            email: email.clone(),
            last_state: Some(initial_state),
        },
    ) {
        Ok(Some(id)) => id,
        Ok(None) => {
            bot.send_message(
                chat_id,
                "Connecte d'abord ton compte principal avec /login avant d'en ajouter un second.",
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Erreur de stockage : {e}"))
                .await?;
            return Ok(());
        }
    };

    accounts::adopt_extra(bot, state, chat_id.0, slot_id, email.clone(), client).await;

    bot.send_message(
        chat_id,
        format!(
            "✅ {email} connecté comme compte supplémentaire. Tape /comptes pour voir la liste."
        ),
    )
    .await?;
    Ok(())
}

async fn list_extra_accounts(bot: &Bot, state: &AppState, chat_id: ChatId) -> HandlerResult {
    let Some(account) = state.store.get(chat_id.0) else {
        bot.send_message(chat_id, NO_ACCOUNT_MSG).await?;
        return Ok(());
    };

    let mut lines = vec![format!("• {} (principal)", account.email)];
    let mut buttons = Vec::new();
    for (slot_id, extra) in &account.extra_accounts {
        lines.push(format!("• {}", extra.email));
        buttons.push(vec![InlineKeyboardButton::callback(
            format!("🗑 Déconnecter {}", extra.email),
            format!("x:{slot_id}"),
        )]);
    }

    let mut text = format!("📧 Comptes connectés :\n{}", lines.join("\n"));
    if account.extra_accounts.is_empty() {
        text.push_str("\n\nPour en ajouter un : /comptes <server_url> <token>");
    }

    let mut send = bot.send_message(chat_id, text);
    if !buttons.is_empty() {
        send = send.reply_markup(InlineKeyboardMarkup::new(buttons));
    }
    send.await?;
    Ok(())
}

/// Shows the live, dynamic list of shared JMAP accounts the connected
/// token currently has access to (delegated/shared mailboxes, distinct
/// from the chat's own account), with a toggle button per account. Always
/// reconnects rather than reusing the cached client, so a mailbox access
/// grant added on the server after /login shows up without needing a
/// fresh /login.
async fn cmd_partages(bot: &Bot, state: &AppState, chat_id: ChatId) -> HandlerResult {
    let Some(account) = state.store.get(chat_id.0) else {
        bot.send_message(chat_id, NO_ACCOUNT_MSG).await?;
        return Ok(());
    };

    let client = match jmap::connect(
        &account.server_url,
        &account.token,
        state.config.allow_private_jmap_hosts,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Connexion impossible : {e}"))
                .await?;
            return Ok(());
        }
    };

    let accounts = jmap::list_shared_accounts(&client);
    if accounts.is_empty() {
        bot.send_message(
            chat_id,
            "Aucune boîte partagée accessible avec ce compte JMAP.",
        )
        .await?;
        return Ok(());
    }

    let enabled: HashSet<String> = account.shared_accounts.keys().cloned().collect();
    let keyboard = format::shared_accounts_keyboard(&accounts, &enabled);
    bot.send_message(
        chat_id,
        "📂 Boîtes partagées accessibles avec ce compte — active ou désactive leurs \
         notifications ici :",
    )
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
    let Ok(client) = jmap::connect(
        &account.server_url,
        &account.token,
        state.config.allow_private_jmap_hosts,
    )
    .await
    else {
        return Ok(());
    };

    let accounts = jmap::list_shared_accounts(&client);
    let enabled: HashSet<String> = account.shared_accounts.keys().cloned().collect();
    let keyboard = format::shared_accounts_keyboard(&accounts, &enabled);
    let _ = bot
        .edit_message_reply_markup(chat_id, message_id)
        .reply_markup(keyboard)
        .await;
    Ok(())
}

/// Disconnects one extra personal account (the "🗑 Déconnecter" button on
/// `/comptes`'s listing). Unlike `/partages`' shared-account toggle, this
/// is one-directional — reconnecting means running `/comptes <url>
/// <token>` again with its credentials, not a re-enable button, since we
/// don't keep a disconnected extra account's token around to reuse.
async fn disconnect_extra_account(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    query_id: CallbackQueryId,
    slot_id: &str,
) -> HandlerResult {
    state.forget_extra(chat_id.0, slot_id).await;
    match state.store.remove_extra_account(chat_id.0, slot_id) {
        Ok(true) => {
            bot.answer_callback_query(query_id)
                .text("🗑 Compte déconnecté.")
                .await?;
        }
        Ok(false) => {
            bot.answer_callback_query(query_id)
                .text("Déjà déconnecté.")
                .await?;
        }
        Err(e) => {
            bot.answer_callback_query(query_id)
                .text(format!("❌ Erreur : {e}"))
                .show_alert(true)
                .await?;
        }
    }
    Ok(())
}

async fn toggle_shared_account(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    message_id: MessageId,
    query_id: CallbackQueryId,
    account_id: &str,
) -> HandlerResult {
    let Some(account) = state.store.get(chat_id.0) else {
        bot.answer_callback_query(query_id)
            .text("Compte non connecté.")
            .await?;
        return Ok(());
    };

    if account.shared_accounts.contains_key(account_id) {
        state.forget_shared(chat_id.0, account_id).await;
        let _ = state.store.remove_shared_account(chat_id.0, account_id);
        bot.answer_callback_query(query_id)
            .text("🔕 Notifications désactivées.")
            .await?;
    } else {
        let client = match jmap::connect_shared(
            &account.server_url,
            &account.token,
            state.config.allow_private_jmap_hosts,
            account_id,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                bot.answer_callback_query(query_id)
                    .text(format!("❌ Connexion impossible : {e}"))
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
                    .text(format!("❌ Erreur lors de l'initialisation : {e}"))
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
        ) {
            bot.answer_callback_query(query_id)
                .text(format!("❌ Erreur de stockage : {e}"))
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
            .text("🔔 Notifications activées.")
            .await?;
    }

    refresh_partages_keyboard(bot, state, chat_id, message_id).await
}

/// `/mute <terme>` adds a filter; `/mute` with no argument lists the
/// chat's active filters instead of erroring, since "what's currently
/// muted" is something a user reasonably wants to check without having to
/// remember what they typed weeks ago.
async fn cmd_mute(bot: &Bot, state: &AppState, chat_id: ChatId, term: String) -> HandlerResult {
    let term = term.trim();
    if term.is_empty() {
        let Some(account) = state.store.get(chat_id.0) else {
            bot.send_message(chat_id, NO_ACCOUNT_MSG).await?;
            return Ok(());
        };
        let text = if account.muted.is_empty() {
            "Aucun filtre actif. /mute <expéditeur ou mot-clé> pour en ajouter un.".to_string()
        } else {
            let list = account
                .muted
                .iter()
                .map(|m| format!("• {m}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "🔇 Filtres actifs (plus de notification, le mail reste bien synchronisé) :\n{list}"
            )
        };
        bot.send_message(chat_id, text).await?;
        return Ok(());
    }

    match state.store.mute(chat_id.0, term) {
        Ok(true) => {
            bot.send_message(
                chat_id,
                format!("🔇 « {term} » est maintenant filtré : plus de notification pour les mails correspondants."),
            )
            .await?;
        }
        Ok(false) if state.store.get(chat_id.0).is_none() => {
            bot.send_message(chat_id, NO_ACCOUNT_MSG).await?;
        }
        Ok(false) => {
            bot.send_message(chat_id, format!("« {term} » est déjà filtré."))
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Erreur : {e}"))
                .await?;
        }
    }
    Ok(())
}

async fn cmd_unmute(bot: &Bot, state: &AppState, chat_id: ChatId, term: String) -> HandlerResult {
    let term = term.trim();
    if term.is_empty() {
        bot.send_message(
            chat_id,
            "Usage : /unmute <expéditeur ou mot-clé>. Tape /mute sans argument pour voir la liste.",
        )
        .await?;
        return Ok(());
    }

    match state.store.unmute(chat_id.0, term) {
        Ok(true) => {
            bot.send_message(chat_id, format!("🔔 « {term} » n'est plus filtré."))
                .await?;
        }
        Ok(false) => {
            bot.send_message(chat_id, format!("« {term} » n'était pas filtré."))
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Erreur : {e}"))
                .await?;
        }
    }
    Ok(())
}

/// Read-only full-text search against the chat's own mailbox (not its
/// shared accounts — those would need their own per-account search, kept
/// out of scope here). Results are rendered as ordinary notification
/// messages, complete with the same action buttons, so triage works the
/// same way whether a message arrived live or was dug up by search.
async fn cmd_rechercher(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    query: String,
) -> HandlerResult {
    let query = query.trim();
    if query.is_empty() {
        bot.send_message(chat_id, "Usage : /rechercher <texte>")
            .await?;
        return Ok(());
    }

    let client = match state.client_for(chat_id.0).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            bot.send_message(chat_id, NO_ACCOUNT_MSG).await?;
            return Ok(());
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Erreur de connexion : {e}"))
                .await?;
            return Ok(());
        }
    };

    let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

    let results = match jmap::search(&client, query).await {
        Ok(r) => r,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ {e}")).await?;
            return Ok(());
        }
    };

    if results.is_empty() {
        bot.send_message(chat_id, format!("Aucun résultat pour « {query} »."))
            .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        format!("🔎 {} résultat(s) pour « {query} » :", results.len()),
    )
    .await?;
    for summary in &results {
        let text = format::notification_text(summary, state.config.timezone, None);
        let keyboard = format::notification_keyboard(&summary.id, None, false);
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboard)
            .await?;
    }
    Ok(())
}

enum TriageAction {
    Archive,
    Junk,
    Delete,
}

/// Which chat/message a callback tap came from — bundled together purely
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
                .text(format!("❌ {e}"))
                .show_alert(true)
                .await?;
            return Ok(());
        }
    };

    let (result, toast) = match action {
        TriageAction::Archive => (jmap::archive(client, email_id).await, "📥 Archivé."),
        TriageAction::Junk => (jmap::junk(client, email_id).await, "🚫 Marqué comme spam."),
        TriageAction::Delete => (
            jmap::delete(client, email_id).await,
            "🗑 Déplacé vers la corbeille.",
        ),
    };

    match result {
        Ok(()) => {
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
            let _ = bot
                .edit_message_reply_markup(chat_id, message_id)
                .reply_markup(format::undo_keyboard(email_id, account_id))
                .await;
        }
        Err(e) => {
            bot.answer_callback_query(query_id)
                .text(format!("❌ {e}"))
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

    if !message.chat.is_private() || !state.config.authorized_chat_ids.contains(&chat_id) {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    }

    let Some(data) = q.data.clone() else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };

    // Two shapes: "s:{account_id}" (toggle a shared account from
    // /partages) or an email action, either "{action}:{email_id}" (the
    // chat's own mailbox) or "{action}:{account_id}:{email_id}" (a shared
    // mailbox — JMAP email ids are only unique within their account, so
    // the account has to travel with the id to route to the right client).
    let mut parts = data.splitn(3, ':');
    let (Some(action), Some(second)) = (parts.next(), parts.next()) else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };
    let third = parts.next();

    if action == "s" {
        return toggle_shared_account(&bot, &state, chat_id, message_id, q.id, second).await;
    }
    if action == "x" {
        return disconnect_extra_account(&bot, &state, chat_id, q.id, second).await;
    }

    let (account_id, email_id): (Option<&str>, &str) = match third {
        Some(email_id) => (Some(second), email_id),
        None => (None, second),
    };

    // account_id names either a shared JMAP account (under the primary
    // token) or an extra, fully independent one — two separate key spaces
    // (see store::Account::extra_accounts), so try shared first and only
    // fall back to extra when it's a clean "not found there", not an
    // actual connection error.
    let client_result = match account_id {
        Some(acc) => match state.client_for_shared(chat_id.0, acc).await {
            Ok(Some(c)) => Ok(Some(c)),
            Ok(None) => state.client_for_extra(chat_id.0, acc).await,
            Err(e) => Err(e),
        },
        None => state.client_for(chat_id.0).await,
    };
    let client = match client_result {
        Ok(Some(c)) => c,
        Ok(None) => {
            bot.answer_callback_query(q.id)
                .text("Compte non connecté ou désactivé. Tape /login, /partages ou /comptes.")
                .await?;
            return Ok(());
        }
        Err(e) => {
            bot.answer_callback_query(q.id)
                .text(format!("Erreur de connexion : {e}"))
                .show_alert(true)
                .await?;
            return Ok(());
        }
    };

    match action {
        "r" => match jmap::mark_read(&client, email_id).await {
            Ok(()) => {
                bot.answer_callback_query(q.id)
                    .text("✅ Marqué comme lu.")
                    .await?;
                // Reflect the new state on the notification itself (the
                // "Lu" button becomes an inert checkmark) instead of
                // leaving the chat looking exactly as before the tap.
                let _ = bot
                    .edit_message_reply_markup(chat_id, message_id)
                    .reply_markup(format::notification_keyboard(email_id, account_id, true))
                    .await;
            }
            Err(e) => {
                bot.answer_callback_query(q.id)
                    .text(format!("❌ {e}"))
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
            )
            .await;
        }
        "u" => {
            let pending = state.pending_undo.write().await.remove(&chat_id.0);
            match pending {
                Some(p)
                    if !p.is_expired()
                        && p.account_id.as_deref() == account_id
                        && p.email_id == email_id =>
                {
                    match jmap::restore_mailboxes(&client, email_id, p.restore_mailbox_ids).await {
                        Ok(()) => {
                            bot.answer_callback_query(q.id).text("↩️ Restauré.").await?;
                            let _ = bot
                                .edit_message_reply_markup(chat_id, message_id)
                                .reply_markup(format::notification_keyboard(
                                    email_id, account_id, false,
                                ))
                                .await;
                        }
                        Err(e) => {
                            bot.answer_callback_query(q.id)
                                .text(format!("❌ {e}"))
                                .show_alert(true)
                                .await?;
                        }
                    }
                }
                _ => {
                    bot.answer_callback_query(q.id)
                        .text("Trop tard pour annuler.")
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
                    for chunk in chunk_text(&text, TELEGRAM_MAX_MESSAGE_LEN - 16) {
                        bot.send_message(chat_id, chunk).await?;
                    }
                }
                Err(e) => {
                    bot.answer_callback_query(q.id)
                        .text(format!("❌ {e}"))
                        .show_alert(true)
                        .await?;
                }
            }
        }
        // "n" ("already read") is an intentionally inert label button —
        // it still needs to answer the callback so Telegram stops showing
        // a loading spinner on the tap, but there's nothing to do.
        "n" => {
            bot.answer_callback_query(q.id).text("Déjà lu.").await?;
        }
        _ => {
            bot.answer_callback_query(q.id).await?;
        }
    };

    Ok(())
}
