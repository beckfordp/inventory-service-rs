// SPDX-License-Identifier: MIT

//! The routes, as the Scala `InventoryRoutes`, `OrderReservationRoutes` and
//! `ReconciliationRoutes`: the path, query and body decoded first (tapir's
//! plain-text 400), then the rules (400 with the JSON reason), then the store
//! (404, 409). A store failure is a 500.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use service_basics::jvm::java_instant;
use service_basics::text_error::TextError;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::{
    InvalidInventory, InvalidReservation, Inventory, InventoryId, LedgerEntry, LineStatus,
    NewStock, Reconciliation, ReconciliationStatus, RequestedLine, ReservationConflict,
    ReservationLine, ReserveOrder, ResolveOutcome, parse_uuid, stock_update,
};
use crate::server::AppState;
use crate::store::{Inventories, StoreError};

/// A stock record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InventoryResponse {
    /// Inventory id (UUID).
    pub id: String,
    /// Stock-keeping unit.
    pub sku: String,
    /// Units free to reserve.
    pub quantity_available: i32,
    /// Units held by active reservations.
    pub quantity_reserved: i32,
    /// When it was created (UTC).
    pub created_at: String,
    /// When it last changed (UTC).
    pub updated_at: String,
}

impl From<Inventory> for InventoryResponse {
    fn from(stock: Inventory) -> Self {
        Self {
            id: stock.id.to_string(),
            sku: stock.sku,
            quantity_available: stock.quantity_available,
            quantity_reserved: stock.quantity_reserved,
            created_at: java_instant(stock.created_at),
            updated_at: java_instant(stock.updated_at),
        }
    }
}

/// A new stock record.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateInventoryRequest {
    /// Stock-keeping unit, unique.
    pub sku: String,
    /// Units on hand, at least 0.
    pub quantity_available: i32,
}

/// The quantity to set.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInventoryRequest {
    /// Units on hand, at least 0.
    pub quantity_available: i32,
}

/// One line to reserve.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReserveOrderItemRequest {
    /// order-service line id (UUID).
    pub order_item_id: String,
    /// SKU to reserve.
    pub sku: String,
    /// Units to reserve, > 0.
    pub quantity: i32,
}

/// A bulk stock reservation for one order.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReserveOrderRequest {
    /// Order id (UUID); the idempotency key.
    pub order_id: String,
    /// Lines to reserve, at least one; all-or-nothing.
    pub items: Vec<ReserveOrderItemRequest>,
}

/// One reserved line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReservationLineResponse {
    /// order-service line id (UUID).
    pub order_item_id: String,
    /// SKU held.
    pub sku: String,
    /// Units held.
    pub quantity: i32,
}

/// The lines the order now holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrderReservationResponse {
    /// Order id (UUID).
    pub order_id: String,
    /// Every line the order now holds.
    pub items: Vec<ReservationLineResponse>,
}

/// A line that couldn't be reserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReservationFailureResponse {
    /// Failed line id (UUID).
    pub order_item_id: String,
    /// SKU that could not be reserved.
    pub sku: String,
    /// `insufficient_stock` or `unknown_sku`.
    pub reason: String,
}

/// Error body for lines that couldn't be reserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ReservationRejectedResponse {
    /// Always `Insufficient stock`.
    pub error: String,
    /// Every failed line, in request order.
    pub failures: Vec<ReservationFailureResponse>,
}

/// One reservation-ledger row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReservationLedgerEntryResponse {
    /// Order id (UUID).
    pub order_id: String,
    /// order-service line id (UUID).
    pub order_item_id: String,
    /// Request position within the order.
    pub line_no: i32,
    /// SKU held.
    pub sku: String,
    /// Units held.
    pub quantity: i32,
    /// `active`, `released` or `shipped`.
    pub status: String,
    /// When the line was reserved.
    pub reserved_at: String,
    /// When the line was released, if it was.
    pub released_at: Option<String>,
}

impl From<LedgerEntry> for ReservationLedgerEntryResponse {
    fn from(entry: LedgerEntry) -> Self {
        Self {
            order_id: entry.order_id.to_string(),
            order_item_id: entry.order_item_id.to_string(),
            line_no: entry.line_no,
            sku: entry.sku,
            quantity: entry.quantity,
            status: entry.status.as_str().to_owned(),
            reserved_at: java_instant(entry.reserved_at),
            released_at: entry.released_at.map(java_instant),
        }
    }
}

