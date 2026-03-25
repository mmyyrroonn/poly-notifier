use std::collections::HashMap;

use chrono::Utc;
use serde::Serialize;

use crate::types::{
    BookSnapshot, ManagedOrder, PositionSnapshot, QuoteIntent, RewardState, RuntimeFlags,
    RuntimeState, SignalState,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignalTransition {
    pub name: String,
    pub previous_active: Option<bool>,
    pub active: bool,
    pub previous_reason: Option<String>,
    pub reason: String,
}

pub fn signal_transitions(
    before: &HashMap<String, SignalState>,
    after: &HashMap<String, SignalState>,
) -> Vec<SignalTransition> {
    let mut names = after.keys().cloned().collect::<Vec<_>>();
    names.sort();

    names
        .into_iter()
        .filter_map(|name| {
            let next = after.get(&name)?;
            let previous = before.get(&name);
            let changed = match previous {
                Some(previous) => previous.active != next.active || previous.reason != next.reason,
                None => true,
            };

            changed.then(|| SignalTransition {
                name,
                previous_active: previous.map(|signal| signal.active),
                active: next.active,
                previous_reason: previous.map(|signal| signal.reason.clone()),
                reason: next.reason.clone(),
            })
        })
        .collect()
}

pub fn summarize_quotes(quotes: &[QuoteIntent]) -> String {
    if quotes.is_empty() {
        return "none".to_string();
    }

    quotes
        .iter()
        .map(|quote| {
            format!(
                "{} {} {} @ {} ({})",
                quote.side, quote.size, quote.asset_id, quote.price, quote.reason
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditStateSnapshot {
    pub market: AuditMarketSnapshot,
    pub flags: RuntimeFlags,
    pub books: Vec<AuditBookSnapshot>,
    pub open_orders: Vec<ManagedOrder>,
    pub positions: Vec<PositionSnapshot>,
    pub account: AuditAccountSnapshot,
    pub signals: Vec<AuditSignalSnapshot>,
    pub reward: AuditRewardStateSnapshot,
    pub last_market_event_at: Option<chrono::DateTime<Utc>>,
    pub last_user_event_at: Option<chrono::DateTime<Utc>>,
    pub last_heartbeat_at: Option<chrono::DateTime<Utc>>,
    pub last_heartbeat_id: Option<String>,
    pub last_decision_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditMarketSnapshot {
    pub condition_id: String,
    pub question: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditBookSnapshot {
    pub asset_id: String,
    pub received_at: chrono::DateTime<Utc>,
    pub age_ms: i64,
    pub min_top_depth: rust_decimal::Decimal,
    pub best_bid: Option<crate::types::BookLevel>,
    pub best_ask: Option<crate::types::BookLevel>,
    pub bids: Vec<crate::types::BookLevel>,
    pub asks: Vec<crate::types::BookLevel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditAccountSnapshot {
    pub usdc_balance: rust_decimal::Decimal,
    pub token_balances: Vec<AuditTokenBalance>,
    pub updated_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditTokenBalance {
    pub asset_id: String,
    pub balance: rust_decimal::Decimal,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditSignalSnapshot {
    pub name: String,
    pub active: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditRewardStateSnapshot {
    pub last_attempt_at: Option<chrono::DateTime<Utc>>,
    pub last_success_at: Option<chrono::DateTime<Utc>>,
    pub last_error: Option<String>,
    pub snapshot: Option<crate::types::RewardSnapshot>,
}

pub fn runtime_state_snapshot(state: &RuntimeState) -> AuditStateSnapshot {
    let mut books = state
        .books
        .values()
        .map(audit_book_snapshot)
        .collect::<Vec<_>>();
    books.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));

    let mut open_orders = state.open_orders.clone();
    open_orders.sort_by(|left, right| left.order_id.cmp(&right.order_id));

    let mut positions = state.positions.values().cloned().collect::<Vec<_>>();
    positions.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));

    let mut token_balances = state
        .account
        .token_balances
        .iter()
        .map(|(asset_id, balance)| AuditTokenBalance {
            asset_id: asset_id.clone(),
            balance: *balance,
        })
        .collect::<Vec<_>>();
    token_balances.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));

    let mut signals = state
        .signals
        .iter()
        .map(|(name, signal)| AuditSignalSnapshot {
            name: name.clone(),
            active: signal.active,
            reason: signal.reason.clone(),
        })
        .collect::<Vec<_>>();
    signals.sort_by(|left, right| left.name.cmp(&right.name));

    AuditStateSnapshot {
        market: AuditMarketSnapshot {
            condition_id: state.market.condition_id.clone(),
            question: state.market.question.clone(),
        },
        flags: state.flags.clone(),
        books,
        open_orders,
        positions,
        account: AuditAccountSnapshot {
            usdc_balance: state.account.usdc_balance,
            token_balances,
            updated_at: state.account.updated_at,
        },
        signals,
        reward: audit_reward_state_snapshot(&state.reward),
        last_market_event_at: state.last_market_event_at,
        last_user_event_at: state.last_user_event_at,
        last_heartbeat_at: state.last_heartbeat_at,
        last_heartbeat_id: state.last_heartbeat_id.clone(),
        last_decision_reason: state.last_decision_reason.clone(),
    }
}

fn audit_book_snapshot(book: &BookSnapshot) -> AuditBookSnapshot {
    AuditBookSnapshot {
        asset_id: book.asset_id.clone(),
        received_at: book.received_at,
        age_ms: Utc::now()
            .signed_duration_since(book.received_at)
            .num_milliseconds()
            .max(0),
        min_top_depth: book.min_top_depth(),
        best_bid: book.best_bid().cloned(),
        best_ask: book.best_ask().cloned(),
        bids: book.bids.clone(),
        asks: book.asks.clone(),
    }
}

fn audit_reward_state_snapshot(reward: &RewardState) -> AuditRewardStateSnapshot {
    AuditRewardStateSnapshot {
        last_attempt_at: reward.last_attempt_at,
        last_success_at: reward.last_success_at,
        last_error: reward.last_error.clone(),
        snapshot: reward.snapshot.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{signal_transitions, SignalTransition};
    use crate::types::SignalState;

    #[test]
    fn signal_transitions_detect_changes_and_new_signals() {
        let before = HashMap::from([
            (
                "orderbook".to_string(),
                SignalState {
                    active: true,
                    reason: "healthy".to_string(),
                },
            ),
            (
                "external".to_string(),
                SignalState {
                    active: true,
                    reason: "manual on".to_string(),
                },
            ),
        ]);
        let after = HashMap::from([
            (
                "orderbook".to_string(),
                SignalState {
                    active: false,
                    reason: "empty ask".to_string(),
                },
            ),
            (
                "external".to_string(),
                SignalState {
                    active: true,
                    reason: "manual on".to_string(),
                },
            ),
            (
                "risk".to_string(),
                SignalState {
                    active: false,
                    reason: "position breach".to_string(),
                },
            ),
        ]);

        let transitions = signal_transitions(&before, &after);

        assert_eq!(
            transitions,
            vec![
                SignalTransition {
                    name: "orderbook".to_string(),
                    previous_active: Some(true),
                    active: false,
                    previous_reason: Some("healthy".to_string()),
                    reason: "empty ask".to_string(),
                },
                SignalTransition {
                    name: "risk".to_string(),
                    previous_active: None,
                    active: false,
                    previous_reason: None,
                    reason: "position breach".to_string(),
                },
            ]
        );
    }
}
