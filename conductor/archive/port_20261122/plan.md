# Plan — port inventory-service (gluon all-Rust Stage A, GREEN)

Spec: [spec.md](./spec.md).

## Phase 1: Port
- [x] Task 1.1: schema (the V1–V8 end state) and domain (stock rules, reservation validation, failures, reconciliation status, operator auth) — tests first `8f8508f`
- [x] Task 1.2: Postgres store (the Scala SQL, advisory lock, sku-ordered shifts) — tests first (throwaway Postgres) `8f8508f`
- [x] Task 1.3: routes and errors; docs — tests first (in-memory store) `8f8508f`
- [x] Task 1.4: consumers (OrderCancelled, DispatchConfirmed, DLQ) — tests first `8f8508f`
- [x] Task: Conductor - User Manual Verification 'Phase 1' (Protocol in workflow.md): gluon `bin/verify-port inventory-service` all MET (evidence `docs/ports/evidence/inventory-service/green.txt`); contract 33/33 on the first run; CI 98.40% lines

## Archive (2026-11-23)

Reviewed: no High/Medium. Deliberate differences: a UUID is Java's canonical
8-4-4-4-12 form only (Java's `UUID.fromString` also takes short hex groups); a
null-valued record is skipped, not dead-lettered (gluon-rs `kafka-event-consumer`, as
notification-service-rs; Scala inventory dead-letters it; not sendable by the
captures' console producer); the resolve operation's optional `Authorization` header
parameter isn't in the OpenAPI document (the builder has no header parameters). The
template's `IdGen`/`Clock` dropped: Postgres stamps the times, as in Scala.