/// One manual-reconciliation flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReconciliationResponse {
    /// Order the flag belongs to.
    pub order_id: String,
    /// Why manual reconciliation is needed.
    pub reason: String,
    /// `open` or `resolved`.
    pub status: String,
    /// When the flag was recorded.
    pub created_at: String,
    /// Operator who resolved it, when resolved.
    pub operator_id: Option<String>,
    /// Operator's note, when supplied.
    pub note: Option<String>,
    /// When it was resolved.
    pub resolved_at: Option<String>,
}

impl From<Reconciliation> for ReconciliationResponse {
    fn from(flag: Reconciliation) -> Self {
        Self {
            order_id: flag.order_id.to_string(),
            reason: flag.reason,
            status: flag.status.as_str().to_owned(),
            created_at: java_instant(flag.created_at),
            operator_id: flag.operator_id,
            note: flag.note,
            resolved_at: flag.resolved_at.map(java_instant),
        }
    }
}

/// Resolve a reconciliation flag.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ResolveReconciliationRequest {
    /// Only `resolved` is accepted.
    pub status: String,
    /// Optional operator note.
    pub note: Option<String>,
}

/// Error body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ErrorResponse {
    /// Human-readable reason.
    pub error: String,
}

/// `GET /inventorys`'s query.
#[derive(Debug, Default, Deserialize)]
pub struct StockQuery {
    /// Exact sku.
    pub sku: Option<String>,
}

/// The ledger's and the reconciliations' filters.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterQuery {
    /// An order (UUID).
    pub order_id: Option<String>,
    /// A status.
    pub status: Option<String>,
}

/// The reconciliation-status 400's reason, whatever status was asked for (as
/// tapir's single error variant maps it).
const ONLY_RESOLVED: &str = "Invalid reconciliation status: only 'resolved' is accepted";

/// Why a route didn't answer its success.
#[derive(Debug)]
pub enum RouteError {
    /// tapir's text 400: what didn't decode (`path parameter id`, `body (…)`).
    Decode(String),
    /// A rule's 400, a 401, 404 or 409, with its JSON reason.
    Refused(StatusCode, String),
    /// 409 listing the lines that couldn't be reserved.
    Rejected(ReservationRejectedResponse),
    /// Postgres failed.
    Store(StoreError),
}

impl From<StoreError> for RouteError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<InvalidInventory> for RouteError {
    fn from(error: InvalidInventory) -> Self {
        Self::Refused(StatusCode::BAD_REQUEST, error.0)
    }
}

impl From<InvalidReservation> for RouteError {
    fn from(error: InvalidReservation) -> Self {
        Self::Refused(StatusCode::BAD_REQUEST, error.0)
    }
}

impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        match self {
            Self::Decode(what) => text(
                StatusCode::BAD_REQUEST,
                format!("Invalid value for: {what}"),
            ),
            Self::Refused(status, error) => (status, Json(ErrorResponse { error })).into_response(),
            Self::Rejected(body) => (StatusCode::CONFLICT, Json(body)).into_response(),
            Self::Store(_) => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_owned(),
            ),
        }
    }
}

fn text(status: StatusCode, message: String) -> Response {
    TextError { status, message }.into_response()
}

fn refused(status: StatusCode, reason: &str) -> RouteError {
    RouteError::Refused(status, reason.to_owned())
}

fn not_found() -> RouteError {
    refused(StatusCode::NOT_FOUND, "Inventory not found")
}

fn inventory_id(raw: &str) -> Result<InventoryId, RouteError> {
    raw.parse()
        .map_err(|_| RouteError::Decode("path parameter id".to_owned()))
}

fn body<T: serde::de::DeserializeOwned>(bytes: &Bytes) -> Result<T, RouteError> {
    serde_json::from_slice(bytes).map_err(|error| RouteError::Decode(format!("body ({error})")))
}

/// Logs a failure the way the Scala routes do, and passes it on.
fn logged<T, S>(state: &AppState<S>, result: Result<T, RouteError>) -> Result<T, RouteError> {
    match &result {
        Err(RouteError::Store(error)) => state
            .logger
            .error(&[("error", &error.to_string())], "Request failed"),
        Err(RouteError::Refused(_, reason)) => {
            state.logger.warn(&[("reason", reason)], "Request refused");
        }
        Err(RouteError::Rejected(body)) => state.logger.warn(
            &[("failed_lines", &body.failures.len().to_string())],
            "Reservation rejected",
        ),
        _ => {}
    }
    result
}

