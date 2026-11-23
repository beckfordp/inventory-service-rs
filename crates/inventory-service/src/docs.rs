// SPDX-License-Identifier: MIT

//! The service's API description: what tapir generated from the Scala
//! endpoints (capture 20261122 openapi.yaml), stated once here next to the
//! DTO types it derives the schemas from, served with Swagger UI (INV-D1); and
//! the AsyncAPI document of the two topics it consumes, with its page (INV-D2).
//! The resolve operation's optional `Authorization` header parameter is not
//! listed (the builder has path and query parameters only).

use axum::Router;
use serde_json::{Value, json};
use telemetry::docs::asyncapi::{self, Channel};
use telemetry::docs::openapi::{Api, Operation};
use telemetry::docs::swagger;

use crate::events::{
    DISPATCH_CONFIRMED, DispatchConfirmed, FULFILMENT_EVENTS, ORDER_CANCELLED, ORDER_EVENTS,
    OrderCancelled,
};
use crate::routes::{
    CreateInventoryRequest, ErrorResponse, InventoryResponse, OrderReservationResponse,
    ReconciliationResponse, ReservationFailureResponse, ReservationLedgerEntryResponse,
    ReservationLineResponse, ReservationRejectedResponse, ReserveOrderItemRequest,
    ReserveOrderRequest, ResolveReconciliationRequest, UpdateInventoryRequest,
};

const ID: &str = "3f2b8c1e-7a4d-4e9b-9c2a-1d5e6f7a8b90";
const ORDER: &str = "5c1d9e2f-8b3a-4c7d-9e0f-2a1b3c4d5e6f";
const LINE: &str = "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d";
const SKU: &str = "watch-rolex-submariner";
const INVALID: &str =
    "The body failed validation: a blank `sku` (create only) or a negative `quantityAvailable`.";

fn stock() -> Value {
    json!({
        "id": ID,
        "sku": SKU,
        "quantityAvailable": 25,
        "quantityReserved": 3,
        "createdAt": "2026-11-01T12:00:00Z",
        "updatedAt": "2026-11-01T12:05:00Z",
    })
}

fn reservation() -> Value {
    json!({"orderId": ORDER, "items": [{"orderItemId": LINE, "sku": SKU, "quantity": 2}]})
}

fn flag() -> Value {
    json!({
        "orderId": ID,
        "reason": "dispatch_after_release",
        "status": "open",
        "createdAt": "2026-11-02T10:15:30Z",
    })
}

/// A JSON body: its schema component and example. (Always `Some`: the
/// builder takes `None` for a response without one.)
#[allow(clippy::unnecessary_wraps)]
fn body(schema: &'static str, example: Value) -> Option<(&'static str, Value)> {
    Some((schema, example))
}

#[allow(clippy::unnecessary_wraps)]
fn error(message: &str) -> Option<(&'static str, Value)> {
    Some(("ErrorResponse", json!({ "error": message })))
}

fn by_id(operation: Operation, summary: &str, description: &str) -> Operation {
    operation
        .tag("inventorys")
        .summary(summary)
        .description(description)
        .path_parameter("id", "Inventory record id (UUID).", json!(ID))
}

fn not_found(operation: Operation) -> Operation {
    operation.response(
        404,
        "No inventory record with this id.",
        error("Inventory not found"),
    )
}

fn update(operation: Operation, summary: &str, description: &str) -> Operation {
    not_found(
        by_id(operation, summary, description)
            .request_body("UpdateInventoryRequest", json!({"quantityAvailable": 25}))
            .response(200, "", body("InventoryResponse", stock()))
            .response(
                400,
                INVALID,
                error("quantityAvailable must not be negative"),
            ),
    )
}

