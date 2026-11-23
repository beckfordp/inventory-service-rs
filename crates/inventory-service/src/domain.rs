// SPDX-License-Identifier: MIT

//! The pure core, as the Scala domain (`StockLevels`, `OrderReservation`,
//! `Reconciliation`, `OperatorAuth`): values parsed at the edge, so the rest of
//! the service holds only valid ones.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use time::OffsetDateTime;
use uuid::Uuid;

/// Java's `UUID.fromString` for the canonical form: 8-4-4-4-12 hex digits.
/// (Rust's parser also takes braced, URN and unhyphenated forms; Java doesn't.)
#[must_use]
pub fn parse_uuid(raw: &str) -> Option<Uuid> {
    let canonical = raw.len() == 36
        && raw
            .char_indices()
            .all(|(i, c)| matches!(i, 8 | 13 | 18 | 23) == (c == '-'));
    canonical.then(|| Uuid::parse_str(raw).ok()).flatten()
}

/// A stock record's id: a UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InventoryId(Uuid);

impl InventoryId {
    /// The UUID.
    #[must_use]
    pub fn uuid(self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for InventoryId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

/// A malformed id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a UUID")]
pub struct MalformedId;

impl FromStr for InventoryId {
    type Err = MalformedId;

    fn from_str(text: &str) -> Result<Self, MalformedId> {
        parse_uuid(text).map(Self).ok_or(MalformedId)
    }
}

impl fmt::Display for InventoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A stock record, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    /// Its id.
    pub id: InventoryId,
    /// Stock-keeping unit (unique).
    pub sku: String,
    /// Units free to reserve.
    pub quantity_available: i32,
    /// Units held by active reservations.
    pub quantity_reserved: i32,
    /// When it was created (Postgres `now()`).
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

/// Why stock was refused; the reason is the error body's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidInventory(pub String);

/// A new stock record: a non-blank sku, a quantity of at least 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStock {
    /// Non-blank.
    pub sku: String,
    /// At least 0.
    pub quantity_available: i32,
}

fn non_negative(value: i32, field: &str) -> Result<i32, InvalidInventory> {
    if value >= 0 {
        Ok(value)
    } else {
        Err(InvalidInventory(format!("{field} must not be negative")))
    }
}

impl NewStock {
    /// The record, when the sku isn't blank and the quantity isn't negative.
    ///
    /// # Errors
    /// [`InvalidInventory`]: the sku's reason first.
    pub fn parse(sku: &str, quantity_available: i32) -> Result<Self, InvalidInventory> {
        if sku.trim().is_empty() {
            return Err(InvalidInventory("sku must not be blank".to_owned()));
        }
        Ok(Self {
            sku: sku.to_owned(),
            quantity_available: non_negative(quantity_available, "quantityAvailable")?,
        })
    }
}

/// The quantity an update sets: at least 0.
///
/// # Errors
/// [`InvalidInventory`] for a negative one.
pub fn stock_update(quantity_available: i32) -> Result<i32, InvalidInventory> {
    non_negative(quantity_available, "quantityAvailable")
}

/// A line as requested: its ids not yet parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedLine {
    /// order-service's line id; must be a UUID.
    pub order_item_id: String,
    /// The sku to hold (any text: an unknown one is a failure, not a 400).
    pub sku: String,
    /// Units; must be positive.
    pub quantity: i32,
}

/// A valid line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationLine {
    /// order-service's line id.
    pub order_item_id: Uuid,
    /// The sku held.
    pub sku: String,
    /// Units held, > 0.
    pub quantity: i32,
}

/// A valid reservation: an order and at least one line, no line id twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveOrder {
    /// The idempotency key.
    pub order_id: Uuid,
    /// In request order.
    pub items: Vec<ReservationLine>,
}

/// Why a reservation body was refused; the reason is the error body's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidReservation(pub String);

