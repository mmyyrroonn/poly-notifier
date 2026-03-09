//! Telegram bot interface for poly-notifier.
//!
//! This crate sets up a teloxide [`Dispatcher`] that routes incoming Telegram
//! updates to command handlers and a stateful dialogue engine.
//!
//! # Entry Point
//!
//! Call [`run_bot`] from your `main.rs`, passing an already-constructed
//! [`Bot`], the bot's numeric ID string (first segment of the token), an
//! active SQLite pool, API clients, and a [`CancellationToken`] for graceful
//! shutdown.

pub mod commands;
pub mod dialogues;
pub mod handlers;
pub mod keyboards;

use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;
use teloxide::{
    dispatching::{
        dialogue::{self, InMemStorage},
        UpdateFilterExt, UpdateHandler,
    },
    prelude::*,
    types::Update,
    utils::command::BotCommands,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use pn_polymarket::{ClobClient, GammaClient};

use crate::{
    commands::Command,
    dialogues::{
        handle_alert_subscription_selection, handle_alert_threshold, handle_alert_type_selection,
        handle_market_selection, handle_outcome_selection, handle_subscribe_alert_threshold,
        handle_subscribe_alert_type_selection, handle_url_input, DialogueState, MarketOption,
        MyDialogue,
    },
    handlers::{
        alert, callback_unsubscribe, feedback, help, list, prices, start, subscribe, timezone,
        unsubscribe,
    },
};

// ---------------------------------------------------------------------------
// Dispatcher schema
// ---------------------------------------------------------------------------

fn schema() -> UpdateHandler<anyhow::Error> {
    let dialogue_handler = dialogue::enter::<Update, InMemStorage<DialogueState>, DialogueState, _>()
        // ----------------------------------------------------------------
        // Command sub-branch
        // ----------------------------------------------------------------
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .branch(dptree::case![Command::Start].endpoint(
                    |bot: Bot,
                     _dialogue: MyDialogue,
                     msg: Message,
                     pool: SqlitePool,
                     bot_id: Arc<String>| async move {
                        start(bot, msg, pool, bot_id).await
                    },
                ))
                .branch(dptree::case![Command::Help].endpoint(
                    |bot: Bot, _dialogue: MyDialogue, msg: Message| async move {
                        help(bot, msg).await
                    },
                ))
                .branch(dptree::case![Command::List].endpoint(
                    |bot: Bot,
                     _dialogue: MyDialogue,
                     msg: Message,
                     pool: SqlitePool,
                     bot_id: Arc<String>| async move {
                        list(bot, msg, pool, bot_id).await
                    },
                ))
                .branch(dptree::case![Command::Prices].endpoint(
                    |bot: Bot,
                     _dialogue: MyDialogue,
                     msg: Message,
                     pool: SqlitePool,
                     bot_id: Arc<String>,
                     clob_client: Arc<ClobClient>| async move {
                        prices(bot, msg, pool, bot_id, clob_client).await
                    },
                ))
                .branch(dptree::case![Command::Subscribe].endpoint(
                    |bot: Bot, dialogue: MyDialogue, msg: Message| async move {
                        subscribe(bot, dialogue, msg).await
                    },
                ))
                .branch(dptree::case![Command::Unsubscribe].endpoint(
                    |bot: Bot,
                     _dialogue: MyDialogue,
                     msg: Message,
                     pool: SqlitePool,
                     bot_id: Arc<String>| async move {
                        unsubscribe(bot, msg, pool, bot_id).await
                    },
                ))
                .branch(dptree::case![Command::Alert].endpoint(
                    |bot: Bot,
                     dialogue: MyDialogue,
                     msg: Message,
                     pool: SqlitePool,
                     bot_id: Arc<String>| async move {
                        alert(bot, dialogue, msg, pool, bot_id).await
                    },
                ))
                .branch(dptree::case![Command::Feedback].endpoint(
                    |bot: Bot,
                     _dialogue: MyDialogue,
                     msg: Message,
                     pool: SqlitePool,
                     bot_id: Arc<String>| async move {
                        feedback(bot, msg, pool, bot_id).await
                    },
                ))
                .branch(dptree::case![Command::Timezone(tz)].endpoint(
                    |bot: Bot,
                     _dialogue: MyDialogue,
                     msg: Message,
                     tz: String,
                     pool: SqlitePool,
                     bot_id: Arc<String>| async move {
                        timezone(bot, msg, tz, pool, bot_id).await
                    },
                )),
        )
        // ----------------------------------------------------------------
        // Plain-message sub-branch – dialogue state machine transitions
        // ----------------------------------------------------------------
        .branch(
            Update::filter_message()
                .branch(
                    dptree::case![DialogueState::AwaitingUrl].endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         msg: Message,
                         gamma_client: Arc<GammaClient>| async move {
                            handle_url_input(bot, dialogue, msg, gamma_client).await
                        },
                    ),
                )
                .branch(
                    dptree::case![DialogueState::AwaitingSubscribeAlertThreshold {
                        condition_id,
                        question,
                        outcomes,
                        token_ids,
                        outcome_index,
                        alert_type
                    }]
                    .endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         msg: Message,
                         pool: SqlitePool,
                         bot_id: Arc<String>,
                         (condition_id, question, outcomes, token_ids, outcome_index, alert_type): (
                            String,
                            String,
                            Vec<String>,
                            Vec<String>,
                            i64,
                            String,
                        )| async move {
                            handle_subscribe_alert_threshold(
                                bot,
                                dialogue,
                                msg,
                                pool,
                                (*bot_id).clone(),
                                condition_id,
                                question,
                                outcomes,
                                token_ids,
                                outcome_index,
                                alert_type,
                            )
                            .await
                        },
                    ),
                )
                .branch(
                    dptree::case![DialogueState::AwaitingAlertThreshold {
                        subscription_id,
                        question,
                        alert_type
                    }]
                    .endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         msg: Message,
                         pool: SqlitePool,
                         bot_id: Arc<String>,
                         (subscription_id, question, alert_type): (i64, String, String)| async move {
                            handle_alert_threshold(
                                bot,
                                dialogue,
                                msg,
                                pool,
                                (*bot_id).clone(),
                                subscription_id,
                                question,
                                alert_type,
                            )
                            .await
                        },
                    ),
                ),
        )
        // ----------------------------------------------------------------
        // Callback-query sub-branch – dialogue state machine transitions
        // ----------------------------------------------------------------
        .branch(
            Update::filter_callback_query()
                .branch(
                    dptree::case![DialogueState::AwaitingMarketSelection { markets }].endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         q: CallbackQuery,
                         markets: Vec<MarketOption>| async move {
                            handle_market_selection(bot, dialogue, q, markets).await
                        },
                    ),
                )
                .branch(
                    dptree::case![DialogueState::AwaitingOutcomeSelection {
                        condition_id,
                        question,
                        outcomes,
                        token_ids
                    }]
                    .endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         q: CallbackQuery,
                         (condition_id, question, outcomes, token_ids): (
                            String,
                            String,
                            Vec<String>,
                            Vec<String>,
                        )| async move {
                            handle_outcome_selection(
                                bot,
                                dialogue,
                                q,
                                condition_id,
                                question,
                                outcomes,
                                token_ids,
                            )
                            .await
                        },
                    ),
                )
                .branch(
                    dptree::case![DialogueState::AwaitingSubscribeAlertType {
                        condition_id,
                        question,
                        outcomes,
                        token_ids,
                        outcome_index
                    }]
                    .endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         q: CallbackQuery,
                         (condition_id, question, outcomes, token_ids, outcome_index): (
                            String,
                            String,
                            Vec<String>,
                            Vec<String>,
                            i64,
                        )| async move {
                            handle_subscribe_alert_type_selection(
                                bot,
                                dialogue,
                                q,
                                condition_id,
                                question,
                                outcomes,
                                token_ids,
                                outcome_index,
                            )
                            .await
                        },
                    ),
                )
                .branch(
                    dptree::case![DialogueState::AwaitingAlertSubscription].endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         q: CallbackQuery,
                         pool: SqlitePool| async move {
                            handle_alert_subscription_selection(bot, dialogue, q, pool).await
                        },
                    ),
                )
                .branch(
                    dptree::case![DialogueState::AwaitingAlertType {
                        subscription_id,
                        question
                    }]
                    .endpoint(
                        |bot: Bot,
                         dialogue: MyDialogue,
                         q: CallbackQuery,
                         (subscription_id, question): (i64, String)| async move {
                            handle_alert_type_selection(
                                bot,
                                dialogue,
                                q,
                                subscription_id,
                                question,
                            )
                            .await
                        },
                    ),
                ),
        );

    // One-shot callback handler for non-dialogue inline actions.
    let callback_handler = Update::filter_callback_query().endpoint(
        |bot: Bot, q: CallbackQuery, pool: SqlitePool| async move {
            dispatch_callback(bot, q, pool).await
        },
    );

    dptree::entry()
        .branch(dialogue_handler)
        .branch(callback_handler)
}

