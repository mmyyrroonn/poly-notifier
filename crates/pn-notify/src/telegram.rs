//! Telegram bot registry and message delivery.
//!
//! [`BotRegistry`] owns one [`teloxide::Bot`] handle per configured bot token
//! and exposes a single [`BotRegistry::send_message`] method that dispatches
//! to the correct bot by its ID.
//!
//! The "bot ID" is the numeric prefix that appears before the colon in every
//! Telegram bot token (e.g. for `"1234567890:ABC..."` the bot ID is
//! `"1234567890"`).

use std::collections::HashMap;

use teloxide::{
    prelude::*,
    types::ParseMode,
};
use tracing::{debug, warn};

/// Registry that maps bot IDs to live [`Bot`] handles.
///
/// # Example
///
/// ```no_run
/// # use pn_notify::telegram::BotRegistry;
/// let mut registry = BotRegistry::new();
/// registry.register("7123456789:AABBccdd...");
/// ```
pub struct BotRegistry {
    /// `bot_id` (numeric token prefix) → [`Bot`] handle.
    bots: HashMap<String, Bot>,
}

impl BotRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            bots: HashMap::new(),
        }
    }

    /// Register a bot from its Telegram token.
    ///
    /// The bot ID is derived by taking the part of the token that precedes the
    /// first `':'`.  If the token contains no colon the whole token string is
    /// used as the ID.
    pub fn register(&mut self, token: &str) {
        let bot_id = token
            .split(':')
            .next()
            .unwrap_or(token)
            .to_string();
        debug!(bot_id = %bot_id, "registering Telegram bot");
        let bot = Bot::new(token);
        self.bots.insert(bot_id, bot);
    }

    /// Look up a bot handle by its ID.
    pub fn get(&self, bot_id: &str) -> Option<&Bot> {
        self.bots.get(bot_id)
    }

    /// Return the IDs of all registered bots in unspecified order.
    pub fn bot_ids(&self) -> Vec<String> {
        self.bots.keys().cloned().collect()
    }

    /// Return the ID of whichever bot was inserted first, if any.
    ///
    /// Useful as a fallback when a request does not specify a bot.
    pub fn first_bot_id(&self) -> Option<String> {
        self.bots.keys().next().cloned()
    }

    /// Send an HTML-formatted message to `chat_id` using the bot identified
    /// by `bot_id`.
    ///
    /// Returns `Ok(())` on success or an error string on failure so callers
    /// can decide whether to retry.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` when:
    /// - `bot_id` is not present in the registry.
    /// - The Telegram API returns an error.
    pub async fn send_message(
        &self,
        bot_id: &str,
        chat_id: i64,
        text: &str,
    ) -> Result<(), String> {
        let bot = self
            .bots
            .get(bot_id)
            .ok_or_else(|| format!("bot '{}' not found in registry", bot_id))?;

        debug!(
            bot_id = %bot_id,
            chat_id = chat_id,
            text_len = text.len(),
            "sending Telegram message"
        );

        bot.send_message(ChatId(chat_id), text)
            .parse_mode(ParseMode::Html)
            .await
            .map_err(|e| {
                warn!(bot_id = %bot_id, chat_id = chat_id, error = %e, "Telegram send failed");
                e.to_string()
            })?;

        Ok(())
    }

    /// Returns `true` when no bots have been registered yet.
    pub fn is_empty(&self) -> bool {
        self.bots.is_empty()
    }

    /// Number of registered bots.
    pub fn len(&self) -> usize {
        self.bots.len()
    }
}

impl Default for BotRegistry {
    fn default() -> Self {
        Self::new()
    }
}
