use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use pn_common::db::{
    insert_lp_control_action, insert_lp_heartbeat, insert_lp_position_snapshot, insert_lp_report,
    insert_lp_risk_event, upsert_lp_order, upsert_lp_trade,
};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::control::{ControlCommand, LpControlHandle};
use crate::decision::{DecisionConfig, DecisionEngine, DecisionOutcome};
use crate::observability::{signal_transitions, summarize_quotes};
use crate::risk::{FlattenIntent, RiskAction, RiskConfig, RiskEngine};
use crate::signals::{SignalAggregator, SignalUpdate};
use crate::types::{
    AccountSnapshot, BookSnapshot, ManagedOrder, PositionSnapshot, QuoteIntent, QuoteSide,
    RuntimeState, SignalState, TradeFill,
};

#[derive(Debug, Clone)]
pub enum ExchangeEvent {
    Book(BookSnapshot),
    Order(ManagedOrder),
    Trade(TradeFill),
    TickSize {
        asset_id: String,
        new_tick_size: Decimal,
    },
}

#[derive(Debug, Clone)]
pub struct ReconciliationSnapshot {
    pub open_orders: Vec<ManagedOrder>,
    pub positions: Vec<PositionSnapshot>,
    pub account: AccountSnapshot,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub decision: DecisionConfig,
    pub risk: RiskConfig,
    pub heartbeat_interval: Duration,
    pub reconciliation_interval: Duration,
    pub report_interval: Duration,
    pub snapshot_interval: Duration,
    pub max_quote_age: Duration,
}

#[async_trait]
pub trait ExchangeAdapter: Send + Sync {
    fn start(&self, event_tx: mpsc::UnboundedSender<ExchangeEvent>, cancel: CancellationToken);

    async fn reconcile(&self) -> Result<ReconciliationSnapshot>;

    async fn post_quotes(&self, quotes: &[QuoteIntent]) -> Result<Vec<ManagedOrder>>;

    async fn cancel_orders(&self, order_ids: &[String]) -> Result<()>;

    async fn cancel_all(&self) -> Result<()>;

    async fn flatten(&self, intent: &FlattenIntent) -> Result<Option<TradeFill>>;

    async fn post_heartbeat(&self, last_heartbeat_id: Option<&str>) -> Result<String>;

    async fn split(&self, amount: Decimal) -> Result<String>;

    async fn merge(&self, amount: Decimal) -> Result<String>;
}

#[async_trait]
pub trait Reporter: Send + Sync {
    async fn send(&self, report_type: &str, message: &str) -> Result<()>;
}

pub struct LpService {
    exchange: Arc<dyn ExchangeAdapter>,
    pool: SqlitePool,
    config: ServiceConfig,
    decision: DecisionEngine,
    risk: RiskEngine,
    reporter: Option<Arc<dyn Reporter>>,
    control_rx: mpsc::UnboundedReceiver<ControlCommand>,
    snapshot_tx: watch::Sender<RuntimeState>,
}

impl LpService {
    pub fn new(
        exchange: Arc<dyn ExchangeAdapter>,
        pool: SqlitePool,
        config: ServiceConfig,
        reporter: Option<Arc<dyn Reporter>>,
        initial_state: RuntimeState,
    ) -> (Self, LpControlHandle) {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (snapshot_tx, snapshot_rx) = watch::channel(initial_state);

        (
            Self {
                exchange,
                pool,
                decision: DecisionEngine::new(config.decision.clone()),
                risk: RiskEngine::new(config.risk.clone()),
                config,
                reporter,
                control_rx,
                snapshot_tx,
            },
            LpControlHandle::new(control_tx, snapshot_rx),
        )
    }