fn stock_operations(api: Api) -> Api {
    api.operation(
        Operation::get("/inventorys")
            .tag("inventorys")
            .summary("List inventory")
            .description(
                "Stock records ordered by SKU. Optional exact `sku` filter; an unknown filter \
                 returns `[]`. Never 404s.",
            )
            .query_parameter("sku", "Exact SKU to filter by.", json!(SKU))
            .response(200, "", body("InventoryResponse[]", json!([stock()]))),
    )
    .operation(
        Operation::post("/inventorys")
            .tag("inventorys")
            .summary("Create inventory")
            .description(
                "Creates a stock record for a SKU; `quantityReserved` starts at 0. Body is \
                 validated first: blank `sku` or a negative quantity → 400. SKU is unique: an \
                 existing SKU → 409. Not idempotent — each successful call creates a new record.",
            )
            .request_body(
                "CreateInventoryRequest",
                json!({"sku": SKU, "quantityAvailable": 25}),
            )
            .response(201, "", body("InventoryResponse", stock()))
            .response(
                400,
                INVALID,
                error("quantityAvailable must not be negative"),
            )
            .response(
                409,
                "A stock record already exists for this SKU.",
                error("Inventory already exists for this sku"),
            ),
    )
    .operation(not_found(
        by_id(
            Operation::get("/inventorys/{id}"),
            "Get inventory",
            "One stock record by id. 404 for an unknown id; a malformed UUID is rejected by the \
             path decoder as 400.",
        )
        .response(200, "", body("InventoryResponse", stock())),
    ))
    .operation(update(
        Operation::put("/inventorys/{id}"),
        "Update inventory (replace)",
        "Same as PATCH — sets `quantityAvailable` only; kept for PUT clients.",
    ))
    .operation(not_found(
        by_id(
            Operation::delete("/inventorys/{id}"),
            "Delete inventory",
            "Hard-deletes the record; repeat → 404. Past reservation ledger lines for the SKU \
             are left untouched.",
        )
        .response(204, "", None),
    ))
    .operation(update(
        Operation::patch("/inventorys/{id}"),
        "Update inventory",
        "Sets `quantityAvailable`. Idempotent. `quantityReserved` is changed only by reserve, \
         release and ship on the `order_reservations` ledger; a sent value is ignored. Body is \
         checked first (400), then existence (404).",
    ))
}

fn reservation_operations(api: Api) -> Api {
    api.operation(
        Operation::get("/inventorys/order-reservations")
            .tag("order-reservations")
            .summary("List the reservation ledger")
            .description(
                "Admin read of every reservation line, optionally filtered by orderId (UUID) and \
                 status (`active`/`released`/`shipped`). Returns ledger rows in order/line order. \
                 Read-only; no event is published.",
            )
            .query_parameter(
                "orderId",
                "Only this order's ledger rows; must be a UUID.",
                json!(ORDER),
            )
            .query_parameter(
                "status",
                "Only `active`, `released` or `shipped` rows.",
                json!("active"),
            )
            .response(200, "", body("ReservationLedgerEntryResponse[]", json!(null)))
            .response(
                400,
                "Invalid filter value.",
                error("status must be active, released or shipped"),
            ),
    )
    .operation(
        Operation::post("/inventorys/order-reservations")
            .tag("order-reservations")
            .summary("Reserve stock for an order")
            .description(
                "All-or-nothing bulk reservation, idempotent per `orderId`. Validates the body \
                 (UUIDs, non-empty `items`, positive `quantity`, no duplicate `orderItemId`) → \
                 400. Reserves every line in one transaction, summing demand per SKU in `Long`, \
                 or reserves nothing and returns 409 listing every failed line. An order that \
                 still holds active lines returns its original lines and reserves nothing more \
                 (201); a released order returns 409 rather than reporting stock it no longer \
                 holds. No event is published.",
            )
            .request_body("ReserveOrderRequest", reservation())
            .response(201, "", body("OrderReservationResponse", reservation()))
            .response(
                400,
                "The reservation body failed validation: `orderId` or an `orderItemId` isn't a \
                 UUID, `items` is empty, a line's `quantity` is not positive, or an \
                 `orderItemId` repeats.",
                error("orderId must be a UUID"),
            )
            .response(
                409,
                "The order's reservation was already released or shipped; stock is not held.",
                error("Order reservation already released or shipped"),
            )
            .response(
                409,
                "Lines that couldn't be reserved.",
                body(
                    "ReservationRejectedResponse",
                    json!({
                        "error": "Insufficient stock",
                        "failures": [{"orderItemId": LINE, "sku": SKU, "reason": "insufficient_stock"}],
                    }),
                ),
            ),
    )
    .operation(
        Operation::post("/inventorys/order-reservations/{orderId}/release")
            .tag("order-reservations")
            .summary("Release an order's reservation")
            .description(
                "Returns every active line to `quantityAvailable` and marks its ledger lines \
                 released. Always 204: unknown, already-released or non-UUID `orderId` holds \
                 nothing, so compensation is safe to retry. No event is published.",
            )
            .path_parameter(
                "orderId",
                "Order id; any string is accepted and a non-UUID is a no-op.",
                json!(ORDER),
            )
            .response(204, "", None),
    )
}

