use chrono::Utc;
use pn_lp::types::{BookLevel, BookSnapshot, ManagedOrder, QuoteIntent, QuoteSide};
use pn_lp::ExchangeEvent;
use poly_lp::shared_streams::{BuyReservationLedger, SharedEventFanout};
use rust_decimal_macros::dec;
use tokio::sync::mpsc;

#[tokio::test]
async fn shared_event_fanout_routes_only_matching_assets() {
    let fanout = SharedEventFanout::default();
    let (tx_a, mut rx_a) = mpsc::unbounded_channel();
    let (tx_b, mut rx_b) = mpsc::unbounded_channel();

    fanout.register(vec!["asset-a".to_string()], tx_a);
    fanout.register(vec!["asset-b".to_string()], tx_b);

    fanout.dispatch(book_event("asset-a"));

    match rx_a.try_recv().expect("asset-a event") {
        ExchangeEvent::Book(book) => assert_eq!(book.asset_id, "asset-a"),
        other => panic!("expected book event, got {other:?}"),
    }
    assert!(rx_b.try_recv().is_err());
}

fn book_event(asset_id: &str) -> ExchangeEvent {
    ExchangeEvent::Book(BookSnapshot {
        asset_id: asset_id.to_string(),
        bids: vec![BookLevel {
            price: "0.45".parse().unwrap(),
            size: "10".parse().unwrap(),
        }],
        asks: vec![BookLevel {
            price: "0.55".parse().unwrap(),
            size: "10".parse().unwrap(),
        }],
        received_at: Utc::now(),
    })
}

#[test]
fn buy_reservation_ledger_blocks_cross_market_overcommit() {
    let ledger = BuyReservationLedger::default();
    ledger.configure_market("condition-a", Some(dec!(10)));
    ledger.configure_market("condition-b", Some(dec!(10)));
    ledger.refresh_market(
        "condition-a",
        dec!(15),
        &[buy_order("asset-a", dec!(0.4), dec!(10))],
    );
    ledger.refresh_market("condition-b", dec!(15), &[]);

    let result = ledger.reserve_quotes("condition-b", &[buy_quote("asset-b", dec!(0.6), dec!(20))]);
    assert!(result.is_err());

    ledger
        .reserve_quotes("condition-b", &[buy_quote("asset-b", dec!(0.5), dec!(10))])
        .expect("reserve within remaining capacity");
}

fn buy_order(
    asset_id: &str,
    price: rust_decimal::Decimal,
    size: rust_decimal::Decimal,
) -> ManagedOrder {
    ManagedOrder {
        order_id: "order-1".to_string(),
        asset_id: asset_id.to_string(),
        side: QuoteSide::Buy,
        price,
        size,
        created_at: Utc::now(),
        status: "LIVE".to_string(),
    }
}

fn buy_quote(
    asset_id: &str,
    price: rust_decimal::Decimal,
    size: rust_decimal::Decimal,
) -> QuoteIntent {
    QuoteIntent {
        asset_id: asset_id.to_string(),
        side: QuoteSide::Buy,
        price,
        size,
        reason: "test".to_string(),
    }
}