/// `POST /inventorys` (201).
///
/// # Errors
/// An undecodable body or a broken rule (400); a used sku (409); a store failure (500).
pub async fn create<S: Inventories>(
    State(state): State<AppState<S>>,
    bytes: Bytes,
) -> Result<(StatusCode, Json<InventoryResponse>), RouteError> {
    let request: CreateInventoryRequest = body(&bytes)?;
    let result = async {
        let new = NewStock::parse(&request.sku, request.quantity_available)?;
        state
            .store
            .create(InventoryId::from(Uuid::new_v4()), &new)
            .await?
            .map_err(|duplicate| refused(StatusCode::CONFLICT, &duplicate.to_string()))
    }
    .await;
    let stock = logged(&state, result)?;
    Ok((StatusCode::CREATED, Json(stock.into())))
}

/// `GET /inventorys/{id}`.
///
/// # Errors
/// A malformed id (400), no such record (404), a store failure (500).
pub async fn get<S: Inventories>(
    State(state): State<AppState<S>>,
    Path(raw): Path<String>,
) -> Result<Json<InventoryResponse>, RouteError> {
    let id = inventory_id(&raw)?;
    let result = state
        .store
        .get(id)
        .await
        .map_err(RouteError::from)
        .and_then(|s| s.ok_or_else(not_found));
    Ok(Json(logged(&state, result)?.into()))
}

/// `PATCH` and `PUT /inventorys/{id}`: the body judged before the lookup.
///
/// # Errors
/// A malformed id or body, a negative quantity (400), no such record (404), a
/// store failure (500).
pub async fn update<S: Inventories>(
    State(state): State<AppState<S>>,
    Path(raw): Path<String>,
    bytes: Bytes,
) -> Result<Json<InventoryResponse>, RouteError> {
    let id = inventory_id(&raw)?;
    let request: UpdateInventoryRequest = body(&bytes)?;
    let result = async {
        let quantity = stock_update(request.quantity_available)?;
        state
            .store
            .update(id, quantity)
            .await?
            .ok_or_else(not_found)
    }
    .await;
    Ok(Json(logged(&state, result)?.into()))
}

/// `DELETE /inventorys/{id}` (204).
///
/// # Errors
/// A malformed id (400), no such record (404), a store failure (500).
pub async fn delete<S: Inventories>(
    State(state): State<AppState<S>>,
    Path(raw): Path<String>,
) -> Result<StatusCode, RouteError> {
    let id = inventory_id(&raw)?;
    let result = match state.store.delete(id).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(not_found()),
        Err(error) => Err(error.into()),
    };
    logged(&state, result)
}

/// `GET /inventorys?sku`: every record in sku order, or the sku's.
///
/// # Errors
/// A store failure (500).
pub async fn list<S: Inventories>(
    State(state): State<AppState<S>>,
    Query(query): Query<StockQuery>,
) -> Result<Json<Vec<InventoryResponse>>, RouteError> {
    let result = state
        .store
        .list(query.sku.as_deref())
        .await
        .map_err(RouteError::from);
    let found = logged(&state, result)?;
    Ok(Json(
        found.into_iter().map(InventoryResponse::from).collect(),
    ))
}

fn reservation(order_id: Uuid, lines: Vec<ReservationLine>) -> OrderReservationResponse {
    OrderReservationResponse {
        order_id: order_id.to_string(),
        items: lines
            .into_iter()
            .map(|line| ReservationLineResponse {
                order_item_id: line.order_item_id.to_string(),
                sku: line.sku,
                quantity: line.quantity,
            })
            .collect(),
    }
}

fn conflict(error: ReservationConflict) -> RouteError {
    match error {
        ReservationConflict::Released => refused(
            StatusCode::CONFLICT,
            "Order reservation already released or shipped",
        ),
        ReservationConflict::Rejected(failures) => {
            RouteError::Rejected(ReservationRejectedResponse {
                error: "Insufficient stock".to_owned(),
                failures: failures
                    .into_iter()
                    .map(|f| ReservationFailureResponse {
                        order_item_id: f.order_item_id.to_string(),
                        sku: f.sku,
                        reason: f.reason.wire().to_owned(),
                    })
                    .collect(),
            })
        }
    }
}

/// `POST /inventorys/order-reservations` (201): all-or-nothing, idempotent per order.
///
/// # Errors
/// An undecodable or invalid body (400); lines that can't be reserved or a
/// released order (409); a store failure (500).
pub async fn reserve<S: Inventories>(
    State(state): State<AppState<S>>,
    bytes: Bytes,
) -> Result<(StatusCode, Json<OrderReservationResponse>), RouteError> {
    let request: ReserveOrderRequest = body(&bytes)?;
    let result = async {
        let lines: Vec<RequestedLine> = request
            .items
            .into_iter()
            .map(|item| RequestedLine {
                order_item_id: item.order_item_id,
                sku: item.sku,
                quantity: item.quantity,
            })
            .collect();
        let order = ReserveOrder::validate(&request.order_id, &lines)?;
        let held = state.store.reserve(&order).await?.map_err(conflict)?;
        Ok(reservation(order.order_id, held))
    }
    .await;
    Ok((StatusCode::CREATED, Json(logged(&state, result)?)))
}

