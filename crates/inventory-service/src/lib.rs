// SPDX-License-Identifier: MIT

//! The inventory-service binary: the Rust port of gluon's Scala
//! inventory-service (ADR 0029): stock, order reservations, the consumers of
//! `OrderCancelled` and `DispatchConfirmed`, and reconciliation flags.
pub mod config;
pub mod consumers;
pub mod db;
pub mod docs;
pub mod domain;
pub mod events;
pub mod lifecycle;
pub mod routes;
pub mod server;
pub mod store;

/// The package version, as built.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
