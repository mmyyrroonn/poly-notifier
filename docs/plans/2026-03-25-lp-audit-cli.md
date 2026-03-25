# LP Audit CLI Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a CLI that replays LP audit events by market so operators can inspect decisions, signals, quote changes, and flatten actions from persisted audit logs.

**Architecture:** Reuse the existing `lp_reports` persistence path and add a lightweight reader binary under `poly-notifier`. The CLI will load the configured SQLite database, fetch recent reports for a condition, filter to audit report types, and render either a readable timeline or raw JSON.

**Tech Stack:** Rust, `pn-common`, `sqlx`, `serde_json`, SQLite

---

### Task 1: Write failing tests

**Files:**
- Modify: `crates/pn-common/src/db.rs`
- Create: `crates/poly-notifier/src/bin/poly-lp-audit.rs`

**Step 1: Add tests for report filtering and rendering**

- Verify the CLI argument parser accepts `--condition-id`, `--type`, `--json`, `--limit`, and config/db overrides.
- Verify report selection keeps only matching audit report types and renders a readable timeline entry.

**Step 2: Run tests to confirm they fail**

Run: `cargo test -p poly-lp audit`

Expected: FAIL because the new CLI and helper functions do not exist yet.

### Task 2: Implement the CLI

**Files:**
- Modify: `crates/pn-common/src/db.rs`
- Modify: `crates/poly-notifier/Cargo.toml`
- Create: `crates/poly-notifier/src/bin/poly-lp-audit.rs`

**Step 1: Add a DB helper**

- Add a query helper that loads recent `lp_reports` rows for a `condition_id`.

**Step 2: Add the new binary**

- Parse CLI args.
- Load database URL from config unless `--db` overrides it.
- Fetch reports, filter audit types, and print timeline or JSON.

### Task 3: Verify and commit

**Files:**
- Modify: `crates/pn-common/src/db.rs`
- Modify: `crates/poly-notifier/Cargo.toml`
- Create: `crates/poly-notifier/src/bin/poly-lp-audit.rs`

**Step 1: Run focused verification**

Run: `cargo test -p poly-lp`

Expected: PASS

**Step 2: Commit**

Run: `git add ... && git commit -m "feat(lp): add audit timeline cli"`
