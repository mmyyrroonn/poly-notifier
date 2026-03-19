use std::collections::HashMap;

use crate::types::{QuoteIntent, SignalState};

#[derive(Debug, Clone, PartialEq, Eq)]
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

    names.into_iter()
        .filter_map(|name| {
            let next = after.get(&name)?;
            let previous = before.get(&name);
            let changed = match previous {
                Some(previous) => {
                    previous.active != next.active || previous.reason != next.reason
                }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{SignalTransition, signal_transitions};
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
