// SPDX-License-Identifier: MIT

//! The service's tables (`inventory`, `order_reservations`,
//! `inventory_reconciliation`), with the Scala `InventoryStore`'s SQL: an
//! order's reservation, release and dispatch each run in one transaction
//! under the order's advisory lock, locking and shifting stock in sku order
//! (deadlock avoidance). Handlers and consumers reach it through
//! [`Inventories`], so they are testable over a store in memory.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::{
    Inventory, InventoryId, LedgerEntry, LineStatus, NewStock, Reconciliation,
    ReconciliationStatus, ReleaseOutcome, ReservationConflict, ReservationLine, ReserveOrder,
    ResolveOutcome, ShipOutcome, demand, failures,
};

/// Why a store call failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database refused or failed.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// A row the schema allows but the service never writes.
    #[error("unexpected {0}")]
    Corrupt(String),
}

/// The sku already has a stock record (`inventory_sku_unique`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Inventory already exists for this sku")]
pub struct DuplicateSku;

/// What the service needs from its tables.
pub trait Inventories: Clone + Send + Sync + 'static {
    /// Stores `stock` as `id`, nothing reserved.
    fn create(
        &self,
        id: InventoryId,
        stock: &NewStock,
    ) -> impl Future<Output = Result<Result<Inventory, DuplicateSku>, StoreError>> + Send;

    /// The record, if there is one.
    fn get(
        &self,
        id: InventoryId,
    ) -> impl Future<Output = Result<Option<Inventory>, StoreError>> + Send;

    /// Sets `quantityAvailable`; the record, if there is one.
    fn update(
        &self,
        id: InventoryId,
        quantity_available: i32,
    ) -> impl Future<Output = Result<Option<Inventory>, StoreError>> + Send;

    /// Deletes the record; whether there was one.
    fn delete(&self, id: InventoryId) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Every record in sku order, or the one with `sku`.
    fn list(
        &self,
        sku: Option<&str>,
    ) -> impl Future<Output = Result<Vec<Inventory>, StoreError>> + Send;

    /// Reserves every line or none; an order already reserved answers its lines.
    fn reserve(
        &self,
        order: &ReserveOrder,
    ) -> impl Future<Output = Result<Result<Vec<ReservationLine>, ReservationConflict>, StoreError>> + Send;

    /// Returns the order's active lines to available.
    fn release(
        &self,
        order_id: Uuid,
    ) -> impl Future<Output = Result<ReleaseOutcome, StoreError>> + Send;

    /// Marks the order's active lines shipped: reserved goes down.
    fn ship(&self, order_id: Uuid) -> impl Future<Output = Result<ShipOutcome, StoreError>> + Send;

    /// Flags the order for reconciliation, if it has a reservation and no
    /// flag yet; whether it was flagged.
    fn flag(
        &self,
        order_id: Uuid,
        reason: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Flags, newest first (ties by order), filtered.
    fn reconciliations(
        &self,
        status: Option<ReconciliationStatus>,
        order_id: Option<Uuid>,
    ) -> impl Future<Output = Result<Vec<Reconciliation>, StoreError>> + Send;

    /// Resolves the order's open flag; the first resolver wins.
    fn resolve(
        &self,
        order_id: Uuid,
        operator_id: &str,
        note: Option<&str>,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<ResolveOutcome, StoreError>> + Send;

    /// Ledger rows by order then line, filtered.
    fn ledger(
        &self,
        order_id: Option<Uuid>,
        status: Option<LineStatus>,
    ) -> impl Future<Output = Result<Vec<LedgerEntry>, StoreError>> + Send;

    /// True when the database answers within `within` (readiness).
    fn ping(&self, within: Duration) -> impl Future<Output = bool> + Send;
}

/// The tables in Postgres.
#[derive(Debug, Clone)]
pub struct PgInventories {
    pool: PgPool,
}

impl PgInventories {
    /// Over `pool`, migrated.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

type StockRow = (Uuid, String, i32, i32, OffsetDateTime, OffsetDateTime);

fn stock(
    (id, sku, quantity_available, quantity_reserved, created_at, updated_at): StockRow,
) -> Inventory {
    Inventory {
        id: id.into(),
        sku,
        quantity_available,
        quantity_reserved,
        created_at,
        updated_at,
    }
}

const STOCK: &str = "id, sku, quantity_available, quantity_reserved, created_at, updated_at";

type LedgerRow = (
    Uuid,
    Uuid,
    i32,
    String,
    i32,
    String,
    OffsetDateTime,
    Option<OffsetDateTime>,
);

type FlagRow = (
    Uuid,
    String,
    String,
    OffsetDateTime,
    Option<String>,
    Option<String>,
    Option<OffsetDateTime>,
);

const FLAG: &str = "order_id, reason, status, created_at, operator_id, note, resolved_at";

fn line_status(raw: &str) -> Result<LineStatus, StoreError> {
    LineStatus::parse(raw).ok_or_else(|| StoreError::Corrupt(format!("ledger status {raw:?}")))
}

fn flag(
    (order_id, reason, status, created_at, operator_id, note, resolved_at): FlagRow,
) -> Result<Reconciliation, StoreError> {
    let status = ReconciliationStatus::parse(&status)
        .ok_or_else(|| StoreError::Corrupt(format!("reconciliation status {status:?}")))?;
    Ok(Reconciliation {
        order_id,
        reason,
        status,
        created_at,
        operator_id,
        note,
        resolved_at,
    })
}

/// The order's advisory lock, held to the end of the transaction.
async fn lock_order(tx: &mut PgConnection, order_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(order_id.to_string())
        .execute(tx)
        .await?;
    Ok(())
}

/// The order's lines, in line order, with their status.
async fn lines_of(
    tx: &mut PgConnection,
    order_id: Uuid,
) -> Result<Vec<(ReservationLine, LineStatus)>, StoreError> {
    let rows: Vec<(Uuid, String, i32, String)> = sqlx::query_as(
        r#"SELECT order_item_id, sku, quantity, status FROM "order_reservations"
           WHERE order_id = $1 ORDER BY line_no"#,
    )
    .bind(order_id)
    .fetch_all(tx)
    .await?;
    rows.into_iter()
        .map(|(order_item_id, sku, quantity, status)| {
            let line = ReservationLine {
                order_item_id,
                sku,
                quantity,
            };
            Ok((line, line_status(&status)?))
        })
        .collect()
}

fn with_status(
    lines: &[(ReservationLine, LineStatus)],
    wanted: LineStatus,
) -> Vec<ReservationLine> {
    lines
        .iter()
        .filter(|(_, status)| *status == wanted)
        .map(|(line, _)| line.clone())
        .collect()
}

impl PgInventories {
    async fn reserve_new(
        tx: &mut PgConnection,
        order: &ReserveOrder,
    ) -> Result<Result<Vec<ReservationLine>, ReservationConflict>, StoreError> {
        let wanted = demand(&order.items);
        let mut available = HashMap::new();
        for sku in wanted.keys() {
            let row: Option<(i32,)> = sqlx::query_as(
                r#"SELECT quantity_available FROM "inventory" WHERE sku = $1 FOR UPDATE"#,
            )
            .bind(sku)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((qty,)) = row {
                available.insert(sku.clone(), qty);
            }
        }
        let failed = failures(&order.items, &available);
        if !failed.is_empty() {
            return Ok(Err(ReservationConflict::Rejected(failed)));
        }
        for (sku, qty) in &wanted {
            sqlx::query(
                r#"UPDATE "inventory" SET quantity_available = quantity_available - $1,
                   quantity_reserved = quantity_reserved + $1, updated_at = now() WHERE sku = $2"#,
            )
            .bind(qty)
            .bind(sku)
            .execute(&mut *tx)
            .await?;
        }
        for (line_no, line) in (0..).zip(&order.items) {
            sqlx::query(
                r#"INSERT INTO "order_reservations" (order_id, order_item_id, line_no, sku, quantity, status)
                   VALUES ($1, $2, $3, $4, $5, 'active')"#,
            )
            .bind(order.order_id)
            .bind(line.order_item_id)
            .bind::<i32>(line_no)
            .bind(&line.sku)
            .bind(line.quantity)
            .execute(&mut *tx)
            .await?;
        }
        Ok(Ok(order.items.clone()))
    }
}

impl Inventories for PgInventories {
    async fn create(
        &self,
        id: InventoryId,
        new: &NewStock,
    ) -> Result<Result<Inventory, DuplicateSku>, StoreError> {
        let row: Result<StockRow, sqlx::Error> = sqlx::query_as(&format!(
            r#"INSERT INTO "inventory" (id, sku, quantity_available, quantity_reserved)
               VALUES ($1, $2, $3, 0) RETURNING {STOCK}"#
        ))
        .bind(id.uuid())
        .bind(&new.sku)
        .bind(new.quantity_available)
        .fetch_one(&self.pool)
        .await;
        match row {
            Ok(row) => Ok(Ok(stock(row))),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                Ok(Err(DuplicateSku))
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn get(&self, id: InventoryId) -> Result<Option<Inventory>, StoreError> {
        let row: Option<StockRow> =
            sqlx::query_as(&format!(r#"SELECT {STOCK} FROM "inventory" WHERE id = $1"#))
                .bind(id.uuid())
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(stock))
    }

    async fn update(
        &self,
        id: InventoryId,
        quantity_available: i32,
    ) -> Result<Option<Inventory>, StoreError> {
        let row: Option<StockRow> = sqlx::query_as(&format!(
            r#"UPDATE "inventory" SET quantity_available = $1, updated_at = now()
               WHERE id = $2 RETURNING {STOCK}"#
        ))
        .bind(quantity_available)
        .bind(id.uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(stock))
    }

    async fn delete(&self, id: InventoryId) -> Result<bool, StoreError> {
        let done = sqlx::query(r#"DELETE FROM "inventory" WHERE id = $1"#)
            .bind(id.uuid())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn list(&self, sku: Option<&str>) -> Result<Vec<Inventory>, StoreError> {
        let rows: Vec<StockRow> = match sku {
            None => {
                sqlx::query_as(&format!(r#"SELECT {STOCK} FROM "inventory" ORDER BY sku"#))
                    .fetch_all(&self.pool)
                    .await?
            }
            Some(sku) => {
                sqlx::query_as(&format!(
                    r#"SELECT {STOCK} FROM "inventory" WHERE sku = $1"#
                ))
                .bind(sku)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(stock).collect())
    }

    async fn reserve(
        &self,
        order: &ReserveOrder,
    ) -> Result<Result<Vec<ReservationLine>, ReservationConflict>, StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_order(&mut tx, order.order_id).await?;
        let existing = lines_of(&mut tx, order.order_id).await?;
        let outcome = if existing.is_empty() {
            Self::reserve_new(&mut tx, order).await?
        } else if existing.iter().any(|(_, s)| *s == LineStatus::Active) {
            Ok(existing.into_iter().map(|(line, _)| line).collect())
        } else {
            Err(ReservationConflict::Released)
        };
        tx.commit().await?;
        Ok(outcome)
    }

    async fn release(&self, order_id: Uuid) -> Result<ReleaseOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_order(&mut tx, order_id).await?;
        let before = lines_of(&mut tx, order_id).await?;
        sqlx::query(
            r#"UPDATE "order_reservations" SET status = 'released', released_at = now()
               WHERE order_id = $1 AND status = 'active'"#,
        )
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
        let released = with_status(&before, LineStatus::Active);
        for (sku, qty) in demand(&released) {
            sqlx::query(
                r#"UPDATE "inventory" SET quantity_available = quantity_available + $1,
                   quantity_reserved = quantity_reserved - $1, updated_at = now() WHERE sku = $2"#,
            )
            .bind(qty)
            .bind(sku)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(ReleaseOutcome {
            released,
            shipped: with_status(&before, LineStatus::Shipped),
        })
    }

    async fn ship(&self, order_id: Uuid) -> Result<ShipOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        lock_order(&mut tx, order_id).await?;
        let before = lines_of(&mut tx, order_id).await?;
        sqlx::query(
            r#"UPDATE "order_reservations" SET status = 'shipped'
               WHERE order_id = $1 AND status = 'active'"#,
        )
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
        let shipped = with_status(&before, LineStatus::Active);
        for (sku, qty) in demand(&shipped) {
            sqlx::query(
                r#"UPDATE "inventory" SET quantity_reserved = quantity_reserved - $1,
                   updated_at = now() WHERE sku = $2"#,
            )
            .bind(qty)
            .bind(sku)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(ShipOutcome {
            shipped,
            already_shipped: with_status(&before, LineStatus::Shipped),
            released: with_status(&before, LineStatus::Released),
        })
    }

    async fn flag(&self, order_id: Uuid, reason: &str) -> Result<bool, StoreError> {
        let row: Option<(Uuid,)> = sqlx::query_as(
            r"INSERT INTO inventory_reconciliation (order_id, reason)
              SELECT $1, $2 WHERE EXISTS (SELECT 1 FROM order_reservations WHERE order_id = $1)
              ON CONFLICT (order_id) DO NOTHING RETURNING order_id",
        )
        .bind(order_id)
        .bind(reason)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn reconciliations(
        &self,
        status: Option<ReconciliationStatus>,
        order_id: Option<Uuid>,
    ) -> Result<Vec<Reconciliation>, StoreError> {
        let rows: Vec<FlagRow> = sqlx::query_as(&format!(
            r"SELECT {FLAG} FROM inventory_reconciliation
              WHERE ($1::text IS NULL OR status = $1) AND ($2::uuid IS NULL OR order_id = $2)
              ORDER BY created_at DESC, order_id"
        ))
        .bind(status.map(ReconciliationStatus::as_str))
        .bind(order_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(flag).collect()
    }

    async fn resolve(
        &self,
        order_id: Uuid,
        operator_id: &str,
        note: Option<&str>,
        at: OffsetDateTime,
    ) -> Result<ResolveOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        let resolved: Option<FlagRow> = sqlx::query_as(&format!(
            r"UPDATE inventory_reconciliation
              SET status = 'resolved', operator_id = $1, note = $2, resolved_at = $3
              WHERE order_id = $4 AND status = 'open' RETURNING {FLAG}"
        ))
        .bind(operator_id)
        .bind(note)
        .bind(at)
        .bind(order_id)
        .fetch_optional(&mut *tx)
        .await?;
        let outcome = if let Some(row) = resolved {
            ResolveOutcome::Resolved(flag(row)?)
        } else {
            let existing: Option<FlagRow> = sqlx::query_as(&format!(
                r"SELECT {FLAG} FROM inventory_reconciliation WHERE order_id = $1"
            ))
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?;
            match existing {
                Some(row) => ResolveOutcome::AlreadyResolved(flag(row)?),
                None => ResolveOutcome::NotFound,
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    async fn ledger(
        &self,
        order_id: Option<Uuid>,
        status: Option<LineStatus>,
    ) -> Result<Vec<LedgerEntry>, StoreError> {
        let rows: Vec<LedgerRow> = sqlx::query_as(
            r#"SELECT order_id, order_item_id, line_no, sku, quantity, status, created_at, released_at
               FROM "order_reservations"
               WHERE order_id = COALESCE($1, order_id) AND status = COALESCE($2, status)
               ORDER BY order_id, line_no"#,
        )
        .bind(order_id)
        .bind(status.map(LineStatus::as_str))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(
                    order_id,
                    order_item_id,
                    line_no,
                    sku,
                    quantity,
                    status,
                    reserved_at,
                    released_at,
                )| {
                    Ok(LedgerEntry {
                        order_id,
                        order_item_id,
                        line_no,
                        sku,
                        quantity,
                        status: line_status(&status)?,
                        reserved_at,
                        released_at,
                    })
                },
            )
            .collect()
    }

    async fn ping(&self, within: Duration) -> bool {
        service_basics::postgres::ping(&self.pool, within)
            .await
            .is_ok()
    }
}
