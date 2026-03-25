use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::types::RuntimeState;

#[derive(Debug, Clone)]
pub enum ControlCommand {
    Pause {
        reason: String,
    },
    Resume {
        reason: String,
    },
    CancelAll {
        reason: String,
    },
    Flatten {
        reason: String,
    },
    Split {
        amount: String,
        reason: String,
    },
    Merge {
        amount: String,
        reason: String,
    },
    ExternalSignal {
        name: String,
        active: bool,
        reason: String,
    },
}

pub type RuntimeSnapshot = RuntimeState;

#[derive(Clone)]
pub struct LpControlHandle {
    cmd_tx: mpsc::UnboundedSender<ControlCommand>,
    snapshot_rx: watch::Receiver<RuntimeSnapshot>,
}

impl LpControlHandle {
    pub fn new(
        cmd_tx: mpsc::UnboundedSender<ControlCommand>,
        snapshot_rx: watch::Receiver<RuntimeSnapshot>,
    ) -> Self {
        Self {
            cmd_tx,
            snapshot_rx,
        }
    }

    pub fn send(&self, command: ControlCommand) -> Result<(), String> {
        self.cmd_tx.send(command).map_err(|error| error.to_string())
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot_rx.borrow().clone()
    }
}

#[derive(Clone, Default)]
pub struct MultiLpControlHandle {
    handles: Arc<BTreeMap<String, LpControlHandle>>,
}

impl MultiLpControlHandle {
    pub fn new(handles: BTreeMap<String, LpControlHandle>) -> Self {
        Self {
            handles: Arc::new(handles),
        }
    }

    pub fn from_single(condition_id: impl Into<String>, handle: LpControlHandle) -> Self {
        let mut handles = BTreeMap::new();
        handles.insert(condition_id.into(), handle);
        Self::new(handles)
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    pub fn sole_condition_id(&self) -> Option<&str> {
        (self.handles.len() == 1)
            .then(|| self.handles.keys().next())
            .flatten()
            .map(String::as_str)
    }

    pub fn send(&self, condition_id: &str, command: ControlCommand) -> Result<(), String> {
        let handle = self
            .handles
            .get(condition_id)
            .ok_or_else(|| format!("unknown lp market {condition_id}"))?;
        handle.send(command)
    }

    pub fn broadcast(&self, command: ControlCommand) -> Result<Vec<String>, String> {
        let mut targets = Vec::with_capacity(self.handles.len());
        let mut errors = Vec::new();
        for (condition_id, handle) in self.handles.iter() {
            match handle.send(command.clone()) {
                Ok(()) => targets.push(condition_id.clone()),
                Err(error) => errors.push(format!("{condition_id}: {error}")),
            }
        }
        if errors.is_empty() {
            Ok(targets)
        } else {
            Err(format!(
                "partial broadcast failure; succeeded: [{}]; failed: [{}]",
                targets.join(", "),
                errors.join("; ")
            ))
        }
    }

    pub fn snapshot(&self, condition_id: &str) -> Option<RuntimeSnapshot> {
        self.handles
            .get(condition_id)
            .map(LpControlHandle::snapshot)
    }

    pub fn snapshots(&self) -> Vec<RuntimeSnapshot> {
        self.handles
            .values()
            .map(LpControlHandle::snapshot)
            .collect::<Vec<_>>()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};

    use chrono::Utc;
    use tokio::sync::{mpsc, watch};

    use super::{ControlCommand, LpControlHandle, MultiLpControlHandle};
    use crate::types::{
        AccountSnapshot, MarketMetadata, RewardState, RuntimeFlags, RuntimeState, SignalState,
        TokenMetadata,
    };

    #[test]
    fn broadcast_reports_partial_failures_after_best_effort_delivery() {
        let (tx_ok, mut rx_ok) = mpsc::unbounded_channel();
        let (_snapshot_tx_ok, snapshot_rx_ok) = watch::channel(runtime_state("condition-ok"));
        let handle_ok = LpControlHandle::new(tx_ok, snapshot_rx_ok);

        let (tx_closed, _rx_closed) = mpsc::unbounded_channel();
        drop(_rx_closed);
        let (_snapshot_tx_closed, snapshot_rx_closed) =
            watch::channel(runtime_state("condition-closed"));
        let handle_closed = LpControlHandle::new(tx_closed, snapshot_rx_closed);

        let handles = MultiLpControlHandle::new(BTreeMap::from([
            ("condition-closed".to_string(), handle_closed),
            ("condition-ok".to_string(), handle_ok),
        ]));

        let error = handles
            .broadcast(ControlCommand::Pause {
                reason: "test pause".to_string(),
            })
            .expect_err("broadcast should report the closed receiver");

        assert!(error.contains("condition-closed"));
        match rx_ok
            .try_recv()
            .expect("open handle should still receive command")
        {
            ControlCommand::Pause { reason } => assert_eq!(reason, "test pause"),
            other => panic!("expected pause command, got {other:?}"),
        }
    }

    fn runtime_state(condition_id: &str) -> RuntimeState {
        let now = Utc::now();
        RuntimeState {
            market: MarketMetadata {
                condition_id: condition_id.to_string(),
                question: "Will X happen?".to_string(),
                tokens: vec![TokenMetadata {
                    asset_id: "asset-yes".to_string(),
                    outcome: "Yes".to_string(),
                    tick_size: "0.01".parse().unwrap(),
                }],
            },
            books: HashMap::new(),
            open_orders: Vec::new(),
            terminal_order_ids: HashSet::new(),
            positions: HashMap::new(),
            account: AccountSnapshot {
                usdc_balance: "100".parse().unwrap(),
                token_balances: HashMap::new(),
                updated_at: now,
            },
            signals: HashMap::from([(
                "external".to_string(),
                SignalState {
                    active: true,
                    reason: "test".to_string(),
                },
            )]),
            flags: RuntimeFlags::default(),
            last_market_event_at: Some(now),
            last_user_event_at: Some(now),
            last_heartbeat_at: Some(now),
            last_heartbeat_id: Some("hb-1".to_string()),
            last_decision_reason: None,
            reward: RewardState::default(),
        }
    }
}
