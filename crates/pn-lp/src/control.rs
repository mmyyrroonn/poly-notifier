use tokio::sync::{mpsc, watch};

use crate::types::RuntimeState;

#[derive(Debug, Clone)]
pub enum ControlCommand {
    Pause { reason: String },
    Resume { reason: String },
    CancelAll { reason: String },
    Flatten { reason: String },
    Split { amount: String, reason: String },
    Merge { amount: String, reason: String },
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