fn reconciliation_operations(api: Api) -> Api {
    let only = "Only `resolved` is accepted.";
    api.operation(
        Operation::get("/reconciliations")
            .tag("reconciliations")
            .summary("List reconciliation flags")
            .description(
                "The manual-reconciliation queue, newest first. Both filters are optional and \
                 ANDed; an unknown status → 400 and a malformed orderId returns an empty list. \
                 Unauthenticated local/demo read (ADM-4).",
            )
            .query_parameter("status", "open or resolved (default: both).", json!("open"))
            .query_parameter("orderId", "Only this order's flag.", json!(ID))
            .response(200, "", body("ReconciliationResponse[]", json!([flag()])))
            .response(400, only, error("Invalid reconciliation status: 'open'")),
    )
    .operation(
        Operation::patch("/reconciliations/{orderId}")
            .tag("reconciliations")
            .summary("Resolve a reconciliation flag")
            .description(
                "Marks an open flag resolved, recording the operator id from the token, an \
                 optional note and the timestamp. The first resolver wins: a repeat is an \
                 idempotent 200 and never overwrites the original. 401 without a valid token, \
                 404 when no flag exists.",
            )
            .path_parameter(
                "orderId",
                "Order id (UUID) whose flag to resolve.",
                json!(ID),
            )
            .request_body(
                "ResolveReconciliationRequest",
                json!({"status": "resolved", "note": "Checked stock"}),
            )
            .response(200, "", body("ReconciliationResponse", flag()))
            .response(400, only, error("Invalid reconciliation status: 'open'"))
            .response(
                401,
                "The dev operator token is absent, malformed or does not match.",
                error("Invalid operator token"),
            )
            .response(
                404,
                "No flag for this order.",
                error("Reconciliation flag not found"),
            ),
    )
}

/// The OpenAPI document: stock, reservations, reconciliations, then the two
/// health checks.
#[must_use]
pub fn api() -> Api {
    let api = Api::new("Inventory Service", "1.0")
        .schema::<CreateInventoryRequest>()
        .schema::<ErrorResponse>()
        .schema::<InventoryResponse>()
        .schema::<OrderReservationResponse>()
        .schema::<ReconciliationResponse>()
        .schema::<ReservationFailureResponse>()
        .schema::<ReservationLedgerEntryResponse>()
        .schema::<ReservationLineResponse>()
        .schema::<ReservationRejectedResponse>()
        .schema::<ReserveOrderItemRequest>()
        .schema::<ReserveOrderRequest>()
        .schema::<ResolveReconciliationRequest>()
        .schema::<UpdateInventoryRequest>();
    reconciliation_operations(stock_operations(reservation_operations(api)))
        .operation(
            Operation::get("/health")
                .tag("health")
                .summary("Liveness check")
                .description("200 while the process is up.")
                .response(200, "", None),
        )
        .operation(
            Operation::get("/health/ready")
                .tag("health")
                .summary("Readiness check")
                .description("200 when Postgres answers a ping, else 503.")
                .response(200, "", None)
                .response(503, "Postgres didn't answer a ping.", None),
        )
}

/// The AsyncAPI document: `OrderCancelled` and `DispatchConfirmed`, received.
#[must_use]
pub fn asyncapi() -> Value {
    asyncapi::document(
        "Inventory Service",
        "1.0",
        &[
            Channel::subscribe::<OrderCancelled>(ORDER_EVENTS, ORDER_CANCELLED),
            Channel::subscribe::<DispatchConfirmed>(FULFILMENT_EVENTS, DISPATCH_CONFIRMED),
        ],
    )
}

/// Swagger UI at `/docs/` (`/docs/docs.yaml`), and the AsyncAPI document and page.
pub fn router() -> Router {
    swagger::router(api().to_yaml()).merge(asyncapi::router_with_page(&asyncapi()))
}