    pub async fn run(mut self, cancel: CancellationToken) -> Result<()> {
        info!("LP service starting");

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        self.exchange.start(event_tx, cancel.clone());

        let mut heartbeat_tick = interval(self.config.heartbeat_interval);
        heartbeat_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        heartbeat_tick.tick().await;

        let mut reconcile_tick = interval(self.config.reconciliation_interval);
        reconcile_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        reconcile_tick.tick().await;

        let mut report_tick = interval(self.config.report_interval);
        report_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        report_tick.tick().await;

        let mut snapshot_tick = interval(self.config.snapshot_interval);
        snapshot_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        snapshot_tick.tick().await;

        let mut state = self.snapshot_tx.borrow().clone();
        info!(
            target: "lp.runtime",
            condition_id = %state.market.condition_id,
            question = %state.market.question,
            tokens = state.market.tokens.len(),
            bootstrap_open_orders = state.open_orders.len(),
            bootstrap_positions = state.positions.len(),
            "LP runtime bootstrapped"
        );
        self.recompute_quotes(&mut state).await?;
        self.publish_snapshot(&state);

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                maybe_cmd = self.control_rx.recv() => {
                    match maybe_cmd {
                        Some(command) => {
                            self.handle_control_command(&mut state, command).await?;
                        }
                        None => break,
                    }
                }
                maybe_event = event_rx.recv() => {
                    match maybe_event {
                        Some(event) => self.handle_exchange_event(&mut state, event).await?,
                        None => break,
                    }
                }
                _ = heartbeat_tick.tick() => {
                    self.handle_heartbeat_tick(&mut state).await?;
                }
                _ = reconcile_tick.tick() => {
                    self.handle_reconciliation_tick(&mut state).await?;
                }
                _ = report_tick.tick() => {
                    self.send_periodic_report(&state).await?;
                }
                _ = snapshot_tick.tick() => {
                    self.persist_positions_snapshot(&state, "periodic").await?;
                }
            }

