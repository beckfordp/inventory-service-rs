// SPDX-License-Identifier: MIT

//! The pure core, as the Scala `StockLevels`, `ReserveOrder`, `Reconciliation`
//! and `OperatorAuth`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use inventory_service::domain::{
    FailureReason, InventoryId, LineStatus, NewStock, OperatorAuth, ReconciliationStatus,
    RequestedLine, ReservationLine, ReserveOrder, demand, failures, parse_uuid, stock_update,
};
use uuid::Uuid;

const A: &str = "00000000-0000-4000-8000-000000000001";
const B: &str = "00000000-0000-4000-8000-000000000002";

fn requested(item: &str, sku: &str, quantity: i32) -> RequestedLine {
    RequestedLine {
        order_item_id: item.to_owned(),
        sku: sku.to_owned(),
        quantity,
    }
}

fn line(n: u128, sku: &str, quantity: i32) -> ReservationLine {
    ReservationLine {
        order_item_id: Uuid::from_u128(n),
        sku: sku.to_owned(),
        quantity,
    }
}

fn reason(result: Result<ReserveOrder, inventory_service::domain::InvalidReservation>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn a_uuid_is_javas_canonical_form() {
    assert_eq!(
        parse_uuid(A),
        Some(Uuid::from_u128(0x4000_8000_0000_0000_0001))
    );
    assert!(parse_uuid(&A.to_uppercase()).is_some());
    for bad in [
        "nope",
        "",
        "00000000000040008000000000000001",
        &format!("{{{A}}}"),
        "00000000-0000-4000-8000-00000000000g",
    ] {
        assert_eq!(parse_uuid(bad), None, "{bad}");
    }
    let id: InventoryId = A.parse().unwrap();
    assert_eq!(id.to_string(), A);
    assert_eq!(id.uuid(), parse_uuid(A).unwrap());
    assert_eq!(
        "x".parse::<InventoryId>().unwrap_err().to_string(),
        "not a UUID"
    );
}

#[test]
fn stock_needs_a_sku_and_no_negative_quantity() {
    let ok = NewStock::parse("SKU-1", 0).unwrap();
    assert_eq!((ok.sku.as_str(), ok.quantity_available), ("SKU-1", 0));
    assert_eq!(
        NewStock::parse(" \t", -1).unwrap_err().0,
        "sku must not be blank"
    );
    assert_eq!(
        NewStock::parse("S", -1).unwrap_err().to_string(),
        "quantityAvailable must not be negative"
    );
    assert_eq!(stock_update(3), Ok(3));
    assert_eq!(
        stock_update(-1).unwrap_err().0,
        "quantityAvailable must not be negative"
    );
}

#[test]
fn a_reservation_is_judged_in_the_scala_order() {
    let ok = ReserveOrder::validate(A, &[requested(B, "", 2)]).unwrap();
    assert_eq!(ok.order_id, parse_uuid(A).unwrap());
    assert_eq!(
        ok.items,
        vec![ReservationLine {
            order_item_id: parse_uuid(B).unwrap(),
            sku: String::new(),
            quantity: 2
        }]
    );
    assert_eq!(
        reason(ReserveOrder::validate("x", &[])),
        "orderId must be a UUID"
    );
    assert_eq!(
        reason(ReserveOrder::validate(A, &[])),
        "items must not be empty"
    );
    assert_eq!(
        reason(ReserveOrder::validate(
            A,
            &[requested(B, "s", 0), requested("x", "s", 1)]
        )),
        "quantity must be positive",
        "the first bad line's reason"
    );
    assert_eq!(
        reason(ReserveOrder::validate(A, &[requested("x", "s", 0)])),
        "orderItemId must be a UUID",
        "a line's id before its quantity"
    );
    assert_eq!(
        reason(ReserveOrder::validate(
            A,
            &[requested(B, "s", 1), requested(B, "t", 1)]
        )),
        "duplicate orderItemId"
    );
}

#[test]
fn failures_sum_demand_per_sku_in_request_order() {
    let lines = [
        line(1, "b", 1),
        line(2, "a", 1),
        line(3, "nope", 1),
        line(4, "a", 2),
    ];
    let available = HashMap::from([("a".to_owned(), 2), ("b".to_owned(), 5)]);
    let found: Vec<_> = failures(&lines, &available)
        .into_iter()
        .map(|f| (f.order_item_id, f.sku, f.reason.wire()))
        .collect();
    assert_eq!(
        found,
        vec![
            (Uuid::from_u128(2), "a".to_owned(), "insufficient_stock"),
            (Uuid::from_u128(3), "nope".to_owned(), "unknown_sku"),
            (Uuid::from_u128(4), "a".to_owned(), "insufficient_stock"),
        ]
    );
    assert_eq!(
        failures(&lines[..1], &available),
        Vec::<inventory_service::domain::ReservationFailure>::new()
    );
    let huge = [line(1, "a", i32::MAX), line(2, "a", i32::MAX)];
    let plenty = HashMap::from([("a".to_owned(), i32::MAX)]);
    assert_eq!(failures(&huge, &plenty).len(), 2, "summed without overflow");
    assert_eq!(FailureReason::UnknownSku.wire(), "unknown_sku");
    let by_sku: Vec<_> = demand(&lines).into_iter().collect();
    assert_eq!(
        by_sku,
        vec![
            ("a".to_owned(), 3),
            ("b".to_owned(), 1),
            ("nope".to_owned(), 1)
        ]
    );
}

#[test]
fn statuses_parse_as_stored() {
    for status in [
        LineStatus::Active,
        LineStatus::Released,
        LineStatus::Shipped,
    ] {
        assert_eq!(LineStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(LineStatus::parse("gone"), None);
    for status in [ReconciliationStatus::Open, ReconciliationStatus::Resolved] {
        assert_eq!(ReconciliationStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(ReconciliationStatus::parse("closed"), None);
}

#[test]
fn the_operator_token_is_a_bearer_token() {
    let auth = OperatorAuth::new(" secret ", " ops ");
    assert_eq!(auth.operator(Some("Bearer secret")), Some("ops"));
    assert_eq!(auth.operator(Some("bEaReR   secret  ")), Some("ops"));
    for bad in [
        None,
        Some("Bearer"),
        Some("Bearer "),
        Some("Bearer wrong"),
        Some("Bearer secre"),
        Some("Basic secret"),
        Some("Bearer\u{e9}secret"),
    ] {
        assert_eq!(auth.operator(bad), None, "{bad:?}");
    }
    assert_eq!(
        OperatorAuth::new("  ", "ops").operator(Some("Bearer  ")),
        None,
        "no token, none accepted"
    );
}
