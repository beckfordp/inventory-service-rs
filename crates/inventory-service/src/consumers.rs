// SPDX-License-Identifier: MIT

//! The two consumers (Scala `KafkaConsumers.supervised`): `order-events` and
//! `fulfilment-events`, each supervised with its own DLQ guard, run together
//! until `stop`.

use std::time::Duration;

use kafka_client::KafkaError;
use kafka_client::settings::{ConsumerSettings, SettingsError};
use kafka_client::supervised::{ConsumerTelemetry, Resilience, run_supervised};
use tokio::sync::watch;

use crate::events::{
    FULFILMENT_EVENTS, FULFILMENT_GROUP, FulfilmentEvents, ORDER_EVENTS, ORDER_GROUP, OrderEvents,
};
use crate::store::Inventories;

/// How often the consumers report their lag.
pub const LAG_INTERVAL: Duration = Duration::from_secs(5);

/// One consumer's settings.
///
/// # Errors
/// A topic or group name the client refuses.
pub fn settings(
    bootstrap: &str,
    topic: &str,
    group: &str,
) -> Result<ConsumerSettings, SettingsError> {
    Ok(ConsumerSettings {
        bootstrap: bootstrap.to_owned(),
        group_id: group.parse()?,
        topic: topic.parse()?,
        lag_interval: LAG_INTERVAL,
    })
}

/// Runs both consumers over `store` until `stop` is set.
///
/// # Errors
/// When a consumer can't be built (its settings, or its DLQ producer).
pub async fn run<S: Inventories>(
    bootstrap: &str,
    store: S,
    telemetry: ConsumerTelemetry,
    stop: watch::Receiver<bool>,
) -> Result<(), KafkaError> {
    let created = |error: SettingsError| KafkaError::ClientCreation(error.to_string());
    let orders = settings(bootstrap, ORDER_EVENTS, ORDER_GROUP).map_err(created)?;
    let fulfilment = settings(bootstrap, FULFILMENT_EVENTS, FULFILMENT_GROUP).map_err(created)?;
    let resilience = Resilience::default();
    let logger = telemetry.logger.clone();
    tokio::try_join!(
        run_supervised(
            orders,
            &resilience,
            OrderEvents::new(store.clone(), logger.clone()),
            telemetry.clone(),
            stop.clone(),
        ),
        run_supervised(
            fulfilment,
            &resilience,
            FulfilmentEvents::new(store, logger),
            telemetry,
            stop,
        ),
    )?;
    Ok(())
}
