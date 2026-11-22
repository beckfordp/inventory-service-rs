# Plan — port inventory-service (gluon all-Rust Stage A, GREEN)

Spec: [spec.md](./spec.md).

## Phase 1: Port
- [ ] Task 1.1: schema (the V1–V8 end state) and domain (stock rules, reservation validation, failures, reconciliation status, operator auth) — tests first
- [ ] Task 1.2: Postgres store (the Scala SQL, advisory lock, sku-ordered shifts) — tests first (throwaway Postgres)
- [ ] Task 1.3: routes and errors; docs — tests first (in-memory store)
- [ ] Task 1.4: consumers (OrderCancelled, DispatchConfirmed, DLQ) — tests first
- [ ] Task: Conductor - User Manual Verification 'Phase 1' (Protocol in workflow.md): gluon `bin/verify-port inventory-service` all MET; CI ≥90% lines
