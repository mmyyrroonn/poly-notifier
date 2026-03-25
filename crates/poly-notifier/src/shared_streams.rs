use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use pn_lp::{ExchangeEvent, ManagedOrder, QuoteIntent, QuoteSide};
use rust_decimal::Decimal;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
pub struct SharedEventFanout {
    subscribers: Arc<DashMap<String, Vec<mpsc::UnboundedSender<ExchangeEvent>>>>,
}

impl SharedEventFanout {
    pub fn register(&self, asset_ids: Vec<String>, tx: mpsc::UnboundedSender<ExchangeEvent>) {
        for asset_id in asset_ids {
            self.subscribers
                .entry(asset_id)
                .or_default()
                .push(tx.clone());
        }
    }

    pub fn dispatch(&self, event: ExchangeEvent) {
        let asset_id = event_asset_id(&event).to_string();
        if let Some(mut entry) = self.subscribers.get_mut(&asset_id) {
            entry.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
    }
}

fn event_asset_id(event: &ExchangeEvent) -> &str {
    match event {
        ExchangeEvent::Book(book) => &book.asset_id,
        ExchangeEvent::Order(order) => &order.asset_id,
        ExchangeEvent::Trade(fill) => &fill.asset_id,
        ExchangeEvent::TickSize { asset_id, .. } => asset_id.as_str(),
    }
}

#[derive(Clone, Default)]
pub struct BuyReservationLedger {
    inner: Arc<Mutex<BuyReservationState>>,
}

#[derive(Default)]
struct BuyReservationState {
    account_usdc_balance: Decimal,
    reserved_by_market: HashMap<String, Decimal>,
    caps_by_market: HashMap<String, Decimal>,
}

impl BuyReservationLedger {
    pub fn configure_market(&self, condition_id: &str, buy_budget_usdc: Option<Decimal>) {
        let mut state = self.inner.lock().expect("buy reservation lock");
        match buy_budget_usdc {
            Some(cap) => {
                state.caps_by_market.insert(condition_id.to_string(), cap);
            }
            None => {
                state.caps_by_market.remove(condition_id);
            }
        }
    }

    pub fn refresh_market(
        &self,
        condition_id: &str,
        account_usdc_balance: Decimal,
        open_orders: &[ManagedOrder],
    ) {
        let reserved = open_orders
            .iter()
            .filter(|order| matches!(order.side, QuoteSide::Buy))
            .fold(Decimal::ZERO, |sum, order| sum + (order.price * order.size));
        let mut state = self.inner.lock().expect("buy reservation lock");
        state.account_usdc_balance = account_usdc_balance;
        state
            .reserved_by_market
            .insert(condition_id.to_string(), reserved);
    }

    pub fn reserve_quotes(
        &self,
        condition_id: &str,
        quotes: &[QuoteIntent],
    ) -> Result<Decimal, String> {
        let requested = quotes
            .iter()
            .filter(|quote| matches!(quote.side, QuoteSide::Buy))
            .fold(Decimal::ZERO, |sum, quote| sum + (quote.price * quote.size));
        let mut state = self.inner.lock().expect("buy reservation lock");
        let previous_reserved = state
            .reserved_by_market
            .get(condition_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        let reserved_elsewhere = state
            .reserved_by_market
            .iter()
            .filter(|(market, _)| market.as_str() != condition_id)
            .fold(Decimal::ZERO, |sum, (_, reserved)| sum + *reserved);
        let available_global = (state.account_usdc_balance - reserved_elsewhere).max(Decimal::ZERO);
        if requested > available_global {
            return Err(format!(
                "insufficient shared buy capacity: requested {requested}, available {available_global}"
            ));
        }

        if let Some(cap) = state.caps_by_market.get(condition_id) {
            if requested > *cap {
                return Err(format!(
                    "market buy budget exceeded: requested {requested}, cap {cap}"
                ));
            }
        }

        state
            .reserved_by_market
            .insert(condition_id.to_string(), requested);

        Ok(previous_reserved)
    }

    pub fn clear_market(&self, condition_id: &str) {
        let mut state = self.inner.lock().expect("buy reservation lock");
        state
            .reserved_by_market
            .insert(condition_id.to_string(), Decimal::ZERO);
    }

    pub fn restore_market(&self, condition_id: &str, reserved: Decimal) {
        let mut state = self.inner.lock().expect("buy reservation lock");
        state
            .reserved_by_market
            .insert(condition_id.to_string(), reserved);
    }
}