// ---------------------------------------------------------------------------
// Callback-query dispatcher (non-dialogue one-shot actions)
// ---------------------------------------------------------------------------

/// Route a raw callback-query to the appropriate handler based on its
/// data prefix.
///
/// Handled prefixes:
/// * `unsub:{id}` – delete a subscription (cascade deletes alerts).
async fn dispatch_callback(
    bot: Bot,
    q: CallbackQuery,
    pool: SqlitePool,
) -> anyhow::Result<()> {
    let data = match q.data.as_deref() {
        Some(d) => d.to_string(),
        None => {
            bot.answer_callback_query(q.id.clone()).await?;
            return Ok(());
        }
    };

    if let Some(id_str) = data.strip_prefix("unsub:") {
        if let Ok(sub_id) = id_str.parse::<i64>() {
            return callback_unsubscribe(bot, q, pool, sub_id).await;
        }
    }

    // Unknown callback – just acknowledge it.
    bot.answer_callback_query(q.id.clone()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Start the Telegram bot dispatcher and run until `cancel` is triggered.
pub async fn run_bot(
    bot: Bot,
    bot_id: String,
    pool: SqlitePool,
    clob_client: Arc<ClobClient>,
    gamma_client: Arc<GammaClient>,
    cancel: CancellationToken,
) -> Result<()> {
    info!(bot_id, "setting up Telegram bot");

    bot.set_my_commands(Command::bot_commands()).await?;

    let bot_id = Arc::new(bot_id);
    let storage = InMemStorage::<DialogueState>::new();

    let mut dispatcher = Dispatcher::builder(bot, schema())
        .dependencies(dptree::deps![
            storage,
            pool,
            bot_id,
            clob_client,
            gamma_client
        ])
        .default_handler(|_upd| async { /* silently drop unhandled updates */ })
        .error_handler(LoggingErrorHandler::with_custom_text(
            "an error occurred in the dispatcher",
        ))
        .build();

    let shutdown = dispatcher.shutdown_token();

    tokio::spawn(async move {
        cancel.cancelled().await;
        info!("cancellation received, shutting down bot dispatcher");
        if let Ok(fut) = shutdown.shutdown() {
            fut.await;
        }
    });

    dispatcher.dispatch().await;

    info!("bot dispatcher stopped");
    Ok(())
}
