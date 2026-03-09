//! Bot command definitions.
//!
//! Commands are parsed by teloxide from incoming Telegram messages.  The
//! `BotCommands` derive macro generates the `descriptions()` and
//! `parse()` implementations automatically.
//!
//! # Example
//!
//! ```rust
//! use pn_bot::commands::Command;
//! use teloxide::utils::command::BotCommands;
//!
//! let cmd = Command::parse("/help", "botname").unwrap();
//! assert!(matches!(cmd, Command::Help));
//! ```

use teloxide::utils::command::BotCommands;

/// All commands that users can send to the bot.
#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Available commands:")]
pub enum Command {
    /// Start the bot and register the user.
    #[command(description = "Start the bot")]
    Start,

    /// Begin the multi-step subscription flow.
    #[command(description = "Subscribe to a market")]
    Subscribe,

    /// Remove an existing subscription via inline keyboard.
    #[command(description = "Unsubscribe from a market")]
    Unsubscribe,

    /// Begin the multi-step alert creation flow.
    #[command(description = "Set a price alert")]
    Alert,

    /// Display all active subscriptions for the calling user.
    #[command(description = "List your subscriptions")]
    List,

    /// Remove an alert via inline keyboard.
    #[command(description = "Remove an alert")]
    RemoveAlert,

    /// Fetch and display current prices for subscribed markets.
    #[command(description = "Check current prices")]
    Prices,

    /// Update the user's IANA timezone string.
    #[command(description = "Set your timezone", parse_with = "split")]
    Timezone(String),

    /// Show the command descriptions.
    #[command(description = "Show help")]
    Help,
}