fn uuid_field(raw: &str, field: &str) -> Result<Uuid, InvalidReservation> {
    parse_uuid(raw).ok_or_else(|| InvalidReservation(format!("{field} must be a UUID")))
}

impl ReserveOrder {
    /// The reservation, judged as Scala's `ReserveOrder.validate`: the order id,
    /// then a non-empty list, then each line in order (its id, then its
    /// quantity), then repeated line ids.
    ///
    /// # Errors
    /// [`InvalidReservation`]: the first reason found.
    pub fn validate(order_id: &str, items: &[RequestedLine]) -> Result<Self, InvalidReservation> {
        let order_id = uuid_field(order_id, "orderId")?;
        if items.is_empty() {
            return Err(InvalidReservation("items must not be empty".to_owned()));
        }
        let lines = items
            .iter()
            .map(|item| {
                let order_item_id = uuid_field(&item.order_item_id, "orderItemId")?;
                if item.quantity > 0 {
                    Ok(ReservationLine {
                        order_item_id,
                        sku: item.sku.clone(),
                        quantity: item.quantity,
                    })
                } else {
                    Err(InvalidReservation("quantity must be positive".to_owned()))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let distinct: HashSet<Uuid> = lines.iter().map(|l| l.order_item_id).collect();
        if distinct.len() != lines.len() {
            return Err(InvalidReservation("duplicate orderItemId".to_owned()));
        }
        Ok(Self {
            order_id,
            items: lines,
        })
    }
}

/// Each sku's summed demand over `lines`, in sku order (the order stock is
/// locked and shifted in, against deadlocks).
#[must_use]
pub fn demand(lines: &[ReservationLine]) -> BTreeMap<String, i32> {
    let mut by_sku = BTreeMap::new();
    for line in lines {
        *by_sku.entry(line.sku.clone()).or_insert(0) += line.quantity;
    }
    by_sku
}

/// Why a line couldn't be reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// The sku's summed demand exceeds what's available.
    InsufficientStock,
    /// No stock record has the sku.
    UnknownSku,
}

impl FailureReason {
    /// As the error body says it.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::InsufficientStock => "insufficient_stock",
            Self::UnknownSku => "unknown_sku",
        }
    }
}

/// A line that couldn't be reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationFailure {
    /// The line's id.
    pub order_item_id: Uuid,
    /// Its sku.
    pub sku: String,
    /// Why.
    pub reason: FailureReason,
}

/// Every line that can't be reserved against `available` (sku → units), in
/// request order; demand is summed per sku (in `i64`, as Scala's `Long`), so
/// every line of an oversubscribed sku fails.
#[must_use]
pub fn failures<H: std::hash::BuildHasher>(
    lines: &[ReservationLine],
    available: &HashMap<String, i32, H>,
) -> Vec<ReservationFailure> {
    let mut wanted: HashMap<&str, i64> = HashMap::new();
    for line in lines {
        *wanted.entry(line.sku.as_str()).or_insert(0) += i64::from(line.quantity);
    }
    lines
        .iter()
        .filter_map(|line| {
            let reason = match available.get(&line.sku) {
                None => FailureReason::UnknownSku,
                Some(&qty) if wanted[line.sku.as_str()] > i64::from(qty) => {
                    FailureReason::InsufficientStock
                }
                Some(_) => return None,
            };
            Some(ReservationFailure {
                order_item_id: line.order_item_id,
                sku: line.sku.clone(),
                reason,
            })
        })
        .collect()
}

/// Why a valid reservation wasn't made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationConflict {
    /// Some lines can't be reserved: every one, in request order.
    Rejected(Vec<ReservationFailure>),
    /// The order's reservation was released or shipped.
    Released,
}

/// A ledger line's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStatus {
    /// Holding stock.
    Active,
    /// Returned to available.
    Released,
    /// Gone with the parcel.
    Shipped,
}

