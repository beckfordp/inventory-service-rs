// SPDX-License-Identifier: MIT

//! Shared by the integration tests: gluon-rs `service-testing`'s throwaway
//! PostgreSQL cluster, this service's store over it and its binary under
//! test, a store that always fails, and a logger whose lines the
//! test can read back.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unused_async_trait_impl
)]

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use inventory_service::config::{PORT_VARIABLE, PostgresConfig};
use inventory_service::db;
use inventory_service::domain::{
    Inventory, InventoryId, LedgerEntry, LineStatus, NewStock, Reconciliation,
    ReconciliationStatus, ReleaseOutcome, ReservationConflict, ReservationLine, ReserveOrder,
    ResolveOutcome, ShipOutcome,
};
use inventory_service::store::{DuplicateSku, Inventories, PgInventories, StoreError};
use serde_json::Value;
use telemetry::logging::Logger;
use time::OffsetDateTime;
use uuid::Uuid;

// Not every test file uses both.
#[allow(unused_imports)]
pub use service_testing::{Postgres, free_port};

/// The inventory-service binary, on a fresh port.
pub struct Service(service_testing::Service);

impl Service {
    /// Starts it with exactly `extra` (plus its port); does not wait.
    pub fn spawn(extra: &[(&str, String)]) -> Self {
        Self(service_testing::Service::spawn(
            env!("CARGO_BIN_EXE_inventory-service"),
            PORT_VARIABLE,
            extra,
        ))
    }

    /// Starts it and waits until `/health` answers.
    pub fn start(extra: &[(&str, String)]) -> Self {
        Self(service_testing::Service::start(
            env!("CARGO_BIN_EXE_inventory-service"),
            PORT_VARIABLE,
            extra,
        ))
    }
}

impl Deref for Service {
    type Target = service_testing::Service;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Service {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// The service's store over `postgres`, migrated as the service migrates it.
pub async fn store(postgres: &Postgres) -> PgInventories {
    let env: HashMap<_, _> = postgres.env().into_iter().collect();
    let config = PostgresConfig {
        host: env["POSTGRES_HOST"].clone(),
        port: env["POSTGRES_PORT"].parse().unwrap(),
        database: env["POSTGRES_DB"].clone(),
        user: env["POSTGRES_USER"].clone(),
        password: env["POSTGRES_PASSWORD"].clone(),
    };
    let pool = db::connect(&config).await.unwrap();
    db::migrate(&pool).await.unwrap();
    PgInventories::new(pool)
}

/// A store whose every call fails as a database would, and whose ping fails.
#[derive(Clone, Default)]
pub struct Broken;

fn broken<T>() -> Result<T, StoreError> {
    Err(StoreError::Database(sqlx::Error::PoolClosed))
}

impl Inventories for Broken {
    async fn create(
        &self,
        _: InventoryId,
        _: &NewStock,
    ) -> Result<Result<Inventory, DuplicateSku>, StoreError> {
        broken()
    }
    async fn get(&self, _: InventoryId) -> Result<Option<Inventory>, StoreError> {
        broken()
    }
    async fn update(&self, _: InventoryId, _: i32) -> Result<Option<Inventory>, StoreError> {
        broken()
    }
    async fn delete(&self, _: InventoryId) -> Result<bool, StoreError> {
        broken()
    }
    async fn list(&self, _: Option<&str>) -> Result<Vec<Inventory>, StoreError> {
        broken()
    }
    async fn reserve(
        &self,
        _: &ReserveOrder,
    ) -> Result<Result<Vec<ReservationLine>, ReservationConflict>, StoreError> {
        broken()
    }
    async fn release(&self, _: Uuid) -> Result<ReleaseOutcome, StoreError> {
        broken()
    }
    async fn ship(&self, _: Uuid) -> Result<ShipOutcome, StoreError> {
        broken()
    }
    async fn flag(&self, _: Uuid, _: &str) -> Result<bool, StoreError> {
        broken()
    }
    async fn reconciliations(
        &self,
        _: Option<ReconciliationStatus>,
        _: Option<Uuid>,
    ) -> Result<Vec<Reconciliation>, StoreError> {
        broken()
    }
    async fn resolve(
        &self,
        _: Uuid,
        _: &str,
        _: Option<&str>,
        _: OffsetDateTime,
    ) -> Result<ResolveOutcome, StoreError> {
        broken()
    }
    async fn ledger(
        &self,
        _: Option<Uuid>,
        _: Option<LineStatus>,
    ) -> Result<Vec<LedgerEntry>, StoreError> {
        broken()
    }
    async fn ping(&self, _: Duration) -> bool {
        false
    }
}

/// Where a [`captured_logger`] writes.
pub type LogBuffer = Arc<Mutex<Vec<u8>>>;

/// A logger named as the service's, writing to a buffer the test reads back.
pub fn captured_logger() -> (Logger, LogBuffer) {
    let buffer = LogBuffer::default();
    (
        Logger::to_writer("inventory-service", buffer.clone()),
        buffer,
    )
}

/// The lines logged to `buffer` so far, each as JSON.
pub fn log_lines(buffer: &LogBuffer) -> Vec<Value> {
    let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
