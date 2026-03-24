use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use pn_common::db::{
    insert_lp_control_action, insert_lp_heartbeat, insert_lp_position_snapshot, insert_lp_report,
    insert_lp_risk_event, upsert_lp_order, upsert_lp_trade,
};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::control::{ControlCommand, LpControlHandle};
use crate::decision::{DecisionConfig, DecisionDiagnostics, DecisionEngine, DecisionOutcome};
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
    pub books: Vec<BookSnapshot>,
    pub open_orders: Vec<ManagedOrder>,
    pub positions: Vec<PositionSnapshot>,
    pub account: AccountSnapshot,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub decision: DecisionConfig,
    pub risk: RiskConfig,
    pub startup_split_amount: Option<Decimal>,
    pub heartbeat_interval: Duration,
    pub reconciliation_interval: Duration,
    pub report_interval: Duration,
    pub snapshot_interval: Duration,
    pub max_quote_age: Duration,
    pub reward_refresh_interval: Duration,
}

const BOOK_ACTIVITY_LOG_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq)]
struct BookActivitySnapshot {
    asset_id: String,
    updates: u64,
    avg_gap_ms: Option<i64>,
    max_gap_ms: Option<i64>,
    processing_lag_ms: i64,
    min_top_depth: Decimal,
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
}

#[derive(Debug, Clone)]
struct BookActivityWindow {
    started_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    updates: u64,
    total_gap_ms: i64,
    gap_samples: u64,
    max_gap_ms: i64,
}

impl BookActivityWindow {
    fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            last_seen_at: started_at,
            updates: 0,
            total_gap_ms: 0,
            gap_samples: 0,
            max_gap_ms: 0,
        }
    }
}

#[derive(Debug, Default)]
struct BookActivityTracker {
    windows: HashMap<String, BookActivityWindow>,
}

