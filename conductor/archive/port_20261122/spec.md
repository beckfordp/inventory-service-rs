# Spec — port inventory-service (gluon all-Rust Stage A, GREEN)

## Overview
The Scala inventory-service in idiomatic Rust on the template's shape: its tables (the
Flyway V1–V8 end state), stock CRUD at `/inventorys`, order reservations (reserve
all-or-nothing and idempotent per order under a per-order advisory lock, release, the
ledger), the OrderCancelled and DispatchConfirmed consumers with their DLQs and
reconciliation flags, reconciliations and operator auth, readiness on Postgres, CORS,
the docs (OpenAPI and AsyncAPI).

## Functional requirements
INV-F1–F18, INV-D1–D2 (tests/contract `docs/behaviours.md`).

## Acceptance criteria
`scripts/ci.sh` green, ≥90% lines; `bin/verify-port inventory-service` all MET.

## Out of scope
Wire rows: metrics (db durations, flag counter, event age), log messages, tracing spans.
