use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, Me};
use teloxide::utils::command::BotCommands;

use crate::Bot;
use crate::format::{TELEGRAM_MAX_MESSAGE_LEN, chunk_text};
use crate::jmap;
use crate::state::AppState;
use crate::store::Account;

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

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

    let client = std::sync::Arc::new(client);
    state
        .clients
        .write()
        .await
        .insert(chat_id.0, client.clone());
    let handle = crate::watcher::spawn(bot.clone(), state.clone(), chat_id.0, client);
    state.watchers.write().await.insert(chat_id.0, handle);

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
        bot.send_message(
            chat_id,
            "Aucun compte connecté. Tape /login pour commencer.",
        )
        .await?;
        return Ok(());
    };

    let watching = state.watchers.read().await.contains_key(&chat_id.0);
    let sync_state = if account.last_state.is_some() {
        "synchronisé"
    } else {
        "initialisation…"
    };

    bot.send_message(
        chat_id,
        format!(
            "📬 Connecté en tant que {}\nSurveillance : {}\nÉtat : {}",
            account.email,
            if watching {
                "active ✅"
            } else {
                "inactive ⚠️ (tape /login à nouveau)"
            },
            sync_state,
        ),
    )
    .await?;
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
    let Some((action, email_id)) = data.split_once(':') else {
        bot.answer_callback_query(q.id).await?;
        return Ok(());
    };

    let client = match state.client_for(chat_id.0).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            bot.answer_callback_query(q.id)
                .text("Compte non connecté. Tape /login.")
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
            }
            Err(e) => {
                bot.answer_callback_query(q.id)
                    .text(format!("❌ {e}"))
                    .show_alert(true)
                    .await?;
            }
        },
        "a" => match jmap::archive(&client, email_id).await {
            Ok(()) => {
                bot.answer_callback_query(q.id).text("📥 Archivé.").await?;
                let _ = bot
                    .edit_message_reply_markup(chat_id, message_id)
                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<_>>::new()))
                    .await;
            }
            Err(e) => {
                bot.answer_callback_query(q.id)
                    .text(format!("❌ {e}"))
                    .show_alert(true)
                    .await?;
            }
        },
        "d" => match jmap::delete(&client, email_id).await {
            Ok(()) => {
                bot.answer_callback_query(q.id).text("🗑 Supprimé.").await?;
                let _ = bot
                    .edit_message_reply_markup(chat_id, message_id)
                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<_>>::new()))
                    .await;
            }
            Err(e) => {
                bot.answer_callback_query(q.id)
                    .text(format!("❌ {e}"))
                    .show_alert(true)
                    .await?;
            }
        },
        "f" => match jmap::fetch_full_text(&client, email_id).await {
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
        },
        _ => {
            bot.answer_callback_query(q.id).await?;
        }
    };

    Ok(())
}
