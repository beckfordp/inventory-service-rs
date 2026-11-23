// SPDX-License-Identifier: MIT

//! The consumers' routes, less the stream, over the store on a throwaway
//! Postgres: which envelopes they act on, what poison is, and the stock and
//! flags they leave. The rest of the per-record decision is gluon-rs
//! `kafka-event-consumer`'s; the consumers against a broker are tests/contract's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use inventory_service::consumers::{LAG_INTERVAL, settings};
use inventory_service::domain::{
    CANCEL_AFTER_DISPATCH, DISPATCH_AFTER_RELEASE, NewStock, RequestedLine, ReserveOrder,
};
use inventory_service::events::{
    EventsError, FULFILMENT_EVENTS, FULFILMENT_GROUP, FulfilmentEvents, ORDER_EVENTS, ORDER_GROUP,
    OrderEvents,
};
use inventory_service::store::{Inventories, PgInventories};
use kafka_event_consumer::envelope::{Envelope, Routes};
use serde_json::{Value, json};
use support::{Broken, Postgres, captured_logger, log_lines};
use uuid::Uuid;

const ORDER: &str = "00000000-0000-4000-8000-000000000001";

fn envelope(event_type: &str, payload: &Value) -> Envelope {
    serde_json::from_value(json!({
        "eventId": "e1",
        "eventType": event_type,
        "timestamp": "2026-11-20T10:00:00Z",
        "source": "test",
        "correlationId": "c1",
        "payload": payload,
    }))
    .unwrap()
}

fn cancelled(order: &str) -> Envelope {
    envelope(
        "OrderCancelled",
        &json!({"orderId": order, "customerId": "c", "timestamp": "2026-11-20T10:00:00Z"}),
    )
}

fn dispatched(order: &str) -> Envelope {
    envelope(
        "DispatchConfirmed",
        &json!({"orderId": order, "trackingNo": "TRK-1"}),
    )
}

async fn reserved(store: &PgInventories, quantity: i32) {
    if store.list(Some("A")).await.unwrap().is_empty() {
        store
            .create(
                Uuid::from_u128(1).into(),
                &NewStock::parse("A", 10).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
    }
    let line = RequestedLine {
        order_item_id: Uuid::new_v4().to_string(),
        sku: "A".to_owned(),
        quantity,
    };
    let order = ReserveOrder::validate(ORDER, &[line]).unwrap();
    store.reserve(&order).await.unwrap().unwrap();
}

async fn levels(store: &PgInventories) -> (i32, i32) {
    let [found] = store.list(Some("A")).await.unwrap().try_into().unwrap();
    (found.quantity_available, found.quantity_reserved)
}

async fn reasons(store: &PgInventories) -> Vec<String> {
    let flags = store.reconciliations(None, None).await.unwrap();
    flags.into_iter().map(|f| f.reason).collect()
}

#[tokio::test]
async fn order_cancelled_releases_and_after_dispatch_flags() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    let (logger, buffer) = captured_logger();
    let route = OrderEvents::new(store.clone(), logger);
    reserved(&store, 3).await;
    let event = route.decode(&cancelled(ORDER)).unwrap().unwrap();
    route.handle("e1", &event).await.unwrap();
    assert_eq!(levels(&store).await, (10, 0));
    assert_eq!(reasons(&store).await, Vec::<std::string::String>::new());

    let fulfilment = FulfilmentEvents::new(store.clone(), captured_logger().0);
    // Another order: this one, once released, can't reserve again.
    let other = "00000000-0000-4000-8000-000000000002";
    let line = RequestedLine {
        order_item_id: Uuid::new_v4().to_string(),
        sku: "A".to_owned(),
        quantity: 1,
    };
    store
        .reserve(&ReserveOrder::validate(other, &[line]).unwrap())
        .await
        .unwrap()
        .unwrap();
    let shipped = fulfilment.decode(&dispatched(other)).unwrap().unwrap();
    fulfilment.handle("e2", &shipped).await.unwrap();
    let late = route.decode(&cancelled(other)).unwrap().unwrap();
    route.handle("e3", &late).await.unwrap();
    assert_eq!(reasons(&store).await, [CANCEL_AFTER_DISPATCH]);
    assert_eq!(levels(&store).await, (9, 0));
    let messages: Vec<_> = log_lines(&buffer)
        .into_iter()
        .map(|l| l["message"].clone())
        .collect();
    assert_eq!(
        messages,
        [
            json!("Reservation released on order cancellation"),
            json!("OrderCancelled after dispatch - reconciliation flagged"),
        ]
    );
}