/// `POST /inventorys/order-reservations/{orderId}/release`: always 204 (a
/// non-UUID holds nothing).
///
/// # Errors
/// A store failure (500).
pub async fn release<S: Inventories>(
    State(state): State<AppState<S>>,
    Path(raw): Path<String>,
) -> Result<StatusCode, RouteError> {
    let Some(order_id) = parse_uuid(&raw) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let result = state
        .store
        .release(order_id)
        .await
        .map_err(RouteError::from);
    logged(&state, result)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /inventorys/order-reservations?orderId&status`: the ledger.
///
/// # Errors
/// A non-UUID orderId or an unknown status (400); a store failure (500).
pub async fn ledger<S: Inventories>(
    State(state): State<AppState<S>>,
    Query(query): Query<FilterQuery>,
) -> Result<Json<Vec<ReservationLedgerEntryResponse>>, RouteError> {
    let result = async {
        let order_id = query
            .order_id
            .as_deref()
            .map(|raw| {
                parse_uuid(raw)
                    .ok_or_else(|| refused(StatusCode::BAD_REQUEST, "orderId must be a UUID"))
            })
            .transpose()?;
        let status = query
            .status
            .as_deref()
            .map(|raw| {
                LineStatus::parse(raw).ok_or_else(|| {
                    refused(
                        StatusCode::BAD_REQUEST,
                        "status must be active, released or shipped",
                    )
                })
            })
            .transpose()?;
        Ok(state.store.ledger(order_id, status).await?)
    }
    .await;
    let rows = logged(&state, result)?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// Trimmed, and absent when blank (as the Scala list reads its filters).
fn filter(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|text| !text.is_empty())
}

/// `GET /reconciliations?status&orderId`: flags, newest first.
///
/// # Errors
/// An unknown status (400); a store failure (500).
pub async fn reconciliations<S: Inventories>(
    State(state): State<AppState<S>>,
    Query(query): Query<FilterQuery>,
) -> Result<Json<Vec<ReconciliationResponse>>, RouteError> {
    let result = async {
        let status = filter(query.status.as_deref())
            .map(|raw| {
                ReconciliationStatus::parse(raw)
                    .ok_or_else(|| refused(StatusCode::BAD_REQUEST, ONLY_RESOLVED))
            })
            .transpose()?;
        let order_id = match filter(query.order_id.as_deref()) {
            None => None,
            Some(raw) => match parse_uuid(raw) {
                Some(id) => Some(id),
                None => return Ok(Vec::new()),
            },
        };
        Ok(state.store.reconciliations(status, order_id).await?)
    }
    .await;
    let flags = logged(&state, result)?;
    Ok(Json(flags.into_iter().map(Into::into).collect()))
}

/// `PATCH /reconciliations/{orderId}`: an operator resolves the flag; the
/// first resolver wins.
///
/// # Errors
/// A malformed id or body (400); no valid operator token (401); a status other
/// than `resolved` (400); no flag (404); a store failure (500).
pub async fn resolve<S: Inventories>(
    State(state): State<AppState<S>>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<ReconciliationResponse>, RouteError> {
    let order_id =
        parse_uuid(&raw).ok_or_else(|| RouteError::Decode("path parameter orderId".to_owned()))?;
    let request: ResolveReconciliationRequest = body(&bytes)?;
    let result = async {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let operator = state
            .auth
            .operator(authorization)
            .ok_or_else(|| refused(StatusCode::UNAUTHORIZED, "Invalid operator token"))?;
        if request.status.trim() != ReconciliationStatus::Resolved.as_str() {
            return Err(refused(StatusCode::BAD_REQUEST, ONLY_RESOLVED));
        }
        let outcome = state
            .store
            .resolve(
                order_id,
                operator,
                request.note.as_deref(),
                OffsetDateTime::now_utc(),
            )
            .await?;
        match outcome {
            ResolveOutcome::Resolved(flag) | ResolveOutcome::AlreadyResolved(flag) => Ok(flag),
            ResolveOutcome::NotFound => Err(refused(
                StatusCode::NOT_FOUND,
                "Reconciliation flag not found",
            )),
        }
    }
    .await;
    Ok(Json(logged(&state, result)?.into()))
}
