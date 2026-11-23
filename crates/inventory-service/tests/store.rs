// SPDX-License-Identifier: MIT

//! The tables in a throwaway Postgres, migrated as the service migrates them:
//! stock, an order's reserve/release/ship under its lock, flags and their
//! resolution, the ledger.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use inventory_service::domain::{
    CANCEL_AFTER_DISPATCH, DISPATCH_AFTER_RELEASE, InventoryId, LineStatus, NewStock,
    ReconciliationStatus, RequestedLine, ReservationConflict, ReserveOrder, ResolveOutcome,
};
use inventory_service::store::{DuplicateSku, Inventories, PgInventories};
use support::Postgres;
use time::macros::datetime;
use uuid::Uuid;

fn id(n: u128) -> InventoryId {
    Uuid::from_u128(n).into()
}

fn order(n: u128, lines: &[(u128, &str, i32)]) -> ReserveOrder {
    let lines: Vec<RequestedLine> = lines
        .iter()
        .map(|&(item, sku, quantity)| RequestedLine {
            order_item_id: Uuid::from_u128(item).to_string(),
            sku: sku.to_owned(),
            quantity,
        })
        .collect();
    ReserveOrder::validate(&Uuid::from_u128(n).to_string(), &lines).unwrap()
}

async fn stocked(store: &PgInventories, n: u128, sku: &str, quantity: i32) {
    let new = NewStock::parse(sku, quantity).unwrap();
    store.create(id(n), &new).await.unwrap().unwrap();
}

async fn levels(store: &PgInventories, sku: &str) -> (i32, i32) {
    let [found] = store.list(Some(sku)).await.unwrap().try_into().unwrap();
    (found.quantity_available, found.quantity_reserved)
}

#[tokio::test]
async fn stock_is_created_read_updated_listed_and_deleted() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    let created = store
        .create(id(1), &NewStock::parse("b", 5).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            created.sku.as_str(),
            created.quantity_available,
            created.quantity_reserved
        ),
        ("b", 5, 0)
    );
    assert_eq!(created.created_at, created.updated_at);
    let again = store
        .create(id(2), &NewStock::parse("b", 1).unwrap())
        .await
        .unwrap();
    assert_eq!(again, Err(DuplicateSku));
    stocked(&store, 3, "a", 1).await;
    assert_eq!(store.get(id(1)).await.unwrap(), Some(created.clone()));
    assert_eq!(store.get(id(9)).await.unwrap(), None);
    let updated = store.update(id(1), 7).await.unwrap().unwrap();
    assert_eq!(updated.quantity_available, 7);
    assert!(updated.updated_at > created.updated_at);
    assert_eq!(store.update(id(9), 7).await.unwrap(), None);
    let skus: Vec<_> = store
        .list(None)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.sku)
        .collect();
    assert_eq!(skus, ["a", "b"]);
    assert_eq!(
        store.list(Some("c")).await.unwrap(),
        Vec::<inventory_service::domain::Inventory>::new()
    );
    assert!(store.delete(id(1)).await.unwrap());
    assert!(!store.delete(id(1)).await.unwrap());
}

#[tokio::test]
async fn a_reservation_is_all_or_nothing_and_idempotent() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    stocked(&store, 1, "a", 10).await;
    stocked(&store, 2, "b", 3).await;
    let first = order(1, &[(11, "b", 1), (12, "a", 4), (13, "a", 2)]);
    assert_eq!(
        store.reserve(&first).await.unwrap(),
        Ok(first.items.clone())
    );
    assert_eq!(
        (levels(&store, "a").await, levels(&store, "b").await),
        ((4, 6), (2, 1))
    );
    let repeat = order(1, &[(19, "a", 1)]);
    assert_eq!(
        store.reserve(&repeat).await.unwrap(),
        Ok(first.items.clone())
    );
    assert_eq!(levels(&store, "a").await, (4, 6), "nothing more reserved");
    let short = order(2, &[(21, "a", 5), (22, "nope", 1)]);
    let Err(ReservationConflict::Rejected(failed)) = store.reserve(&short).await.unwrap() else {
        panic!("expected a rejection");
    };
    assert_eq!(failed.len(), 2);
    assert_eq!(levels(&store, "a").await, (4, 6), "nothing reserved");
    assert_eq!(
        store.ledger(Some(Uuid::from_u128(2)), None).await.unwrap(),
        Vec::<inventory_service::domain::LedgerEntry>::new()
    );
    let rows = store.ledger(Some(Uuid::from_u128(1)), None).await.unwrap();
    let seen: Vec<_> = rows
        .iter()
        .map(|r| (r.line_no, r.sku.as_str(), r.status, r.released_at))
        .collect();
    assert_eq!(
        seen,
        [
            (0, "b", LineStatus::Active, None),
            (1, "a", LineStatus::Active, None),
            (2, "a", LineStatus::Active, None)
        ]
    );
}

