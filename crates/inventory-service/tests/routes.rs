// SPDX-License-Identifier: MIT

//! The routes over the store on a throwaway Postgres: the order of judgement
//! (decode, rules, lookup), the error bodies, reservations, the ledger,
//! reconciliations and the operator token, a store failure a 500, CORS and
//! the docs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use inventory_service::domain::{CANCEL_AFTER_DISPATCH, OperatorAuth};
use inventory_service::server::{AppState, app};
use inventory_service::store::{Inventories, PgInventories};
use serde_json::{Value, json};
use support::{Broken, Postgres};
use telemetry::logging::Logger;
use tower::ServiceExt;
use uuid::Uuid;

const MISSING: &str = "00000000-0000-4000-8000-000000000000";
const ORDER: &str = "00000000-0000-4000-8000-000000000001";
const LINE: &str = "00000000-0000-4000-9000-000000000001";
const TOKEN: &str = "Bearer t0k";

fn service<S: Inventories>(store: S) -> Router {
    app(AppState {
        store,
        auth: OperatorAuth::new("t0k", "ops"),
        logger: Logger::stdout("inventory-service-test"),
    })
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    Answer {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> Answer {
    send(
        app,
        method,
        path,
        body.map(|b| b.to_string()).as_deref(),
        &[],
    )
    .await
}

fn error(reason: &str) -> Value {
    json!({ "error": reason })
}

async fn setup() -> (Postgres, PgInventories, Router) {
    let postgres = Postgres::start();
    let store = support::store(&postgres).await;
    let app = service(store.clone());
    (postgres, store, app)
}

#[tokio::test]
async fn stock_is_judged_decoded_first_then_by_its_rules_then_looked_up() {
    let (_postgres, _, app) = setup().await;
    let created = call(
        &app,
        "POST",
        "/inventorys",
        Some(json!({"sku": "A", "quantityAvailable": 2})),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let stock = created.json();
    assert_eq!(
        (stock["sku"].as_str(), stock["quantityReserved"].as_i64()),
        (Some("A"), Some(0))
    );
    let duplicate = call(
        &app,
        "POST",
        "/inventorys",
        Some(json!({"sku": "A", "quantityAvailable": 1})),
    )
    .await;
    assert_eq!(
        (duplicate.status, duplicate.json()),
        (
            StatusCode::CONFLICT,
            error("Inventory already exists for this sku")
        )
    );
    let blank = call(
        &app,
        "POST",
        "/inventorys",
        Some(json!({"sku": " ", "quantityAvailable": -1})),
    )
    .await;
    assert_eq!(
        (blank.status, blank.json()),
        (StatusCode::BAD_REQUEST, error("sku must not be blank"))
    );
    let undecodable = call(&app, "POST", "/inventorys", Some(json!({"sku": "A"}))).await;
    assert_eq!(undecodable.status, StatusCode::BAD_REQUEST);
    assert!(
        undecodable.body.starts_with("Invalid value for: body"),
        "{}",
        undecodable.body
    );
}

#[tokio::test]
async fn stock_is_read_updated_listed_and_deleted() {
    let (_postgres, _, app) = setup().await;
    let body = Some(json!({"sku": "A", "quantityAvailable": 2}));
    let stock = call(&app, "POST", "/inventorys", body).await.json();
    let id = stock["id"].as_str().unwrap();
    let path = format!("/inventorys/{id}");
    assert_eq!(call(&app, "GET", &path, None).await.json(), stock);
    let missing = call(&app, "GET", &format!("/inventorys/{MISSING}"), None).await;
    assert_eq!(
        (missing.status, missing.json()),
        (StatusCode::NOT_FOUND, error("Inventory not found"))
    );
    let malformed = call(&app, "GET", "/inventorys/nope", None).await;
    assert_eq!(
        (malformed.status, malformed.body.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "Invalid value for: path parameter id"
        )
    );
    for (method, quantity) in [("PATCH", 9), ("PUT", 8)] {
        let updated = call(
            &app,
            method,
            &path,
            Some(json!({"quantityAvailable": quantity})),
        )
        .await;
        assert_eq!(updated.json()["quantityAvailable"], quantity);
    }
    let negative = call(
        &app,
        "PATCH",
        &format!("/inventorys/{MISSING}"),
        Some(json!({"quantityAvailable": -1})),
    )
    .await;
    assert_eq!(
        negative.json(),
        error("quantityAvailable must not be negative"),
        "rules before the lookup"
    );
    let gone = call(
        &app,
        "PATCH",
        &format!("/inventorys/{MISSING}"),
        Some(json!({"quantityAvailable": 1})),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let listed = call(&app, "GET", "/inventorys?sku=A", None).await;
    assert_eq!(listed.json().as_array().unwrap().len(), 1);
    assert_eq!(
        call(&app, "DELETE", &path, None).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "DELETE", &path, None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "GET", "/inventorys", None).await.json(),
        json!([])
    );
}

fn reservation(order: &str, lines: &[(&str, &str, i32)]) -> Value {
    let items: Vec<Value> = lines
        .iter()
        .map(|(item, sku, quantity)| json!({"orderItemId": item, "sku": sku, "quantity": quantity}))
        .collect();
    json!({"orderId": order, "items": items})
}

#[tokio::test]
async fn reservations_release_and_the_ledger() {
    let (_postgres, _, app) = setup().await;
    call(
        &app,
        "POST",
        "/inventorys",
        Some(json!({"sku": "A", "quantityAvailable": 3})),
    )
    .await;
    let r = "/inventorys/order-reservations";
    let short = call(&app, "POST", r, Some(reservation(ORDER, &[(LINE, "A", 4)]))).await;
    assert_eq!(short.status, StatusCode::CONFLICT);
    assert_eq!(
        short.json(),
        json!({"error": "Insufficient stock", "failures": [{"orderItemId": LINE, "sku": "A", "reason": "insufficient_stock"}]})
    );
    let body = reservation(ORDER, &[(LINE, "A", 2)]);
    let made = call(&app, "POST", r, Some(body.clone())).await;
    assert_eq!(
        (made.status, made.json()),
        (StatusCode::CREATED, body.clone())
    );
    let invalid = call(&app, "POST", r, Some(reservation("x", &[]))).await;
    assert_eq!(
        (invalid.status, invalid.json()),
        (StatusCode::BAD_REQUEST, error("orderId must be a UUID"))
    );
    assert_eq!(
        call(&app, "POST", r, Some(json!({"orderId": ORDER})))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    let ledger = call(
        &app,
        "GET",
        &format!("{r}?orderId={ORDER}&status=active"),
        None,
    )
    .await
    .json();
    assert_eq!(ledger[0]["lineNo"], 0);
    assert_eq!(ledger[0]["releasedAt"], Value::Null);
    let bad = call(&app, "GET", &format!("{r}?status=gone"), None).await;
    assert_eq!(
        (bad.status, bad.json()),
        (
            StatusCode::BAD_REQUEST,
            error("status must be active, released or shipped")
        )
    );
    let bad = call(&app, "GET", &format!("{r}?orderId=nope"), None).await;
    assert_eq!(bad.json(), error("orderId must be a UUID"));

    for order in [ORDER, ORDER, MISSING, "nope"] {
        let released = call(&app, "POST", &format!("{r}/{order}/release"), None).await;
        assert_eq!(released.status, StatusCode::NO_CONTENT, "{order}");
    }
    let refused = call(&app, "POST", r, Some(body)).await;
    assert_eq!(
        (refused.status, refused.json()),
        (
            StatusCode::CONFLICT,
            error("Order reservation already released or shipped")
        )
    );
    let released = call(&app, "GET", &format!("{r}?status=released"), None)
        .await
        .json();
    assert!(released[0]["releasedAt"].is_string());
}

/// The app over a fresh Postgres, with `ORDER` flagged `cancel_after_dispatch`.
async fn flagged() -> (Postgres, Router) {
    let (postgres, store, app) = setup().await;
    call(
        &app,
        "POST",
        "/inventorys",
        Some(json!({"sku": "A", "quantityAvailable": 3})),
    )
    .await;
    call(
        &app,
        "POST",
        "/inventorys/order-reservations",
        Some(reservation(ORDER, &[(LINE, "A", 1)])),
    )
    .await;
    store
        .flag(Uuid::parse_str(ORDER).unwrap(), CANCEL_AFTER_DISPATCH)
        .await
        .unwrap();
    (postgres, app)
}

#[tokio::test]
async fn reconciliations_are_listed_by_status_and_order() {
    let (_postgres, app) = flagged().await;

    let listed = call(&app, "GET", "/reconciliations?status=%20open%20", None)
        .await
        .json();
    assert_eq!(
        (listed[0]["orderId"].as_str(), listed[0]["reason"].as_str()),
        (Some(ORDER), Some(CANCEL_AFTER_DISPATCH))
    );
    assert_eq!(listed[0]["operatorId"], Value::Null);
    let only = "Invalid reconciliation status: only 'resolved' is accepted";
    let bad = call(&app, "GET", "/reconciliations?status=closed", None).await;
    assert_eq!(
        (bad.status, bad.json()),
        (StatusCode::BAD_REQUEST, error(only))
    );
    assert_eq!(
        call(&app, "GET", "/reconciliations?orderId=nope", None)
            .await
            .json(),
        json!([])
    );
    assert_eq!(
        call(
            &app,
            "GET",
            &format!("/reconciliations?orderId={ORDER}&status="),
            None
        )
        .await
        .json()
        .as_array()
        .unwrap()
        .len(),
        1
    );
}

#[tokio::test]
async fn a_flag_is_resolved_by_an_operator_and_the_first_wins() {
    let (_postgres, app) = flagged().await;
    let only = "Invalid reconciliation status: only 'resolved' is accepted";
    let path = format!("/reconciliations/{ORDER}");
    let resolved = r#"{"status":"resolved","note":"n"}"#;
    for headers in [&[][..], &[("authorization", "Bearer wrong")][..]] {
        let refused = send(&app, "PATCH", &path, Some(resolved), headers).await;
        assert_eq!(
            (refused.status, refused.json()),
            (StatusCode::UNAUTHORIZED, error("Invalid operator token"))
        );
    }
    let auth = [("authorization", TOKEN)];
    let open = send(&app, "PATCH", &path, Some(r#"{"status":"open"}"#), &auth).await;
    assert_eq!(
        (open.status, open.json()),
        (StatusCode::BAD_REQUEST, error(only))
    );
    let missing = send(
        &app,
        "PATCH",
        &format!("/reconciliations/{MISSING}"),
        Some(resolved),
        &auth,
    )
    .await;
    assert_eq!(
        (missing.status, missing.json()),
        (
            StatusCode::NOT_FOUND,
            error("Reconciliation flag not found")
        )
    );
    let malformed = send(
        &app,
        "PATCH",
        "/reconciliations/nope",
        Some(resolved),
        &auth,
    )
    .await;
    assert_eq!(
        (malformed.status, malformed.body.as_str()),
        (
            StatusCode::BAD_REQUEST,
            "Invalid value for: path parameter orderId"
        )
    );
    let undecodable = send(&app, "PATCH", &path, Some("{}"), &auth).await;
    assert_eq!(undecodable.status, StatusCode::BAD_REQUEST);
    let first = send(&app, "PATCH", &path, Some(resolved), &auth)
        .await
        .json();
    assert_eq!(
        (
            first["status"].as_str(),
            first["operatorId"].as_str(),
            first["note"].as_str()
        ),
        (Some("resolved"), Some("ops"), Some("n"))
    );
    let again = send(
        &app,
        "PATCH",
        &path,
        Some(r#"{"status":" resolved ","note":"x"}"#),
        &auth,
    )
    .await;
    assert_eq!((again.status, again.json()), (StatusCode::OK, first));
}

#[tokio::test]
async fn a_store_failure_is_a_500() {
    let app = service(Broken);
    let order = reservation(ORDER, &[(LINE, "A", 1)]);
    for (method, path, body) in [
        (
            "POST",
            "/inventorys".to_owned(),
            Some(json!({"sku": "A", "quantityAvailable": 1})),
        ),
        ("GET", format!("/inventorys/{MISSING}"), None),
        (
            "PATCH",
            format!("/inventorys/{MISSING}"),
            Some(json!({"quantityAvailable": 1})),
        ),
        ("DELETE", format!("/inventorys/{MISSING}"), None),
        ("GET", "/inventorys".to_owned(), None),
        (
            "POST",
            "/inventorys/order-reservations".to_owned(),
            Some(order),
        ),
        ("GET", "/inventorys/order-reservations".to_owned(), None),
        (
            "POST",
            format!("/inventorys/order-reservations/{ORDER}/release"),
            None,
        ),
        ("GET", "/reconciliations".to_owned(), None),
    ] {
        let answer = call(&app, method, &path, body).await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
            "{method} {path}"
        );
    }
    let resolve = send(
        &app,
        "PATCH",
        &format!("/reconciliations/{ORDER}"),
        Some(r#"{"status":"resolved"}"#),
        &[("authorization", TOKEN)],
    )
    .await;
    assert_eq!(resolve.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        call(&app, "GET", "/health/ready", None).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn the_edge_allows_the_operator_token_and_serves_the_docs() {
    let app = service(Broken);
    let preflight = send(
        &app,
        "OPTIONS",
        "/inventorys",
        None,
        &[
            ("origin", "http://localhost:5173"),
            ("access-control-request-method", "PATCH"),
            (
                "access-control-request-headers",
                "content-type, authorization",
            ),
        ],
    )
    .await;
    assert_eq!(preflight.status, StatusCode::OK);
    let allowed = preflight.headers["access-control-allow-headers"]
        .to_str()
        .unwrap()
        .to_lowercase();
    assert!(allowed.contains("authorization"), "{allowed}");
    assert_eq!(
        call(&app, "GET", "/nope", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", "/inventorys", None).await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );

    let yaml = call(&app, "GET", "/docs/docs.yaml", None).await;
    assert_eq!(yaml.status, StatusCode::OK);
    for path in [
        "/inventorys/order-reservations/{orderId}/release",
        "/reconciliations/{orderId}",
        "/health/ready",
    ] {
        assert!(yaml.body.contains(path), "{path}");
    }
    let asyncapi = call(&app, "GET", "/docs/asyncapi.json", None).await.json();
    let order_events =
        &asyncapi["channels"]["order-events"]["messages"]["OrderCancelled"]["payload"];
    assert_eq!(
        order_events["required"],
        json!(["orderId", "customerId", "timestamp"])
    );
    let dispatch =
        &asyncapi["channels"]["fulfilment-events"]["messages"]["DispatchConfirmed"]["payload"];
    assert_eq!(dispatch["required"], json!(["orderId", "trackingNo"]));
    assert_eq!(asyncapi["operations"]["order-events"]["action"], "receive");
    let page = call(&app, "GET", "/docs/asyncapi/", None).await;
    assert!(page.body.contains("fulfilment-events"));
}