impl BookActivityTracker {
    fn record(
        &mut self,
        book: &BookSnapshot,
        now: DateTime<Utc>,
        log_interval: Duration,
    ) -> Option<BookActivitySnapshot> {
        let window = self
            .windows
            .entry(book.asset_id.clone())
            .or_insert_with(|| BookActivityWindow::new(book.received_at));

        if window.updates > 0 {
            let gap_ms = (book.received_at - window.last_seen_at)
                .num_milliseconds()
                .max(0);
            window.total_gap_ms += gap_ms;
            window.gap_samples += 1;
            window.max_gap_ms = window.max_gap_ms.max(gap_ms);
        }

        window.updates += 1;
        window.last_seen_at = book.received_at;

        let elapsed = now
            .signed_duration_since(window.started_at)
            .to_std()
            .unwrap_or_default();
        if elapsed < log_interval {
            return None;
        }

        let snapshot = BookActivitySnapshot {
            asset_id: book.asset_id.clone(),
            updates: window.updates,
            avg_gap_ms: (window.gap_samples > 0)
                .then(|| window.total_gap_ms / window.gap_samples as i64),
            max_gap_ms: (window.gap_samples > 0).then_some(window.max_gap_ms),
            processing_lag_ms: now
                .signed_duration_since(book.received_at)
                .num_milliseconds()
                .max(0),
            min_top_depth: book.min_top_depth(),
            best_bid: book.best_bid().map(|level| level.price),
            best_ask: book.best_ask().map(|level| level.price),
        };

        *window = BookActivityWindow::new(book.received_at);

        Some(snapshot)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct LoopPostActions {
    refresh_health_flags: bool,
    apply_timer_risk: bool,
    recompute_quotes: bool,
    publish_snapshot: bool,
}

impl LoopPostActions {
    fn quote_path() -> Self {
        Self {
            refresh_health_flags: true,
            apply_timer_risk: false,
            recompute_quotes: true,
            publish_snapshot: true,
        }
    }

    fn timer_quote_path() -> Self {
        Self {
            refresh_health_flags: true,
            apply_timer_risk: true,
            recompute_quotes: true,
            publish_snapshot: true,
        }
    }
}

#[async_trait]
pub trait ExchangeAdapter: Send + Sync {
    fn start(&self, event_tx: mpsc::UnboundedSender<ExchangeEvent>, cancel: CancellationToken);

    async fn reconcile(&self) -> Result<ReconciliationSnapshot>;

    async fn fetch_reward_snapshot(&self) -> Result<Option<crate::types::RewardSnapshot>>;

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
    last_decision_diagnostics: Mutex<Option<DecisionDiagnostics>>,
    book_activity: Mutex<BookActivityTracker>,
    book_activity_log_interval: Duration,
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
                last_decision_diagnostics: Mutex::new(None),
                book_activity: Mutex::new(BookActivityTracker::default()),
                book_activity_log_interval: BOOK_ACTIVITY_LOG_INTERVAL,
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

        let mut reward_tick = interval(self.config.reward_refresh_interval);
        reward_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        reward_tick.tick().await;

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
        if let Some(amount) = self
            .config
            .startup_split_amount
            .filter(|amount| *amount > Decimal::ZERO)
        {
            self.handle_control_command(
                &mut state,
                ControlCommand::Split {
                    amount: amount.to_string(),
                    reason: "startup auto split".to_string(),
                },
                "startup",
            )
            .await?;
        }
        self.handle_heartbeat_tick(&mut state).await?;
        if let Err(error) = self.refresh_reward_state(&mut state).await {
            error!(?error, "startup reward refresh failed");
        }
        self.refresh_health_flags(&mut state);
        self.recompute_quotes(&mut state).await?;
        self.publish_snapshot(&state);

        loop {
            let mut post_actions = LoopPostActions::default();
            tokio::select! {
                () = cancel.cancelled() => break,
                maybe_cmd = self.control_rx.recv() => {
                    post_actions = LoopPostActions::quote_path();
                    match maybe_cmd {
                        Some(command) => {
                            if let Err(error) = self
                                .handle_control_command(&mut state, command, "admin_api")
                                .await
                            {
                                error!(?error, "control command handler failed");
                            }
                        }
                        None => break,
                    }
                }
                maybe_event = event_rx.recv() => {
                    post_actions = LoopPostActions::quote_path();
                    match maybe_event {
                        Some(event) => {
                            if let Err(error) = self.handle_exchange_event(&mut state, event).await {
                                error!(?error, "exchange event handler failed");
                            }
                        }
                        None => break,
                    }
                }
                _ = heartbeat_tick.tick() => {
                    post_actions = LoopPostActions::timer_quote_path();
                    if let Err(error) = self.handle_heartbeat_tick(&mut state).await {
                        error!(?error, "heartbeat tick failed");
                    }
                }
                _ = reconcile_tick.tick() => {
                    post_actions = LoopPostActions::timer_quote_path();
                    if let Err(error) = self.handle_reconciliation_tick(&mut state).await {
                        error!(?error, "reconciliation tick failed");
                    }
                }
                _ = reward_tick.tick() => {
                    post_actions = LoopPostActions::quote_path();
                    if let Err(error) = self.refresh_reward_state(&mut state).await {
                        error!(?error, "reward refresh tick failed");
                    }
                }
                _ = report_tick.tick() => {
                    if let Err(error) = self.send_periodic_report(&state).await {
                        error!(?error, "periodic report failed");
                    }
                }
                _ = snapshot_tick.tick() => {
                    if let Err(error) = self.persist_positions_snapshot(&state, "periodic").await {
                        error!(?error, "positions snapshot failed");
                    }
                }
            }

            if post_actions.refresh_health_flags {
                self.refresh_health_flags(&mut state);
            }
            if post_actions.apply_timer_risk {
                if let Err(error) = self.apply_timer_risk(&mut state).await {
                    error!(?error, "timer risk check failed");
                }
            }
            if post_actions.recompute_quotes {
                if let Err(error) = self.recompute_quotes(&mut state).await {
                    error!(?error, "quote recomputation failed");
                }
            }
            if post_actions.publish_snapshot {
                self.publish_snapshot(&state);
            }
        }

        info!("LP service stopping");
        if !state.open_orders.is_empty() {
            match self.exchange.cancel_all().await {
                Ok(()) => {
                    let canceled_orders = state.open_orders.clone();
                    remember_terminal_orders(&mut state, &canceled_orders);
                    if let Err(error) = self
                        .mark_orders_status(
                            &state.market.condition_id,
                            &canceled_orders,
                            "CANCELED",
                        )
                        .await
                    {
                        error!(?error, "failed to persist shutdown cancel status");
                    }
                    state.open_orders.clear();
                }
                Err(error) => error!(?error, "failed to cancel orders during shutdown"),
            }
        }
        if let Err(error) = self.persist_positions_snapshot(&state, "shutdown").await {
            error!(?error, "failed to persist final position snapshot");
        }
        info!("LP service stopped");
        Ok(())
    }

    async fn refresh_reward_state(&self, state: &mut RuntimeState) -> Result<()> {
        let attempted_at = Utc::now();
        state.reward.last_attempt_at = Some(attempted_at);

        match self.exchange.fetch_reward_snapshot().await {
            Ok(snapshot) => {
                let was_active = state.reward.snapshot.is_some();
                let is_active = snapshot.is_some();
                state.reward.snapshot = snapshot;
                state.reward.last_success_at = Some(attempted_at);
                state.reward.last_error = None;
                info!(
                    target: "lp.reward",
                    was_active,
                    is_active,
                    "reward state refreshed"
                );
                Ok(())
            }
            Err(error) => {
                state.reward.snapshot = None;
                state.reward.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    async fn handle_control_command(
        &self,
        state: &mut RuntimeState,
        command: ControlCommand,
        actor: &str,
    ) -> Result<()> {
        let aggregator = SignalAggregator;
        match command {
            ControlCommand::Pause { reason } => {
                warn!(target: "lp.control", reason = %reason, "pause requested");
                insert_lp_control_action(&self.pool, "pause", Some(&reason), actor).await?;
                state.flags.paused = true;
                self.emit_risk("control_pause", "info", json!({ "reason": reason }))
                    .await?;
            }
            ControlCommand::Resume { reason } => {
                info!(target: "lp.control", reason = %reason, "resume requested");
                insert_lp_control_action(&self.pool, "resume", Some(&reason), actor).await?;
                state.flags.paused = false;
                state.flags.flattening = false;
                self.emit_risk("control_resume", "info", json!({ "reason": reason }))
                    .await?;
            }
            ControlCommand::CancelAll { reason } => {
                warn!(target: "lp.control", reason = %reason, open_orders = state.open_orders.len(), "cancel-all requested");
                insert_lp_control_action(&self.pool, "cancel_all", Some(&reason), actor).await?;
                self.exchange.cancel_all().await?;
                let canceled_orders = state.open_orders.clone();
                remember_terminal_orders(state, &canceled_orders);
                self.mark_orders_status(&state.market.condition_id, &canceled_orders, "CANCELED")
                    .await?;
                state.open_orders.clear();
                self.emit_risk("control_cancel_all", "info", json!({ "reason": reason }))
                    .await?;
            }
            ControlCommand::Flatten { reason } => {
                warn!(target: "lp.control", reason = %reason, positions = state.positions.len(), "flatten requested");
                insert_lp_control_action(&self.pool, "flatten", Some(&reason), actor).await?;
                state.flags.paused = true;
                state.flags.flattening = true;
                self.exchange.cancel_all().await?;
                let canceled_orders = state.open_orders.clone();
                remember_terminal_orders(state, &canceled_orders);
                self.mark_orders_status(&state.market.condition_id, &canceled_orders, "CANCELED")
                    .await?;
                state.open_orders.clear();

                for intent in self.flatten_intents_from_positions(state) {
                    self.execute_flatten(state, &reason, &intent).await?;
                }
            }
            ControlCommand::Split { amount, reason } => {
                info!(target: "lp.control", reason = %reason, amount = %amount, "split requested");
                insert_lp_control_action(&self.pool, "split", Some(&reason), actor).await?;
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
                insert_lp_control_action(&self.pool, "merge", Some(&reason), actor).await?;
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
                insert_lp_control_action(
                    &self.pool,
                    &format!("signal:{name}"),
                    Some(&reason),
                    actor,
                )
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
                self.observe_book_activity(&book, now);
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
                apply_fill_to_positions(state, &fill);
                let actions = self.risk.on_fill(&fill, state);
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
        let previous_open_orders = state.open_orders.clone();
        let snapshot = self.exchange.reconcile().await?;
        let reconciled_missing_orders =
            missing_open_orders(&previous_open_orders, &snapshot.open_orders);
        if !snapshot.books.is_empty() {
            for book in snapshot.books {
                state.books.insert(book.asset_id.clone(), book);
            }
            state.last_market_event_at = Some(Utc::now());
        }
        state.account = snapshot.account;
        state.positions = snapshot
            .positions
            .into_iter()
            .map(|position| (position.asset_id.clone(), position))
            .collect::<HashMap<_, _>>();
        if !reconciled_missing_orders.is_empty() {
            remember_terminal_orders(state, &reconciled_missing_orders);
            self.mark_orders_status(
                &state.market.condition_id,
                &reconciled_missing_orders,
                "RECONCILED_MISSING",
            )
            .await?;
        }
        replace_open_orders(state, snapshot.open_orders);
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
                    insert_lp_control_action(&self.pool, "pause", Some(&reason), "risk_engine")
                        .await?;
                    state.flags.paused = true;
                    self.emit_risk("pause", "warn", json!({ "reason": reason }))
                        .await?;
                }
                RiskAction::Resume { reason } => {
                    info!(target: "lp.risk", reason = %reason, "risk resume triggered");
                    insert_lp_control_action(&self.pool, "resume", Some(&reason), "risk_engine")
                        .await?;
                    state.flags.paused = false;
                    state.flags.flattening = false;
                    self.emit_risk("resume", "info", json!({ "reason": reason }))
                        .await?;
                }
                RiskAction::CancelAll { reason } => {
                    warn!(target: "lp.risk", reason = %reason, open_orders = state.open_orders.len(), "risk cancel-all triggered");
                    insert_lp_control_action(
                        &self.pool,
                        "cancel_all",
                        Some(&reason),
                        "risk_engine",
                    )
                    .await?;
                    self.exchange.cancel_all().await?;
                    let canceled_orders = state.open_orders.clone();
                    remember_terminal_orders(state, &canceled_orders);
                    self.mark_orders_status(
                        &state.market.condition_id,
                        &canceled_orders,
                        "CANCELED",
                    )
                    .await?;
                    state.open_orders.clear();
                    self.emit_risk("cancel_all", "warn", json!({ "reason": reason }))
                        .await?;
                }
                RiskAction::Flatten(mut intent) => {
                    intent.price = flatten_reference_price(state, &intent.asset_id, intent.price);
                    insert_lp_control_action(
                        &self.pool,
                        "flatten",
                        Some("risk flatten"),
                        "risk_engine",
                    )
                    .await?;
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
                state.flags.flattening = false;
                self.persist_trade(state, &fill).await?;
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
                state.flags.flattening = false;
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
                state.flags.flattening = false;
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
        let diagnostics_changed = {
            let mut previous = self
                .last_decision_diagnostics
                .lock()
                .expect("decision diagnostics mutex");
            let changed = previous.as_ref() != Some(&outcome.diagnostics);
            *previous = Some(outcome.diagnostics.clone());
            changed
        };
        let should_log_decision =
            previous_reason.as_deref() != Some(outcome.reason.as_str()) || diagnostics_changed;
        let should_log_cancel = outcome.cancel_all && !state.open_orders.is_empty();
        let diagnostics = if should_log_decision || should_log_cancel {
            Some(
                serde_json::to_string(&outcome.diagnostics)
                    .unwrap_or_else(|error| format!("{{\"serialization_error\":\"{error}\"}}")),
            )
        } else {
            None
        };
        if should_log_decision {
            info!(
                target: "lp.decision",
                reason = %outcome.reason,
                cancel_all = outcome.cancel_all,
                desired_quotes = outcome.desired_quotes.len(),
                quotes = %summarize_quotes(&outcome.desired_quotes),
                diagnostics = %diagnostics.as_deref().unwrap_or("{}"),
                "decision updated"
            );
        }

        if outcome.cancel_all {
            if !state.open_orders.is_empty() {
                warn!(
                    target: "lp.quote",
                    open_orders = state.open_orders.len(),
                    reason = %outcome.reason,
                    diagnostics = %diagnostics.as_deref().unwrap_or("{}"),
                    "decision requested quote cancellation"
                );
                self.exchange.cancel_all().await?;
                let canceled_orders = state.open_orders.clone();
                remember_terminal_orders(state, &canceled_orders);
                self.mark_orders_status(&state.market.condition_id, &canceled_orders, "CANCELED")
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
            let canceled_orders = state.open_orders.clone();
            remember_terminal_orders(state, &canceled_orders);
            self.mark_orders_status(&state.market.condition_id, &canceled_orders, "CANCELED")
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
        replace_open_orders(state, posted);

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
            "error" => {
                error!(target: "lp.risk", event_type = %event_type, details = %payload, "risk event")
            }
            "warn" => {
                warn!(target: "lp.risk", event_type = %event_type, details = %payload, "risk event")
            }
            _ => {
                info!(target: "lp.risk", event_type = %event_type, details = %payload, "risk event")
            }
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

    fn observe_book_activity(&self, book: &BookSnapshot, now: DateTime<Utc>) {
        let snapshot = self
            .book_activity
            .lock()
            .expect("book activity mutex")
            .record(book, now, self.book_activity_log_interval);

        if let Some(snapshot) = snapshot {
            info!(
                target: "lp.book",
                asset_id = %snapshot.asset_id,
                updates = snapshot.updates,
                avg_gap_ms = snapshot.avg_gap_ms.unwrap_or_default(),
                max_gap_ms = snapshot.max_gap_ms.unwrap_or_default(),
                processing_lag_ms = snapshot.processing_lag_ms,
                min_top_depth = %snapshot.min_top_depth,
                best_bid = ?snapshot.best_bid,
                best_ask = ?snapshot.best_ask,
                "aggregated book activity"
            );
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
            let level = if transition.active {
                "enabled"
            } else {
                "disabled"
            };
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
                price: flatten_reference_price(state, &position.asset_id, position.avg_price),
                size: position.size.abs(),
                use_fok: self.config.risk.flatten_use_fok,
            })
            .collect()
    }
}

fn upsert_order_state(state: &mut RuntimeState, order: ManagedOrder) {
    if is_terminal_order_status(&order.status) {
        state.terminal_order_ids.insert(order.order_id.clone());
        state
            .open_orders
            .retain(|existing| existing.order_id != order.order_id);
    } else if state.terminal_order_ids.contains(&order.order_id) {
        return;
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

fn is_terminal_order_status(status: &str) -> bool {
    matches!(status, "CANCELED" | "MATCHED")
}

fn remember_terminal_orders(state: &mut RuntimeState, orders: &[ManagedOrder]) {
    for order in orders {
        state.terminal_order_ids.insert(order.order_id.clone());
    }
}

fn replace_open_orders(state: &mut RuntimeState, orders: Vec<ManagedOrder>) {
    for order in &orders {
        state.terminal_order_ids.remove(&order.order_id);
    }
    state.open_orders = orders;
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

fn missing_open_orders(
    previous_open_orders: &[ManagedOrder],
    current_open_orders: &[ManagedOrder],
) -> Vec<ManagedOrder> {
    previous_open_orders
        .iter()
        .filter(|previous| {
            current_open_orders
                .iter()
                .all(|current| current.order_id != previous.order_id)
        })
        .cloned()
        .collect()
}

fn flatten_reference_price(
    state: &RuntimeState,
    asset_id: &str,
    fallback_price: Decimal,
) -> Decimal {
    match state.books.get(asset_id) {
        Some(book) => match (book.best_bid(), book.best_ask()) {
            (Some(best_bid), Some(best_ask)) => {
                (best_bid.price + best_ask.price) / Decimal::from(2)
            }
            (Some(best_bid), None) => best_bid.price,
            (None, Some(best_ask)) => best_ask.price,
            (None, None) => fallback_price,
        },
        None => fallback_price,
    }
}

fn apply_fill_to_positions(state: &mut RuntimeState, fill: &TradeFill) {
    let position = state
        .positions
        .entry(fill.asset_id.clone())
        .or_insert_with(|| PositionSnapshot {
            asset_id: fill.asset_id.clone(),
            size: Decimal::ZERO,
            avg_price: Decimal::ZERO,
        });

    let delta = match fill.side {
        QuoteSide::Buy => fill.size,
        QuoteSide::Sell => -fill.size,
    };
    let previous_size = position.size;
    let new_size = previous_size + delta;
    position.size = new_size;

    if new_size == Decimal::ZERO {
        position.avg_price = Decimal::ZERO;
        return;
    }

    let extended_same_direction = (previous_size > Decimal::ZERO && delta > Decimal::ZERO)
        || (previous_size < Decimal::ZERO && delta < Decimal::ZERO);
    let flipped_direction = previous_size != Decimal::ZERO
        && previous_size.is_sign_positive() != new_size.is_sign_positive();

    if previous_size == Decimal::ZERO || flipped_direction {
        position.avg_price = fill.price;
        return;
    }

    if extended_same_direction {
        let previous_abs = previous_size.abs();
        let delta_abs = delta.abs();
        position.avg_price =
            ((position.avg_price * previous_abs) + (fill.price * delta_abs)) / new_size.abs();
    }
}

impl std::fmt::Display for QuoteSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buy => f.write_str("BUY"),
            Self::Sell => f.write_str("SELL"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::Utc;
    use pn_common::db::init_db;
    use rust_decimal_macros::dec;
    use sqlx::SqlitePool;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        desired_quotes_match, flatten_reference_price, missing_open_orders, upsert_order_state,
        BookActivityTracker, ExchangeAdapter, ExchangeEvent, LpService, ReconciliationSnapshot,
        ServiceConfig,
    };
    use crate::control::ControlCommand;
    use crate::decision::{DecisionConfig, DecisionEngine, DecisionOutcome};
    use crate::risk::{FlattenIntent, RiskConfig};
    use crate::types::{
        AccountSnapshot, BookLevel, BookSnapshot, ManagedOrder, MarketMetadata, PositionSnapshot,
        QuoteIntent, QuoteSide, RewardSnapshot, RewardState, RuntimeFlags, RuntimeState,
        SignalState, TokenMetadata, TradeFill,
    };

    fn runtime_state_with_book() -> RuntimeState {
        let now = Utc::now();
        RuntimeState {
            market: MarketMetadata {
                condition_id: "condition-1".to_string(),
                question: "Will X happen?".to_string(),
                tokens: vec![TokenMetadata {
                    asset_id: "asset-yes".to_string(),
                    outcome: "Yes".to_string(),
                    tick_size: dec!(0.01),
                }],
            },
            books: HashMap::from([(
                "asset-yes".to_string(),
                BookSnapshot {
                    asset_id: "asset-yes".to_string(),
                    bids: vec![BookLevel {
                        price: dec!(0.40),
                        size: dec!(100),
                    }],
                    asks: vec![BookLevel {
                        price: dec!(0.46),
                        size: dec!(100),
                    }],
                    received_at: now,
                },
            )]),
            open_orders: Vec::new(),
            terminal_order_ids: std::collections::HashSet::new(),
            positions: HashMap::new(),
            account: AccountSnapshot {
                usdc_balance: dec!(500),
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
            flags: RuntimeFlags {
                heartbeat_healthy: true,
                market_feed_healthy: true,
                user_feed_healthy: true,
                ..RuntimeFlags::default()
            },
            last_market_event_at: Some(now),
            last_user_event_at: Some(now),
            last_heartbeat_at: Some(now),
            last_heartbeat_id: Some("hb-1".to_string()),
            last_decision_reason: None,
            reward: RewardState {
                snapshot: Some(RewardSnapshot {
                    condition_id: "condition-1".to_string(),
                    max_spread: dec!(0.03),
                    min_size: dec!(50),
                    total_daily_rate: dec!(4.5),
                    active_until: chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
                    token_prices: HashMap::from([("asset-yes".to_string(), dec!(0.43))]),
                }),
                last_attempt_at: Some(now),
                last_success_at: Some(now),
                last_error: None,
            },
        }
    }

    fn placeholder_decision_diagnostics() -> crate::decision::DecisionDiagnostics {
        DecisionEngine::new(service_config().decision.clone())
            .evaluate(&runtime_state_with_book())
            .diagnostics
    }

    #[test]
    fn flatten_reference_price_prefers_mid_price_and_falls_back() {
        let state = runtime_state_with_book();

        assert_eq!(
            flatten_reference_price(&state, "asset-yes", dec!(0.30)),
            dec!(0.43)
        );
        assert_eq!(
            flatten_reference_price(&state, "missing-asset", dec!(0.30)),
            dec!(0.30)
        );
    }

    #[test]
    fn missing_open_orders_returns_only_orders_absent_from_snapshot() {
        let now = Utc::now();
        let previous = vec![
            ManagedOrder {
                order_id: "order-1".to_string(),
                asset_id: "asset-yes".to_string(),
                side: crate::types::QuoteSide::Buy,
                price: dec!(0.40),
                size: dec!(10),
                created_at: now,
                status: "LIVE".to_string(),
            },
            ManagedOrder {
                order_id: "order-2".to_string(),
                asset_id: "asset-yes".to_string(),
                side: crate::types::QuoteSide::Sell,
                price: dec!(0.46),
                size: dec!(10),
                created_at: now,
                status: "LIVE".to_string(),
            },
        ];
        let current = vec![previous[1].clone()];

        let missing = missing_open_orders(&previous, &current);

        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].order_id, "order-1");
    }

    #[test]
    fn upsert_order_state_ignores_late_live_update_after_cancel() {
        let now = Utc::now();
        let mut state = runtime_state_with_book();
        state.open_orders = vec![ManagedOrder {
            order_id: "order-2".to_string(),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Buy,
            price: dec!(0.08),
            size: dec!(25),
            created_at: now,
            status: "LIVE".to_string(),
        }];

        upsert_order_state(
            &mut state,
            ManagedOrder {
                order_id: "order-1".to_string(),
                asset_id: "asset-yes".to_string(),
                side: QuoteSide::Buy,
                price: dec!(0.09),
                size: dec!(25),
                created_at: now,
                status: "CANCELED".to_string(),
            },
        );
        upsert_order_state(
            &mut state,
            ManagedOrder {
                order_id: "order-1".to_string(),
                asset_id: "asset-yes".to_string(),
                side: QuoteSide::Buy,
                price: dec!(0.09),
                size: dec!(25),
                created_at: now,
                status: "LIVE".to_string(),
            },
        );

        assert_eq!(state.open_orders.len(), 1);
        assert_eq!(state.open_orders[0].order_id, "order-2");
    }

    #[test]
    fn book_activity_tracker_emits_aggregated_frequency_snapshot() {
        let received_at = Utc::now();
        let mut tracker = BookActivityTracker::default();
        let mut book = BookSnapshot {
            asset_id: "asset-yes".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.40),
                size: dec!(30),
            }],
            asks: vec![BookLevel {
                price: dec!(0.46),
                size: dec!(40),
            }],
            received_at,
        };

        assert!(tracker
            .record(&book, received_at, Duration::from_secs(1),)
            .is_none());

        book.received_at = received_at + chrono::Duration::milliseconds(200);
        assert!(tracker
            .record(
                &book,
                received_at + chrono::Duration::milliseconds(200),
                Duration::from_secs(1),
            )
            .is_none());

        book.received_at = received_at + chrono::Duration::milliseconds(1200);
        let snapshot = tracker
            .record(
                &book,
                received_at + chrono::Duration::milliseconds(1210),
                Duration::from_secs(1),
            )
            .expect("window snapshot");

        assert_eq!(snapshot.asset_id, "asset-yes");
        assert_eq!(snapshot.updates, 3);
        assert_eq!(snapshot.avg_gap_ms, Some(600));
        assert_eq!(snapshot.max_gap_ms, Some(1000));
        assert_eq!(snapshot.processing_lag_ms, 10);
        assert_eq!(snapshot.min_top_depth, dec!(30));
        assert_eq!(snapshot.best_bid, Some(dec!(0.40)));
        assert_eq!(snapshot.best_ask, Some(dec!(0.46)));
    }

    #[test]
    fn book_activity_tracker_handles_single_update_window() {
        let received_at = Utc::now();
        let mut tracker = BookActivityTracker::default();
        let book = BookSnapshot {
            asset_id: "asset-no".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.51),
                size: dec!(25),
            }],
            asks: vec![BookLevel {
                price: dec!(0.53),
                size: dec!(20),
            }],
            received_at,
        };

        let snapshot = tracker
            .record(
                &book,
                received_at + chrono::Duration::milliseconds(1200),
                Duration::from_secs(1),
            )
            .expect("window snapshot");

        assert_eq!(snapshot.asset_id, "asset-no");
        assert_eq!(snapshot.updates, 1);
        assert_eq!(snapshot.avg_gap_ms, None);
        assert_eq!(snapshot.max_gap_ms, None);
        assert_eq!(snapshot.processing_lag_ms, 1200);
        assert_eq!(snapshot.min_top_depth, dec!(20));
        assert_eq!(snapshot.best_bid, Some(dec!(0.51)));
        assert_eq!(snapshot.best_ask, Some(dec!(0.53)));
    }

    #[derive(Default)]
    struct ReconcileErrorExchange {
        reconcile_calls: AtomicUsize,
    }

    #[derive(Default)]
    struct RecordingExchange {
        cancel_all_calls: AtomicUsize,
        flatten_calls: Mutex<Vec<FlattenIntent>>,
        flatten_result: Option<TradeFill>,
        reconcile_snapshot: Mutex<Option<ReconciliationSnapshot>>,
        start_events: Mutex<Vec<ExchangeEvent>>,
        reward_snapshots: Mutex<Vec<TestRewardResponse>>,
    }

    type TestRewardResponse = std::result::Result<Option<RewardSnapshot>, String>;

    impl RecordingExchange {
        fn with_flatten_result(flatten_result: Option<TradeFill>) -> Self {
            Self {
                flatten_result,
                ..Self::default()
            }
        }

        fn with_reconcile_snapshot(reconcile_snapshot: ReconciliationSnapshot) -> Self {
            Self {
                reconcile_snapshot: Mutex::new(Some(reconcile_snapshot)),
                ..Self::default()
            }
        }

        fn with_start_events(start_events: Vec<ExchangeEvent>) -> Self {
            Self {
                start_events: Mutex::new(start_events),
                ..Self::default()
            }
        }

        fn with_reward_snapshots(reward_snapshots: Vec<TestRewardResponse>) -> Self {
            Self {
                reward_snapshots: Mutex::new(reward_snapshots),
                ..Self::default()
            }
        }

        fn flatten_calls(&self) -> Vec<FlattenIntent> {
            self.flatten_calls
                .lock()
                .expect("flatten calls mutex")
                .clone()
        }
    }

    #[async_trait]
    impl ExchangeAdapter for RecordingExchange {
        fn start(
            &self,
            event_tx: mpsc::UnboundedSender<super::ExchangeEvent>,
            cancel: CancellationToken,
        ) {
            let start_events = self
                .start_events
                .lock()
                .expect("start events mutex")
                .clone();
            tokio::spawn(async move {
                for event in start_events {
                    if event_tx.send(event).is_err() {
                        return;
                    }
                }
                cancel.cancelled().await;
            });
        }

        async fn reconcile(&self) -> Result<ReconciliationSnapshot> {
            Ok(self
                .reconcile_snapshot
                .lock()
                .expect("reconcile snapshot mutex")
                .clone()
                .unwrap_or_else(|| ReconciliationSnapshot {
                    books: Vec::new(),
                    open_orders: Vec::new(),
                    positions: Vec::new(),
                    account: AccountSnapshot {
                        usdc_balance: dec!(0),
                        token_balances: HashMap::new(),
                        updated_at: Utc::now(),
                    },
                }))
        }

        async fn post_quotes(&self, _quotes: &[QuoteIntent]) -> Result<Vec<ManagedOrder>> {
            Ok(Vec::new())
        }

        async fn cancel_orders(&self, _order_ids: &[String]) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self) -> Result<()> {
            self.cancel_all_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn flatten(&self, intent: &crate::risk::FlattenIntent) -> Result<Option<TradeFill>> {
            self.flatten_calls
                .lock()
                .expect("flatten calls mutex")
                .push(intent.clone());
            Ok(self.flatten_result.clone())
        }

        async fn post_heartbeat(&self, _last_heartbeat_id: Option<&str>) -> Result<String> {
            Ok("hb-test".to_string())
        }

        async fn split(&self, _amount: rust_decimal::Decimal) -> Result<String> {
            Ok("tx-test".to_string())
        }

        async fn merge(&self, _amount: rust_decimal::Decimal) -> Result<String> {
            Ok("tx-test".to_string())
        }

        async fn fetch_reward_snapshot(&self) -> Result<Option<RewardSnapshot>> {
            self.reward_snapshots
                .lock()
                .expect("reward snapshots mutex")
                .pop()
                .unwrap_or(Ok(None))
                .map_err(|error| anyhow!(error))
        }
    }

    #[async_trait]
    impl ExchangeAdapter for ReconcileErrorExchange {
        fn start(
            &self,
            event_tx: mpsc::UnboundedSender<super::ExchangeEvent>,
            cancel: CancellationToken,
        ) {
            tokio::spawn(async move {
                let _event_tx = event_tx;
                cancel.cancelled().await;
            });
        }

        async fn reconcile(&self) -> Result<ReconciliationSnapshot> {
            self.reconcile_calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("transient reconcile error"))
        }

        async fn post_quotes(&self, _quotes: &[QuoteIntent]) -> Result<Vec<ManagedOrder>> {
            Ok(Vec::new())
        }

        async fn cancel_orders(&self, _order_ids: &[String]) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self) -> Result<()> {
            Ok(())
        }

        async fn flatten(&self, _intent: &crate::risk::FlattenIntent) -> Result<Option<TradeFill>> {
            Ok(None)
        }

        async fn post_heartbeat(&self, _last_heartbeat_id: Option<&str>) -> Result<String> {
            Ok("hb-test".to_string())
        }

        async fn split(&self, _amount: rust_decimal::Decimal) -> Result<String> {
            Ok("tx-test".to_string())
        }

        async fn merge(&self, _amount: rust_decimal::Decimal) -> Result<String> {
            Ok("tx-test".to_string())
        }

        async fn fetch_reward_snapshot(&self) -> Result<Option<RewardSnapshot>> {
            Ok(None)
        }
    }

    fn service_config() -> ServiceConfig {
        ServiceConfig {
            decision: DecisionConfig {
                quote_mode: crate::decision::QuoteMode::Inside,
                quote_size: dec!(10),
                min_spread: dec!(0.01),
                min_depth: dec!(20),
                quote_offset_ticks: 1,
                min_usdc_balance: dec!(50),
                min_token_balance: dec!(10),
                min_inside_ticks: 1,
                min_inside_depth_multiple: dec!(1.5),
                reward_stale_after: chrono::Duration::seconds(30),
            },
            risk: RiskConfig {
                max_position: dec!(100),
                flat_position_tolerance: dec!(1),
                stale_feed_after: chrono::Duration::seconds(15),
                auto_flatten_after_fill: true,
                flatten_use_fok: false,
            },
            startup_split_amount: None,
            heartbeat_interval: Duration::from_secs(3600),
            reconciliation_interval: Duration::from_millis(10),
            report_interval: Duration::from_secs(3600),
            snapshot_interval: Duration::from_secs(3600),
            max_quote_age: Duration::from_secs(10),
            reward_refresh_interval: Duration::from_secs(60),
        }
    }

    fn active_reward_snapshot() -> RewardSnapshot {
        RewardSnapshot {
            condition_id: "condition-1".to_string(),
            max_spread: dec!(0.03),
            min_size: dec!(50),
            total_daily_rate: dec!(4.5),
            active_until: chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
            token_prices: HashMap::from([("asset-yes".to_string(), dec!(0.505))]),
        }
    }

    async fn migrated_pool() -> SqlitePool {
        init_db("sqlite::memory:", 1)
            .await
            .expect("migrated in-memory sqlite pool")
    }

    #[tokio::test]
    async fn trade_event_updates_position_before_risk_flatten() {
        let exchange = Arc::new(RecordingExchange::default());
        let pool = migrated_pool().await;
        let mut state = runtime_state_with_book();
        state.positions.insert(
            "asset-yes".to_string(),
            PositionSnapshot {
                asset_id: "asset-yes".to_string(),
                size: dec!(25),
                avg_price: dec!(0.42),
            },
        );
        let (service, _) = LpService::new(
            exchange.clone(),
            pool,
            service_config(),
            None,
            state.clone(),
        );
        let fill = TradeFill {
            trade_id: "trade-1".to_string(),
            order_id: Some("order-1".to_string()),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Buy,
            price: dec!(0.44),
            size: dec!(12),
            status: "MATCHED".to_string(),
            received_at: Utc::now(),
        };

        service
            .handle_exchange_event(&mut state, ExchangeEvent::Trade(fill))
            .await
            .expect("trade event handled");

        let position = state
            .positions
            .get("asset-yes")
            .expect("updated position present");
        assert_eq!(position.size, dec!(37));
        assert_eq!(position.avg_price, dec!(0.4264864864864864864864864865));
        assert_eq!(exchange.flatten_calls().len(), 1);
        assert_eq!(exchange.flatten_calls()[0].size, dec!(37));
    }

    #[tokio::test]
    async fn reconciliation_refreshes_market_books_and_timestamp() {
        let now = Utc::now();
        let exchange = Arc::new(RecordingExchange::with_reconcile_snapshot(
            ReconciliationSnapshot {
                books: vec![BookSnapshot {
                    asset_id: "asset-yes".to_string(),
                    bids: vec![BookLevel {
                        price: dec!(0.41),
                        size: dec!(120),
                    }],
                    asks: vec![BookLevel {
                        price: dec!(0.47),
                        size: dec!(130),
                    }],
                    received_at: now,
                }],
                open_orders: Vec::new(),
                positions: Vec::new(),
                account: AccountSnapshot {
                    usdc_balance: dec!(250),
                    token_balances: HashMap::new(),
                    updated_at: now,
                },
            },
        ));
        let pool = migrated_pool().await;
        let mut state = runtime_state_with_book();
        let stale_at = now - chrono::Duration::seconds(30);
        state.last_market_event_at = Some(stale_at);
        let (service, _) = LpService::new(exchange, pool, service_config(), None, state.clone());

        service
            .handle_reconciliation_tick(&mut state)
            .await
            .expect("reconciliation handled");

        let refreshed_book = state.books.get("asset-yes").expect("book refreshed");
        assert_eq!(refreshed_book.bids[0].price, dec!(0.41));
        assert_eq!(refreshed_book.asks[0].price, dec!(0.47));
        assert!(state
            .last_market_event_at
            .is_some_and(|timestamp| timestamp > stale_at));
        assert_eq!(state.account.usdc_balance, dec!(250));
    }

    #[tokio::test]
    async fn execute_flatten_clears_flattening_after_acknowledged_fill() {
        let exchange = Arc::new(RecordingExchange::with_flatten_result(Some(TradeFill {
            trade_id: "trade-ack".to_string(),
            order_id: Some("order-ack".to_string()),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Sell,
            price: dec!(0.43),
            size: dec!(25),
            status: "MATCHED".to_string(),
            received_at: Utc::now(),
        })));
        let pool = migrated_pool().await;
        let mut state = runtime_state_with_book();
        let (service, _) = LpService::new(exchange, pool, service_config(), None, state.clone());

        service
            .execute_flatten(
                &mut state,
                "test flatten",
                &FlattenIntent {
                    asset_id: "asset-yes".to_string(),
                    side: QuoteSide::Sell,
                    price: dec!(0.43),
                    size: dec!(25),
                    use_fok: false,
                },
            )
            .await
            .expect("flatten succeeded");

        assert!(!state.flags.flattening);
    }

    #[tokio::test]
    async fn run_cancels_open_orders_immediately_after_shallow_book_event() {
        let now = Utc::now();
        let exchange = Arc::new(RecordingExchange::with_start_events(vec![
            ExchangeEvent::Book(BookSnapshot {
                asset_id: "asset-yes".to_string(),
                bids: vec![BookLevel {
                    price: dec!(0.40),
                    size: dec!(5),
                }],
                asks: vec![BookLevel {
                    price: dec!(0.46),
                    size: dec!(100),
                }],
                received_at: now,
            }),
        ]));
        let pool = migrated_pool().await;
        let mut initial_state = runtime_state_with_book();
        initial_state.open_orders = vec![ManagedOrder {
            order_id: "buy-1".to_string(),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Buy,
            price: dec!(0.41),
            size: dec!(10),
            created_at: now,
            status: "LIVE".to_string(),
        }];
        let mut config = service_config();
        config.heartbeat_interval = Duration::from_secs(3600);
        config.reconciliation_interval = Duration::from_secs(3600);
        config.report_interval = Duration::from_secs(3600);
        config.snapshot_interval = Duration::from_secs(3600);

        let (service, control) =
            LpService::new(exchange.clone(), pool, config, None, initial_state);
        let cancel = CancellationToken::new();
        let service_cancel = cancel.clone();
        let handle = tokio::spawn(async move { service.run(service_cancel).await });

        timeout(Duration::from_secs(1), async {
            while exchange.cancel_all_calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancel-all triggered from book event");

        let snapshot = control.snapshot();
        assert!(snapshot.open_orders.is_empty());
        assert_eq!(exchange.cancel_all_calls.load(Ordering::SeqCst), 1);

        cancel.cancel();
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("service task join")
            .expect("service task panicked");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_cancels_open_orders_on_shutdown() {
        let exchange = Arc::new(RecordingExchange::default());
        let pool = migrated_pool().await;
        let mut initial_state = runtime_state_with_book();
        let now = Utc::now();
        initial_state
            .account
            .token_balances
            .insert("asset-yes".to_string(), dec!(40));
        initial_state.open_orders = vec![
            ManagedOrder {
                order_id: "buy-1".to_string(),
                asset_id: "asset-yes".to_string(),
                side: QuoteSide::Buy,
                price: dec!(0.41),
                size: dec!(10),
                created_at: now,
                status: "LIVE".to_string(),
            },
            ManagedOrder {
                order_id: "sell-1".to_string(),
                asset_id: "asset-yes".to_string(),
                side: QuoteSide::Sell,
                price: dec!(0.45),
                size: dec!(10),
                created_at: now,
                status: "LIVE".to_string(),
            },
        ];
        let mut config = service_config();
        config.heartbeat_interval = Duration::from_secs(3600);
        config.reconciliation_interval = Duration::from_secs(3600);
        config.report_interval = Duration::from_secs(3600);
        config.snapshot_interval = Duration::from_secs(3600);

        let (service, _) = LpService::new(exchange.clone(), pool, config, None, initial_state);
        let cancel = CancellationToken::new();
        let service_cancel = cancel.clone();
        let handle = tokio::spawn(async move { service.run(service_cancel).await });

        cancel.cancel();
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("service task join")
            .expect("service task panicked");

        assert!(result.is_ok());
        assert_eq!(exchange.cancel_all_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn run_keeps_loop_alive_after_transient_reconciliation_error() {
        let exchange = Arc::new(ReconcileErrorExchange::default());
        let pool = migrated_pool().await;
        let mut initial_state = runtime_state_with_book();
        initial_state.flags.paused = true;
        let (service, control) = LpService::new(
            exchange.clone(),
            pool,
            service_config(),
            None,
            initial_state,
        );
        let cancel = CancellationToken::new();
        let service_cancel = cancel.clone();

        let handle = tokio::spawn(async move { service.run(service_cancel).await });

        timeout(Duration::from_secs(1), async {
            while exchange.reconcile_calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reconciliation attempt");
        assert!(exchange.reconcile_calls.load(Ordering::SeqCst) >= 1);
        assert!(control
            .send(ControlCommand::Pause {
                reason: "test pause".to_string(),
            })
            .is_ok());

        cancel.cancel();
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("service task join")
            .expect("service task panicked");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_cancels_open_orders_when_reward_refresh_marks_market_inactive() {
        let exchange = Arc::new(RecordingExchange::with_reward_snapshots(vec![
            Ok(None),
            Ok(Some(active_reward_snapshot())),
        ]));
        let pool = migrated_pool().await;
        let mut initial_state = runtime_state_with_book();
        let now = Utc::now();
        initial_state.books.get_mut("asset-yes").unwrap().bids = vec![BookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }];
        initial_state.books.get_mut("asset-yes").unwrap().asks = vec![BookLevel {
            price: dec!(0.51),
            size: dec!(100),
        }];
        initial_state
            .account
            .token_balances
            .insert("asset-yes".to_string(), dec!(40));
        initial_state.reward.snapshot = Some(active_reward_snapshot());
        initial_state.reward.last_attempt_at = Some(now);
        initial_state.reward.last_success_at = Some(now);
        initial_state.open_orders = vec![ManagedOrder {
            order_id: "sell-1".to_string(),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Sell,
            price: dec!(0.53),
            size: dec!(10),
            created_at: now,
            status: "LIVE".to_string(),
        }];

        let mut config = service_config();
        config.heartbeat_interval = Duration::from_secs(3600);
        config.reconciliation_interval = Duration::from_secs(3600);
        config.report_interval = Duration::from_secs(3600);
        config.snapshot_interval = Duration::from_secs(3600);
        config.reward_refresh_interval = Duration::from_millis(10);

        let (service, control) =
            LpService::new(exchange.clone(), pool, config, None, initial_state);
        let cancel = CancellationToken::new();
        let service_cancel = cancel.clone();
        let handle = tokio::spawn(async move { service.run(service_cancel).await });

        timeout(Duration::from_secs(1), async {
            while exchange.cancel_all_calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reward refresh triggers cancel-all");

        let snapshot = control.snapshot();
        assert!(snapshot.open_orders.is_empty());
        assert!(snapshot.reward.snapshot.is_none());

        cancel.cancel();
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("service task join")
            .expect("service task panicked");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_cancels_open_orders_when_reward_refresh_errors() {
        let exchange = Arc::new(RecordingExchange::with_reward_snapshots(vec![
            Err("temporary reward failure".to_string()),
            Ok(Some(active_reward_snapshot())),
        ]));
        let pool = migrated_pool().await;
        let mut initial_state = runtime_state_with_book();
        let now = Utc::now();
        initial_state.books.get_mut("asset-yes").unwrap().bids = vec![BookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }];
        initial_state.books.get_mut("asset-yes").unwrap().asks = vec![BookLevel {
            price: dec!(0.51),
            size: dec!(100),
        }];
        initial_state
            .account
            .token_balances
            .insert("asset-yes".to_string(), dec!(40));
        initial_state.reward.snapshot = Some(active_reward_snapshot());
        initial_state.reward.last_attempt_at = Some(now);
        initial_state.reward.last_success_at = Some(now);
        initial_state.open_orders = vec![ManagedOrder {
            order_id: "sell-1".to_string(),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Sell,
            price: dec!(0.53),
            size: dec!(10),
            created_at: now,
            status: "LIVE".to_string(),
        }];

        let mut config = service_config();
        config.heartbeat_interval = Duration::from_secs(3600);
        config.reconciliation_interval = Duration::from_secs(3600);
        config.report_interval = Duration::from_secs(3600);
        config.snapshot_interval = Duration::from_secs(3600);
        config.reward_refresh_interval = Duration::from_millis(10);

        let (service, control) =
            LpService::new(exchange.clone(), pool, config, None, initial_state);
        let cancel = CancellationToken::new();
        let service_cancel = cancel.clone();
        let handle = tokio::spawn(async move { service.run(service_cancel).await });

        timeout(Duration::from_secs(1), async {
            while exchange.cancel_all_calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reward refresh error triggers cancel-all");

        let snapshot = control.snapshot();
        assert!(snapshot.open_orders.is_empty());
        assert!(snapshot.reward.snapshot.is_none());
        assert_eq!(
            snapshot.reward.last_error.as_deref(),
            Some("temporary reward failure")
        );

        cancel.cancel();
        let result = timeout(Duration::from_secs(1), handle)
            .await
            .expect("service task join")
            .expect("service task panicked");
        assert!(result.is_ok());
    }

    #[test]
    fn desired_quotes_match_rejects_one_tick_price_difference() {
        let mut state = runtime_state_with_book();
        let now = Utc::now();
        state.open_orders = vec![ManagedOrder {
            order_id: "order-1".to_string(),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Buy,
            price: dec!(0.41),
            size: dec!(10),
            created_at: now,
            status: "LIVE".to_string(),
        }];
        let outcome = DecisionOutcome {
            desired_quotes: vec![QuoteIntent {
                asset_id: "asset-yes".to_string(),
                side: QuoteSide::Buy,
                price: dec!(0.42),
                size: dec!(10),
                reason: "test".to_string(),
            }],
            cancel_all: false,
            reason: "test".to_string(),
            diagnostics: placeholder_decision_diagnostics(),
        };

        assert!(!desired_quotes_match(
            &state,
            &outcome,
            Duration::from_secs(10),
        ));
    }

    #[test]
    fn desired_quotes_match_rejects_prices_beyond_one_tick() {
        let mut state = runtime_state_with_book();
        let now = Utc::now();
        state.open_orders = vec![ManagedOrder {
            order_id: "order-1".to_string(),
            asset_id: "asset-yes".to_string(),
            side: QuoteSide::Buy,
            price: dec!(0.41),
            size: dec!(10),
            created_at: now,
            status: "LIVE".to_string(),
        }];
        let outcome = DecisionOutcome {
            desired_quotes: vec![QuoteIntent {
                asset_id: "asset-yes".to_string(),
                side: QuoteSide::Buy,
                price: dec!(0.43),
                size: dec!(10),
                reason: "test".to_string(),
            }],
            cancel_all: false,
            reason: "test".to_string(),
            diagnostics: placeholder_decision_diagnostics(),
        };

        assert!(!desired_quotes_match(
            &state,
            &outcome,
            Duration::from_secs(10),
        ));
    }
}