            self.refresh_health_flags(&mut state);
            self.apply_timer_risk(&mut state).await?;
            self.recompute_quotes(&mut state).await?;
            self.publish_snapshot(&state);
        }

        info!("LP service stopping");
        Ok(())
    }

    async fn handle_control_command(
        &self,
        state: &mut RuntimeState,
        command: ControlCommand,
    ) -> Result<()> {
        let aggregator = SignalAggregator;
        match command {
            ControlCommand::Pause { reason } => {
                warn!(target: "lp.control", reason = %reason, "pause requested");
                insert_lp_control_action(&self.pool, "pause", Some(&reason)).await?;
                state.flags.paused = true;
                self.emit_risk("control_pause", "info", json!({ "reason": reason })).await?;
            }
            ControlCommand::Resume { reason } => {
                info!(target: "lp.control", reason = %reason, "resume requested");
                insert_lp_control_action(&self.pool, "resume", Some(&reason)).await?;
                state.flags.paused = false;
                state.flags.flattening = false;
                self.emit_risk("control_resume", "info", json!({ "reason": reason })).await?;
            }
            ControlCommand::CancelAll { reason } => {
                warn!(target: "lp.control", reason = %reason, open_orders = state.open_orders.len(), "cancel-all requested");
                insert_lp_control_action(&self.pool, "cancel_all", Some(&reason)).await?;
                self.exchange.cancel_all().await?;
                self.mark_orders_status(&state.market.condition_id, &state.open_orders, "CANCELED")
                    .await?;
                state.open_orders.clear();
                self.emit_risk("control_cancel_all", "info", json!({ "reason": reason }))
                    .await?;
            }
            ControlCommand::Flatten { reason } => {
                warn!(target: "lp.control", reason = %reason, positions = state.positions.len(), "flatten requested");
                insert_lp_control_action(&self.pool, "flatten", Some(&reason)).await?;
                state.flags.paused = true;
                state.flags.flattening = true;
                self.exchange.cancel_all().await?;
                self.mark_orders_status(&state.market.condition_id, &state.open_orders, "CANCELED")
                    .await?;
                state.open_orders.clear();

                for intent in self.flatten_intents_from_positions(state) {
                    self.execute_flatten(state, &reason, &intent).await?;
                }
            }
            ControlCommand::Split { amount, reason } => {
                info!(target: "lp.control", reason = %reason, amount = %amount, "split requested");
                insert_lp_control_action(&self.pool, "split", Some(&reason)).await?;
                let amount = amount
                    .parse::<Decimal>()
                    .with_context(|| format!("invalid split amount {amount}"))?;
                let tx_hash = self.exchange.split(amount).await?;
                info!(target: "lp.inventory", amount = %amount, tx_hash = %tx_hash, "split submitted");
                self.emit_risk(
                    "control_split",
                    "info",
                    json!({ "amount": amount.to_string(), "reason": reason, "tx_hash": tx_hash }),
                )
                .await?;
            }
            ControlCommand::Merge { amount, reason } => {
                info!(target: "lp.control", reason = %reason, amount = %amount, "merge requested");
                insert_lp_control_action(&self.pool, "merge", Some(&reason)).await?;
                let amount = amount
                    .parse::<Decimal>()
                    .with_context(|| format!("invalid merge amount {amount}"))?;
                let tx_hash = self.exchange.merge(amount).await?;
                info!(target: "lp.inventory", amount = %amount, tx_hash = %tx_hash, "merge submitted");
                self.emit_risk(
                    "control_merge",
                    "info",
                    json!({ "amount": amount.to_string(), "reason": reason, "tx_hash": tx_hash }),
                )
                .await?;
            }
            ControlCommand::ExternalSignal {
                name,
                active,
                reason,
            } => {
                insert_lp_control_action(&self.pool, &format!("signal:{name}"), Some(&reason))
                    .await?;
                let before = state.signals.clone();
                aggregator.apply(
                    state,
                    SignalUpdate {
                        name,
                        active,
                        reason,
                    },
                );
                self.log_signal_transitions(&before, &state.signals);
            }
        }

        Ok(())
    }

    async fn handle_exchange_event(
        &self,
        state: &mut RuntimeState,
        event: ExchangeEvent,
    ) -> Result<()> {
        let now = Utc::now();
        match event {
            ExchangeEvent::Book(book) => {
                let before = state.signals.clone();
                let signal_state = if book.bids.is_empty() || book.asks.is_empty() {
                    SignalUpdate {
                        name: "orderbook".to_string(),
                        active: false,
                        reason: "book has no two-sided depth".to_string(),
                    }
                } else {
                    SignalUpdate {
                        name: "orderbook".to_string(),
                        active: true,
                        reason: "book healthy".to_string(),
                    }
                };
                SignalAggregator.apply(state, signal_state);
                self.log_signal_transitions(&before, &state.signals);
                state.last_market_event_at = Some(now);
                state.books.insert(book.asset_id.clone(), book);
            }
            ExchangeEvent::Order(order) => {
                info!(
                    target: "lp.order",
                    order_id = %order.order_id,
                    asset_id = %order.asset_id,
                    side = %order.side,
                    status = %order.status,
                    price = %order.price,
                    size = %order.size,
                    "order update received"
                );
                state.last_user_event_at = Some(now);
                self.persist_order(state, &order, None).await?;
                upsert_order_state(state, order);
            }
            ExchangeEvent::Trade(fill) => {
                info!(
                    target: "lp.trade",
                    trade_id = %fill.trade_id,
                    order_id = fill.order_id.as_deref().unwrap_or(""),
                    asset_id = %fill.asset_id,
                    side = %fill.side,
                    price = %fill.price,
                    size = %fill.size,
                    status = %fill.status,
                    "trade fill received"
                );
                state.last_user_event_at = Some(now);
                self.persist_trade(state, &fill).await?;
                state.fills.push(fill.clone());
                let actions = self.risk.on_fill(&fill);
                self.apply_risk_actions(state, actions).await?;
            }
            ExchangeEvent::TickSize {
                asset_id,
                new_tick_size,
            } => {
                info!(target: "lp.market", asset_id = %asset_id, new_tick_size = %new_tick_size, "tick size updated");
                if let Some(token) = state
                    .market
                    .tokens
                    .iter_mut()
                    .find(|token| token.asset_id == asset_id)
                {
                    token.tick_size = new_tick_size;
                }
            }
        }

        Ok(())
    }

    async fn handle_heartbeat_tick(&self, state: &mut RuntimeState) -> Result<()> {
        match self
            .exchange
            .post_heartbeat(state.last_heartbeat_id.as_deref())
            .await
        {
            Ok(heartbeat_id) => {
                state.flags.heartbeat_healthy = true;
                state.last_heartbeat_at = Some(Utc::now());
                state.last_heartbeat_id = Some(heartbeat_id.clone());
                insert_lp_heartbeat(&self.pool, &heartbeat_id, "ok", None).await?;
                info!(target: "lp.heartbeat", heartbeat_id = %heartbeat_id, "heartbeat acknowledged");
            }
            Err(error) => {
                state.flags.heartbeat_healthy = false;
                insert_lp_heartbeat(&self.pool, "unknown", "error", Some(&error.to_string()))
                    .await?;
                error!(target: "lp.heartbeat", error = %error, "heartbeat submission failed");
                self.emit_risk(
                    "heartbeat_error",
                    "warn",
                    json!({ "error": error.to_string() }),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn handle_reconciliation_tick(&self, state: &mut RuntimeState) -> Result<()> {
        let snapshot = self.exchange.reconcile().await?;
        state.account = snapshot.account;
        state.positions = snapshot
            .positions
            .into_iter()
            .map(|position| (position.asset_id.clone(), position))
            .collect::<HashMap<_, _>>();
        state.open_orders = snapshot.open_orders;
        state.last_user_event_at = Some(Utc::now());
        info!(
            target: "lp.reconcile",
            open_orders = state.open_orders.len(),
            positions = state.positions.len(),
            usdc_balance = %state.account.usdc_balance,
            "reconciliation snapshot applied"
        );
        Ok(())
    }

    async fn apply_timer_risk(&self, state: &mut RuntimeState) -> Result<()> {
        let actions = self.risk.on_timer(state, Utc::now());
        self.apply_risk_actions(state, actions).await
    }

    async fn apply_risk_actions(
        &self,
        state: &mut RuntimeState,
        actions: Vec<RiskAction>,
    ) -> Result<()> {
        for action in actions {
            match action {
                RiskAction::None => {}
                RiskAction::Pause { reason } => {
                    warn!(target: "lp.risk", reason = %reason, "risk pause triggered");
                    state.flags.paused = true;
                    self.emit_risk("pause", "warn", json!({ "reason": reason }))
                        .await?;
                }
                RiskAction::Resume { reason } => {
                    info!(target: "lp.risk", reason = %reason, "risk resume triggered");
                    state.flags.paused = false;
                    state.flags.flattening = false;
                    self.emit_risk("resume", "info", json!({ "reason": reason }))
                        .await?;
                }
                RiskAction::CancelAll { reason } => {
                    warn!(target: "lp.risk", reason = %reason, open_orders = state.open_orders.len(), "risk cancel-all triggered");
                    self.exchange.cancel_all().await?;
                    self.mark_orders_status(&state.market.condition_id, &state.open_orders, "CANCELED")
                        .await?;
                    state.open_orders.clear();
                    self.emit_risk("cancel_all", "warn", json!({ "reason": reason }))
                        .await?;
                }
                RiskAction::Flatten(intent) => {
                    self.execute_flatten(state, "risk flatten", &intent).await?;
                }
            }
        }

        Ok(())
    }

    async fn execute_flatten(
        &self,
        state: &mut RuntimeState,
        reason: &str,
        intent: &FlattenIntent,
    ) -> Result<()> {
        state.flags.paused = true;
        state.flags.flattening = true;
        warn!(
            target: "lp.flatten",
            reason = %reason,
            asset_id = %intent.asset_id,
            side = %intent.side,
            size = %intent.size,
            use_fok = intent.use_fok,
            "submitting flatten order"
        );
        match self.exchange.flatten(intent).await {
            Ok(Some(fill)) => {
                self.persist_trade(state, &fill).await?;
                state.fills.push(fill.clone());
                info!(
                    target: "lp.flatten",
                    reason = %reason,
                    trade_id = %fill.trade_id,
                    order_id = fill.order_id.as_deref().unwrap_or(""),
                    asset_id = %fill.asset_id,
                    side = %fill.side,
                    price = %fill.price,
                    size = %fill.size,
                    status = %fill.status,
                    "flatten order acknowledged"
                );
                self.emit_risk(
                    "flatten_submitted",
                    "warn",
                    json!({
                        "reason": reason,
                        "asset_id": intent.asset_id,
                        "side": intent.side.to_string(),
                        "size": intent.size.to_string(),
                    }),
                )
                .await?;
            }
            Ok(None) => {
                warn!(
                    target: "lp.flatten",
                    reason = %reason,
                    asset_id = %intent.asset_id,
                    side = %intent.side,
                    size = %intent.size,
                    "flatten submitted without immediate fill details"
                );
                self.emit_risk(
                    "flatten_submitted",
                    "warn",
                    json!({
                        "reason": reason,
                        "asset_id": intent.asset_id,
                        "side": intent.side.to_string(),
                        "size": intent.size.to_string(),
                    }),
                )
                .await?;
            }
            Err(error) => {
                error!(
                    target: "lp.flatten",
                    reason = %reason,
                    asset_id = %intent.asset_id,
                    side = %intent.side,
                    size = %intent.size,
                    error = %error,
                    "flatten submission failed"
                );
                self.emit_risk(
                    "flatten_error",
                    "error",
                    json!({
                        "reason": reason,
                        "asset_id": intent.asset_id,
                        "side": intent.side.to_string(),
                        "size": intent.size.to_string(),
                        "error": error.to_string(),
                    }),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn recompute_quotes(&self, state: &mut RuntimeState) -> Result<()> {
        let previous_reason = state.last_decision_reason.clone();
        let outcome = self.decision.evaluate(state);
        state.last_decision_reason = Some(outcome.reason.clone());
        if previous_reason.as_deref() != Some(outcome.reason.as_str()) {
            info!(
                target: "lp.decision",
                reason = %outcome.reason,
                cancel_all = outcome.cancel_all,
                desired_quotes = outcome.desired_quotes.len(),
                quotes = %summarize_quotes(&outcome.desired_quotes),
                "decision updated"
            );
        }

        if outcome.cancel_all {
            if !state.open_orders.is_empty() {
                warn!(
                    target: "lp.quote",
                    open_orders = state.open_orders.len(),
                    reason = %outcome.reason,
                    "decision requested quote cancellation"
                );
                self.exchange.cancel_all().await?;
                self.mark_orders_status(&state.market.condition_id, &state.open_orders, "CANCELED")
                    .await?;
                state.open_orders.clear();
            }
            return Ok(());
        }

        if desired_quotes_match(state, &outcome, self.config.max_quote_age) {
            return Ok(());
        }

        if !state.open_orders.is_empty() {
            info!(
                target: "lp.quote",
                open_orders = state.open_orders.len(),
                "refreshing existing quotes before reposting"
            );
            self.exchange.cancel_all().await?;
            self.mark_orders_status(&state.market.condition_id, &state.open_orders, "CANCELED")
                .await?;
            state.open_orders.clear();
        }

        if outcome.desired_quotes.is_empty() {
            return Ok(());
        }

        info!(
            target: "lp.quote",
            desired_quotes = outcome.desired_quotes.len(),
            quotes = %summarize_quotes(&outcome.desired_quotes),
            "submitting passive quotes"
        );
        let posted = match self.exchange.post_quotes(&outcome.desired_quotes).await {
            Ok(posted) => posted,
            Err(error) => {
                error!(
                    target: "lp.quote",
                    desired_quotes = outcome.desired_quotes.len(),
                    quotes = %summarize_quotes(&outcome.desired_quotes),
                    error = %error,
                    "quote submission failed"
                );
                return Err(error);
            }
        };
        for order in &posted {
            let reason = outcome
                .desired_quotes
                .iter()
                .find(|quote| quote.asset_id == order.asset_id && quote.side == order.side)
                .map(|quote| quote.reason.as_str());
            self.persist_order(state, order, reason).await?;
            info!(
                target: "lp.quote",
                order_id = %order.order_id,
                asset_id = %order.asset_id,
                side = %order.side,
                price = %order.price,
                size = %order.size,
                status = %order.status,
                "passive quote acknowledged"
            );
        }
        state.open_orders = posted;

        Ok(())
    }

    async fn persist_order(
        &self,
        state: &RuntimeState,
        order: &ManagedOrder,
        reason: Option<&str>,
    ) -> Result<()> {
        upsert_lp_order(
            &self.pool,
            &order.order_id,
            None,
            &state.market.condition_id,
            &order.asset_id,
            &order.side.to_string(),
            &order.price.to_string(),
            &order.size.to_string(),
            &order.status,
            reason,
        )
        .await?;
        Ok(())
    }

    async fn persist_trade(&self, state: &RuntimeState, fill: &TradeFill) -> Result<()> {
        upsert_lp_trade(
            &self.pool,
            &fill.trade_id,
            fill.order_id.as_deref(),
            &state.market.condition_id,
            &fill.asset_id,
            &fill.side.to_string(),
            &fill.price.to_string(),
            &fill.size.to_string(),
            &fill.status,
        )
        .await?;
        Ok(())
    }

    async fn mark_orders_status(
        &self,
        condition_id: &str,
        orders: &[ManagedOrder],
        status: &str,
    ) -> Result<()> {
        for order in orders {
            upsert_lp_order(
                &self.pool,
                &order.order_id,
                None,
                condition_id,
                &order.asset_id,
                &order.side.to_string(),
                &order.price.to_string(),
                &order.size.to_string(),
                status,
                None,
            )
            .await?;
        }

        Ok(())
    }

    async fn send_periodic_report(&self, state: &RuntimeState) -> Result<()> {
        let payload = json!({
            "condition_id": state.market.condition_id,
            "question": state.market.question,
            "paused": state.flags.paused,
            "flattening": state.flags.flattening,
            "heartbeat_healthy": state.flags.heartbeat_healthy,
            "market_feed_healthy": state.flags.market_feed_healthy,
            "user_feed_healthy": state.flags.user_feed_healthy,
            "open_orders": state.open_orders.len(),
            "positions": state.positions.values().map(|position| json!({
                "asset_id": position.asset_id,
                "size": position.size.to_string(),
                "avg_price": position.avg_price.to_string(),
            })).collect::<Vec<_>>(),
            "usdc_balance": state.account.usdc_balance.to_string(),
            "last_decision_reason": state.last_decision_reason,
        });
        let rendered = serde_json::to_string_pretty(&payload)?;
        insert_lp_report(&self.pool, "summary", &rendered).await?;
        info!(target: "lp.report", report_type = "summary", payload = %rendered, "operator summary generated");
        if let Some(reporter) = &self.reporter {
            reporter.send("summary", &rendered).await?;
        }
        Ok(())
    }

    async fn persist_positions_snapshot(
        &self,
        state: &RuntimeState,
        snapshot_type: &str,
    ) -> Result<()> {
        if state.positions.is_empty() {
            for token in &state.market.tokens {
                insert_lp_position_snapshot(
                    &self.pool,
                    &state.market.condition_id,
                    &token.asset_id,
                    "0",
                    "0",
                    &state.account.usdc_balance.to_string(),
                    snapshot_type,
                )
                .await?;
            }
            return Ok(());
        }

        for position in state.positions.values() {
            insert_lp_position_snapshot(
                &self.pool,
                &state.market.condition_id,
                &position.asset_id,
                &position.size.to_string(),
                &position.avg_price.to_string(),
                &state.account.usdc_balance.to_string(),
                snapshot_type,
            )
            .await?;
        }

        Ok(())
    }

    async fn emit_risk(
        &self,
        event_type: &str,
        severity: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        let payload = serde_json::to_string(&details)?;
        insert_lp_risk_event(&self.pool, event_type, severity, &payload).await?;
        match severity {
            "error" => error!(target: "lp.risk", event_type = %event_type, details = %payload, "risk event"),
            "warn" => warn!(target: "lp.risk", event_type = %event_type, details = %payload, "risk event"),
            _ => info!(target: "lp.risk", event_type = %event_type, details = %payload, "risk event"),
        }
        if let Some(reporter) = &self.reporter {
            reporter.send(event_type, &payload).await?;
        }
        Ok(())
    }

    fn refresh_health_flags(&self, state: &mut RuntimeState) {
        let now = Utc::now();
        state.flags.market_feed_healthy = state
            .last_market_event_at
            .map(|timestamp| (now - timestamp) <= self.config.risk.stale_feed_after)
            .unwrap_or(false);
        state.flags.user_feed_healthy = state
            .last_user_event_at
            .map(|timestamp| (now - timestamp) <= self.config.risk.stale_feed_after)
            .unwrap_or(false);
    }

    fn publish_snapshot(&self, state: &RuntimeState) {
        if self.snapshot_tx.send(state.clone()).is_err() {
            warn!("LP snapshot receiver dropped");
        }
    }

    fn log_signal_transitions(
        &self,
        before: &HashMap<String, SignalState>,
        after: &HashMap<String, SignalState>,
    ) {
        for transition in signal_transitions(before, after) {
            let previous_active = transition
                .previous_active
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string());
            let previous_reason = transition
                .previous_reason
                .clone()
                .unwrap_or_else(|| "none".to_string());
            let level = if transition.active { "enabled" } else { "disabled" };
            info!(
                target: "lp.signal",
                signal = %transition.name,
                active = transition.active,
                previous_active = %previous_active,
                reason = %transition.reason,
                previous_reason = %previous_reason,
                "signal {level}"
            );
        }
    }

    fn flatten_intents_from_positions(&self, state: &RuntimeState) -> Vec<FlattenIntent> {
        state
            .positions
            .values()
            .filter(|position| position.size.abs() > self.config.risk.flat_position_tolerance)
            .map(|position| FlattenIntent {
                asset_id: position.asset_id.clone(),
                side: if position.size.is_sign_negative() {
                    QuoteSide::Buy
                } else {
                    QuoteSide::Sell
                },
                size: position.size.abs(),
                use_fok: self.config.risk.flatten_use_fok,
            })
            .collect()
    }
}

fn upsert_order_state(state: &mut RuntimeState, order: ManagedOrder) {
    let terminal = matches!(order.status.as_str(), "CANCELED" | "MATCHED");
    if terminal {
        state.open_orders.retain(|existing| existing.order_id != order.order_id);
    } else if let Some(existing) = state
        .open_orders
        .iter_mut()
        .find(|existing| existing.order_id == order.order_id)
    {
        *existing = order;
    } else {
        state.open_orders.push(order);
    }
}

fn desired_quotes_match(
    state: &RuntimeState,
    outcome: &DecisionOutcome,
    max_quote_age: Duration,
) -> bool {
    if state.open_orders.len() != outcome.desired_quotes.len() {
        return false;
    }

    let now = Utc::now();
    for order in &state.open_orders {
        if (now - order.created_at)
            .to_std()
            .map(|age| age > max_quote_age)
            .unwrap_or(true)
        {
            return false;
        }

        let matched = outcome.desired_quotes.iter().any(|quote| {
            quote.asset_id == order.asset_id
                && quote.side == order.side
                && quote.price == order.price
                && quote.size == order.size
        });
        if !matched {
            return false;
        }
    }

    true
}

impl std::fmt::Display for QuoteSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buy => f.write_str("BUY"),
            Self::Sell => f.write_str("SELL"),
        }
    }
}