impl LineStatus {
    /// As stored and listed.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Released => "released",
            Self::Shipped => "shipped",
        }
    }

    /// The status `raw` names, if any.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Active, Self::Released, Self::Shipped]
            .into_iter()
            .find(|status| status.as_str() == raw)
    }
}

/// One ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    /// The order.
    pub order_id: Uuid,
    /// The line.
    pub order_item_id: Uuid,
    /// Its request position, from 0.
    pub line_no: i32,
    /// The sku held.
    pub sku: String,
    /// Units held.
    pub quantity: i32,
    /// Its status.
    pub status: LineStatus,
    /// When it was reserved.
    pub reserved_at: OffsetDateTime,
    /// When it was released, if it was.
    pub released_at: Option<OffsetDateTime>,
}

/// What a release did: lines released, and lines already shipped (a cancel
/// after dispatch).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseOutcome {
    /// Were active, now released.
    pub released: Vec<ReservationLine>,
    /// Already shipped: nothing to release.
    pub shipped: Vec<ReservationLine>,
}

/// What a dispatch did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShipOutcome {
    /// Were active, now shipped.
    pub shipped: Vec<ReservationLine>,
    /// Shipped before (a duplicate).
    pub already_shipped: Vec<ReservationLine>,
    /// Released before (a dispatch after release).
    pub released: Vec<ReservationLine>,
}

/// A reconciliation flag's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationStatus {
    /// Waiting for an operator.
    Open,
    /// An operator resolved it.
    Resolved,
}

impl ReconciliationStatus {
    /// As stored and listed.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }

    /// The status `raw` names, if any.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Open, Self::Resolved]
            .into_iter()
            .find(|status| status.as_str() == raw)
    }
}

/// The flag a cancel after dispatch raises.
pub const CANCEL_AFTER_DISPATCH: &str = "cancel_after_dispatch";
/// The flag a dispatch after release raises.
pub const DISPATCH_AFTER_RELEASE: &str = "dispatch_after_release";

/// A manual-reconciliation flag, one per order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// The order.
    pub order_id: Uuid,
    /// Why it needs an operator.
    pub reason: String,
    /// Open or resolved.
    pub status: ReconciliationStatus,
    /// When it was raised.
    pub created_at: OffsetDateTime,
    /// Who resolved it.
    pub operator_id: Option<String>,
    /// Their note.
    pub note: Option<String>,
    /// When.
    pub resolved_at: Option<OffsetDateTime>,
}

/// What resolving a flag did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// It was open; now resolved.
    Resolved(Reconciliation),
    /// It was resolved already: as it was.
    AlreadyResolved(Reconciliation),
    /// No flag for the order.
    NotFound,
}

/// The dev operator token (ADR 0024/0026): `Authorization: Bearer <token>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorAuth {
    token: Option<String>,
    operator_id: String,
}

impl OperatorAuth {
    /// From config: a blank token is none (every request refused).
    #[must_use]
    pub fn new(token: &str, operator_id: &str) -> Self {
        let token = token.trim();
        Self {
            token: (!token.is_empty()).then(|| token.to_owned()),
            operator_id: operator_id.trim().to_owned(),
        }
    }

    /// The operator, when `authorization` is `Bearer <token>` (the scheme in
    /// any case, the token trimmed; compared in constant time).
    #[must_use]
    pub fn operator(&self, authorization: Option<&str>) -> Option<&str> {
        const SCHEME: &str = "bearer ";
        let expected = self.token.as_deref()?;
        let raw = authorization?;
        if raw.len() <= SCHEME.len() || !raw.is_char_boundary(SCHEME.len()) {
            return None;
        }
        let (scheme, presented) = raw.split_at(SCHEME.len());
        (scheme.eq_ignore_ascii_case(SCHEME) && constant_eq(presented.trim(), expected))
            .then_some(self.operator_id.as_str())
    }
}

/// Equal bytes, in time independent of where they differ (`MessageDigest.isEqual`).
fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}
