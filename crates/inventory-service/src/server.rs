// SPDX-License-Identifier: MIT

//! The HTTP server: routes, binding and graceful shutdown. The health routes,
//! tapir's plain-text errors and the HTTP edge (CORS, the Allow-less 405) are
//! gluon-rs `service-basics`.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::Router;
use axum::routing::{get, patch, post};
use service_basics::cors::{CorsPolicy, Unrouted};
use service_basics::edge::edge;
use service_basics::health::{self, Readiness};
use service_basics::text_error::{self, TextError, method_not_allowed};
use telemetry::logging::Logger;
use tokio::net::TcpListener;

use crate::domain::OperatorAuth;
use crate::store::Inventories;
use crate::{docs, routes};

/// How long readiness waits for Postgres to answer.
const READY_PING: Duration = Duration::from_secs(2);

/// What the handlers share.
#[derive(Clone)]
pub struct AppState<S> {
    /// The service's tables.
    pub store: S,
    /// The dev operator token.
    pub auth: OperatorAuth,
    /// The service's JSON log.
    pub logger: Logger,
}

/// Ready when Postgres answers a ping within 2 s.
impl<S: Inventories> Readiness for AppState<S> {
    async fn ready(&self) -> bool {
        self.store.ping(READY_PING).await
    }
}

/// The request headers a preflight may ask for (`Cors.scala`: Authorization
/// for the operator token).
const ALLOWED_HEADERS: &[&str] = &["Content-Type", "Authorization"];

/// The service: its routes behind the Scala services' edge, CORS outermost.
pub fn app<S: Inventories>(state: AppState<S>) -> Router {
    edge(
        routes(state),
        CorsPolicy::default().allowing(ALLOWED_HEADERS),
    )
}

/// Stock, reservations, reconciliations, health and the docs; tapir's 405 for
/// a known path with another method and 404 for any other.
pub fn routes<S: Inventories>(state: AppState<S>) -> Router {
    health::routes()
        .route(
            "/inventorys",
            get(routes::list::<S>).post(routes::create::<S>),
        )
        .route(
            "/inventorys/{id}",
            get(routes::get::<S>)
                .patch(routes::update::<S>)
                .put(routes::update::<S>)
                .delete(routes::delete::<S>),
        )
        .route(
            "/inventorys/order-reservations",
            get(routes::ledger::<S>).post(routes::reserve::<S>),
        )
        .route(
            "/inventorys/order-reservations/{order_id}/release",
            post(routes::release::<S>),
        )
        .route("/reconciliations", get(routes::reconciliations::<S>))
        .route("/reconciliations/{order_id}", patch(routes::resolve::<S>))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(state)
        .merge(docs::router())
}

/// 404 for any unknown path, marked [`Unrouted`] for CORS.
async fn not_found() -> (Unrouted, TextError) {
    (Unrouted, text_error::not_found().await)
}

/// Serves the API on `0.0.0.0:<port>` until `shutdown` completes.
///
/// # Errors
/// When the port can't be bound or serving fails.
pub async fn run<S: Inventories>(
    port: u16,
    state: AppState<S>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await?;
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown)
        .await
}
