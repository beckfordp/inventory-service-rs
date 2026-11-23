// SPDX-License-Identifier: MIT

//! The consumers' routes, as the Scala `OrderCancelledConsumer` and
//! `FulfilmentEventConsumer`: `OrderCancelled` releases the order's stock, and
//! flags it `cancel_after_dispatch` if it had already shipped; `DispatchConfirmed`
//! ships it, and flags it `dispatch_after_release` if it had been released.
//! Every other event type is skipped. The rest of the per-record decision
//! (undecodable envelopes and payloads to the DLQ, the DLQ guard) is gluon-rs
//! `kafka-event-consumer`'s `handle_record`; a payload whose orderId isn't a
//! UUID doesn't decode, so it is dead-lettered too.

use kafka_event_consumer::envelope::{Envelope, Routes};
use schemars::JsonSchema;
use serde::Deserialize;
use telemetry::logging::Logger;
use uuid::Uuid;

use crate::domain::{CANCEL_AFTER_DISPATCH, DISPATCH_AFTER_RELEASE, parse_uuid};
use crate::store::{Inventories, StoreError};

/// The topic `OrderCancelled` comes on.
pub const ORDER_EVENTS: &str = "order-events";
/// Its consumer group.
pub const ORDER_GROUP: &str = "inventory-service-order-events";
/// The topic `DispatchConfirmed` comes on.
pub const FULFILMENT_EVENTS: &str = "fulfilment-events";
/// Its consumer group.
pub const FULFILMENT_GROUP: &str = "inventory-service-fulfilment-events";

/// The event type routed on `order-events`.
pub const ORDER_CANCELLED: &str = "OrderCancelled";
/// The event type routed on `fulfilment-events`.
pub const DISPATCH_CONFIRMED: &str = "DispatchConfirmed";

/// Order cancelled: a customer cancel or a consumed `PaymentAuthorizationExpired`
/// moved the order to cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "OrderCancelledEvent")]
pub struct OrderCancelled {
    /// Order UUID (order-service's order id).
    pub order_id: String,
    /// Customer who placed the order.
    pub customer_id: String,
    /// When the order was cancelled (ISO-8601 instant).
    pub timestamp: String,
    /// Cancellation trigger: customer | `auth_expired` | stale | operator.
    pub reason: Option<String>,
}

/// Order dispatched: pick/pack/dispatch succeeded. Inventory marks the order's
/// active reservation lines shipped.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename = "DispatchConfirmedEvent")]
pub struct DispatchConfirmed {
    /// Order UUID (order-service's order id).
    pub order_id: String,
    /// Carrier tracking number.
    pub tracking_no: String,
}

/// Why an event wasn't handled: logged, retried and dead-lettered by the DLQ guard.
#[derive(Debug, thiserror::Error)]
pub enum EventsError {
    /// The payload isn't the event's (poison).
    #[error(transparent)]
    Payload(#[from] serde_json::Error),
    /// The payload's orderId isn't a UUID (poison).
    #[error("Invalid orderId in {0} payload: {1}")]
    OrderId(&'static str, String),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn decoded<T: serde::de::DeserializeOwned>(
    envelope: &Envelope,
    event_type: &'static str,
    order_id: impl Fn(&T) -> &str,
) -> Result<(Uuid, T), EventsError> {
    let event: T = serde_json::from_value(envelope.payload.clone())?;
    let raw = order_id(&event);
    let id = parse_uuid(raw).ok_or_else(|| EventsError::OrderId(event_type, raw.to_owned()))?;
    Ok((id, event))
}

/// `order-events`: `OrderCancelled` → release.
#[derive(Debug, Clone)]
pub struct OrderEvents<S> {
    store: S,
    logger: Logger,
}

impl<S> OrderEvents<S> {
    /// Over `store`, logging to `logger`.
    #[must_use]
    pub fn new(store: S, logger: Logger) -> Self {
        Self { store, logger }
    }
}

impl<S: Inventories> Routes for OrderEvents<S> {
    type Event = (Uuid, OrderCancelled);
    type Error = EventsError;

    fn decode(&self, envelope: &Envelope) -> Option<Result<Self::Event, EventsError>> {
        (envelope.event_type == ORDER_CANCELLED).then(|| {
            decoded(envelope, ORDER_CANCELLED, |e: &OrderCancelled| {
                e.order_id.as_str()
            })
        })
    }

    async fn handle(
        &self,
        _event_id: &str,
        (order_id, event): &Self::Event,
    ) -> Result<(), EventsError> {
        let outcome = self.store.release(*order_id).await?;
        let after_dispatch = !outcome.shipped.is_empty();
        if after_dispatch {
            self.store.flag(*order_id, CANCEL_AFTER_DISPATCH).await?;
        }
        self.logger.info(
            &[
                ("order_id", &event.order_id),
                ("reason", event.reason.as_deref().unwrap_or("-")),
                ("released_lines", &outcome.released.len().to_string()),
                ("shipped_lines", &outcome.shipped.len().to_string()),
            ],
            if after_dispatch {
                "OrderCancelled after dispatch - reconciliation flagged"
            } else {
                "Reservation released on order cancellation"
            },
        );
        Ok(())
    }
}

/// `fulfilment-events`: `DispatchConfirmed` → ship.
#[derive(Debug, Clone)]
pub struct FulfilmentEvents<S> {
    store: S,
    logger: Logger,
}

impl<S> FulfilmentEvents<S> {
    /// Over `store`, logging to `logger`.
    #[must_use]
    pub fn new(store: S, logger: Logger) -> Self {
        Self { store, logger }
    }
}

impl<S: Inventories> Routes for FulfilmentEvents<S> {
    type Event = (Uuid, DispatchConfirmed);
    type Error = EventsError;

    fn decode(&self, envelope: &Envelope) -> Option<Result<Self::Event, EventsError>> {
        (envelope.event_type == DISPATCH_CONFIRMED).then(|| {
            decoded(envelope, DISPATCH_CONFIRMED, |e: &DispatchConfirmed| {
                e.order_id.as_str()
            })
        })
    }

    async fn handle(
        &self,
        _event_id: &str,
        (order_id, event): &Self::Event,
    ) -> Result<(), EventsError> {
        let outcome = self.store.ship(*order_id).await?;
        if !outcome.released.is_empty() {
            self.store.flag(*order_id, DISPATCH_AFTER_RELEASE).await?;
        }
        let context = [
            ("order_id", event.order_id.clone()),
            ("tracking_no", event.tracking_no.clone()),
            ("shipped_lines", outcome.shipped.len().to_string()),
            (
                "already_shipped_lines",
                outcome.already_shipped.len().to_string(),
            ),
            ("released_lines", outcome.released.len().to_string()),
        ];
        let fields: Vec<(&str, &str)> = context.iter().map(|(k, v)| (*k, v.as_str())).collect();
        if outcome.released.is_empty()
            && outcome.shipped.is_empty()
            && outcome.already_shipped.is_empty()
        {
            self.logger.warn(
                &fields,
                "DispatchConfirmed for an unknown reservation - skipping",
            );
        } else if !outcome.released.is_empty() {
            self.logger.warn(
                &fields,
                "DispatchConfirmed after release - reconciliation flagged",
            );
        } else if outcome.shipped.is_empty() {
            self.logger.info(
                &fields,
                "DispatchConfirmed for an already-shipped reservation - skipping duplicate",
            );
        } else {
            self.logger.info(&fields, "Reservation marked shipped");
        }
        Ok(())
    }
}