#[tokio::test]
async fn release_returns_active_lines_and_a_released_order_cant_reserve() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    stocked(&store, 1, "a", 10).await;
    let reserved = order(1, &[(11, "a", 3), (12, "a", 1)]);
    store.reserve(&reserved).await.unwrap().unwrap();
    let outcome = store.release(Uuid::from_u128(1)).await.unwrap();
    assert_eq!(
        (outcome.released, outcome.shipped),
        (reserved.items.clone(), vec![])
    );
    assert_eq!(levels(&store, "a").await, (10, 0));
    let again = store.release(Uuid::from_u128(1)).await.unwrap();
    assert_eq!(
        again.released,
        Vec::<inventory_service::domain::ReservationLine>::new()
    );
    assert_eq!(levels(&store, "a").await, (10, 0));
    assert_eq!(
        store.reserve(&reserved).await.unwrap(),
        Err(ReservationConflict::Released)
    );
    let rows = store
        .ledger(None, Some(LineStatus::Released))
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.released_at.is_some()));
    assert_eq!(
        store.ledger(None, Some(LineStatus::Active)).await.unwrap(),
        Vec::<inventory_service::domain::LedgerEntry>::new()
    );
}

#[tokio::test]
async fn shipping_takes_reserved_stock_and_races_raise_one_flag() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    stocked(&store, 1, "a", 10).await;
    store
        .reserve(&order(1, &[(11, "a", 3)]))
        .await
        .unwrap()
        .unwrap();
    let shipped = store.ship(Uuid::from_u128(1)).await.unwrap();
    assert_eq!(shipped.shipped.len(), 1);
    assert_eq!(levels(&store, "a").await, (7, 0));
    let duplicate = store.ship(Uuid::from_u128(1)).await.unwrap();
    assert_eq!(
        (duplicate.shipped.len(), duplicate.already_shipped.len()),
        (0, 1)
    );
    let cancel = store.release(Uuid::from_u128(1)).await.unwrap();
    assert_eq!((cancel.released.len(), cancel.shipped.len()), (0, 1));
    assert!(
        store
            .flag(Uuid::from_u128(1), CANCEL_AFTER_DISPATCH)
            .await
            .unwrap()
    );
    assert!(
        !store
            .flag(Uuid::from_u128(1), DISPATCH_AFTER_RELEASE)
            .await
            .unwrap(),
        "one flag per order"
    );
    assert!(
        !store
            .flag(Uuid::from_u128(9), DISPATCH_AFTER_RELEASE)
            .await
            .unwrap(),
        "never reserved"
    );

    store
        .reserve(&order(2, &[(21, "a", 1)]))
        .await
        .unwrap()
        .unwrap();
    store.release(Uuid::from_u128(2)).await.unwrap();
    let late = store.ship(Uuid::from_u128(2)).await.unwrap();
    assert_eq!(late.released.len(), 1);
    assert!(
        store
            .flag(Uuid::from_u128(2), DISPATCH_AFTER_RELEASE)
            .await
            .unwrap()
    );
    assert_eq!(levels(&store, "a").await, (7, 0));

    let all = store.reconciliations(None, None).await.unwrap();
    let orders: Vec<_> = all
        .iter()
        .map(|f| (f.order_id, f.reason.as_str(), f.status))
        .collect();
    assert_eq!(
        orders,
        [
            (
                Uuid::from_u128(2),
                DISPATCH_AFTER_RELEASE,
                ReconciliationStatus::Open
            ),
            (
                Uuid::from_u128(1),
                CANCEL_AFTER_DISPATCH,
                ReconciliationStatus::Open
            ),
        ],
        "newest first"
    );
    let one = store
        .reconciliations(Some(ReconciliationStatus::Open), Some(Uuid::from_u128(1)))
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(
        store
            .reconciliations(Some(ReconciliationStatus::Resolved), None)
            .await
            .unwrap(),
        Vec::<inventory_service::domain::Reconciliation>::new()
    );
    assert_eq!(
        store
            .ledger(None, Some(LineStatus::Shipped))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn the_first_resolver_wins() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    stocked(&store, 1, "a", 10).await;
    store
        .reserve(&order(1, &[(11, "a", 1)]))
        .await
        .unwrap()
        .unwrap();
    store
        .flag(Uuid::from_u128(1), CANCEL_AFTER_DISPATCH)
        .await
        .unwrap();
    let at = datetime!(2026-11-22 10:00:00.123456 UTC);
    let ResolveOutcome::Resolved(first) = store
        .resolve(Uuid::from_u128(1), "ops", Some("checked"), at)
        .await
        .unwrap()
    else {
        panic!("expected resolved");
    };
    assert_eq!(
        (
            first.status,
            first.operator_id.as_deref(),
            first.note.as_deref(),
            first.resolved_at
        ),
        (
            ReconciliationStatus::Resolved,
            Some("ops"),
            Some("checked"),
            Some(at)
        )
    );
    let again = store
        .resolve(Uuid::from_u128(1), "other", None, at)
        .await
        .unwrap();
    assert_eq!(again, ResolveOutcome::AlreadyResolved(first));
    let missing = store
        .resolve(Uuid::from_u128(9), "ops", None, at)
        .await
        .unwrap();
    assert_eq!(missing, ResolveOutcome::NotFound);
}

#[tokio::test]
async fn the_ping_follows_the_database() {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    assert!(store.ping(Duration::from_secs(2)).await);
    postgres.stop();
    assert!(!store.ping(Duration::from_secs(2)).await);
    assert!(store.get(id(1)).await.is_err());
}