#[tokio::test]
async fn dispatch_confirmed_ships_and_after_release_flags() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    let (logger, buffer) = captured_logger();
    let route = FulfilmentEvents::new(store.clone(), logger);
    let unknown = route.decode(&dispatched(ORDER)).unwrap().unwrap();
    route.handle("e0", &unknown).await.unwrap();
    reserved(&store, 2).await;
    let event = route.decode(&dispatched(ORDER)).unwrap().unwrap();
    route.handle("e1", &event).await.unwrap();
    route.handle("e2", &event).await.unwrap();
    assert_eq!(levels(&store).await, (8, 0));
    assert_eq!(reasons(&store).await, Vec::<std::string::String>::new());

    let other = "00000000-0000-4000-8000-000000000002";
    let line = RequestedLine {
        order_item_id: Uuid::new_v4().to_string(),
        sku: "A".to_owned(),
        quantity: 1,
    };
    store
        .reserve(&ReserveOrder::validate(other, &[line]).unwrap())
        .await
        .unwrap()
        .unwrap();
    store
        .release(Uuid::parse_str(other).unwrap())
        .await
        .unwrap();
    let late = route.decode(&dispatched(other)).unwrap().unwrap();
    route.handle("e3", &late).await.unwrap();
    assert_eq!(reasons(&store).await, [DISPATCH_AFTER_RELEASE]);
    assert_eq!(levels(&store).await, (8, 0));
    let messages: Vec<_> = log_lines(&buffer)
        .into_iter()
        .map(|l| l["message"].clone())
        .collect();
    assert_eq!(
        messages,
        [
            json!("DispatchConfirmed for an unknown reservation - skipping"),
            json!("Reservation marked shipped"),
            json!("DispatchConfirmed for an already-shipped reservation - skipping duplicate"),
            json!("DispatchConfirmed after release - reconciliation flagged"),
        ]
    );
}

#[test]
fn other_types_are_skipped_and_bad_payloads_are_poison() {
    let orders = OrderEvents::new(Broken, captured_logger().0);
    let fulfilment = FulfilmentEvents::new(Broken, captured_logger().0);
    assert!(orders.decode(&dispatched(ORDER)).is_none());
    assert!(fulfilment.decode(&cancelled(ORDER)).is_none());
    let missing = orders.decode(&envelope("OrderCancelled", &json!({"customerId": "c"})));
    assert!(
        matches!(missing, Some(Err(EventsError::Payload(_)))),
        "{missing:?}"
    );
    let bad_id = fulfilment.decode(&dispatched("nope")).unwrap().unwrap_err();
    assert_eq!(
        bad_id.to_string(),
        "Invalid orderId in DispatchConfirmed payload: nope"
    );
    assert!(matches!(
        orders.decode(&cancelled("nope")),
        Some(Err(EventsError::OrderId(..)))
    ));
}

#[tokio::test]
async fn a_store_failure_is_the_handlers_error() {
    let orders = OrderEvents::new(Broken, captured_logger().0);
    let event = orders.decode(&cancelled(ORDER)).unwrap().unwrap();
    assert!(matches!(
        orders.handle("e1", &event).await,
        Err(EventsError::Store(_))
    ));
    let fulfilment = FulfilmentEvents::new(Broken, captured_logger().0);
    let event = fulfilment.decode(&dispatched(ORDER)).unwrap().unwrap();
    assert!(matches!(
        fulfilment.handle("e1", &event).await,
        Err(EventsError::Store(_))
    ));
}

#[test]
fn each_consumer_reads_its_topic_as_its_group() {
    for (topic, group) in [
        (ORDER_EVENTS, ORDER_GROUP),
        (FULFILMENT_EVENTS, FULFILMENT_GROUP),
    ] {
        let settings = settings("kafka:9092", topic, group).unwrap();
        assert_eq!(settings.bootstrap, "kafka:9092");
        assert_eq!(settings.group_id.as_str(), group);
        assert_eq!(settings.topic.as_str(), topic);
        assert_eq!(settings.lag_interval, LAG_INTERVAL);
    }
}
