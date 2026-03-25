# LP Audit Logs Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a dedicated audit trail for LP decision, signal, quote, and risk-driven open/close actions so operators can replay why the daemon acted on a given market state.

**Architecture:** Keep the current operational logs intact and add a parallel structured audit path inside `pn-lp::service`. Audit events will be emitted to a dedicated tracing target and persisted through existing `lp_reports` rows, avoiding schema churn while still making events searchable by `report_type` and `audit=true`.

**Tech Stack:** Rust, `tracing`, `serde`, `serde_json`, `sqlx`, SQLite

---

### Task 1: Define audit coverage with failing tests

**Files:**
- Modify: `crates/pn-lp/src/service.rs`
- Test: `crates/pn-lp/src/service.rs`

**Step 1: Write the failing tests**

- Add a test that runs `recompute_quotes` and expects a persisted `decision_audit` report with decision reason, desired quotes, and market state snapshot.
- Add a test that runs `execute_flatten` and expects a persisted `flatten_audit` report with risk reason and flatten intent details.

**Step 2: Run tests to verify they fail**

Run: `cargo test -p pn-lp decision_audit flatten_audit -- --nocapture`

Expected: FAIL because no audit report is persisted yet.

### Task 2: Implement structured audit emission

**Files:**
- Modify: `crates/pn-lp/src/observability.rs`
- Modify: `crates/pn-lp/src/service.rs`

**Step 1: Add audit payload helpers**

- Introduce serializable audit payload structs/helpers for runtime state snapshots and event payload assembly.

**Step 2: Emit audit events from critical paths**

- Persist and log audit events for decision updates, signal transitions, quote submissions/cancellations, and flatten/risk actions.

**Step 3: Keep the implementation minimal**

- Reuse current diagnostics/state instead of duplicating evaluation logic.

### Task 3: Verify

**Files:**
- Modify: `crates/pn-lp/src/service.rs`

**Step 1: Run targeted tests**

Run: `cargo test -p pn-lp audit -- --nocapture`

Expected: PASS

**Step 2: Run focused crate tests**

Run: `cargo test -p pn-lp`

Expected: PASS
