use crate::types::{RuntimeState, SignalState};

#[derive(Debug, Clone)]
pub struct SignalUpdate {
    pub name: String,
    pub active: bool,
    pub reason: String,
}

#[derive(Debug, Default, Clone)]
pub struct SignalAggregator;

impl SignalAggregator {
    pub fn apply(&self, state: &mut RuntimeState, update: SignalUpdate) {
        state.signals.insert(
            update.name,
            SignalState {
                active: update.active,
                reason: update.reason,
            },
        );
    }

    pub fn quoting_allowed(&self, state: &RuntimeState) -> bool {
        state.active_signals_allow_quoting()
    }
}
