# Guard Direct Cancel Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `poly-guard` cancel Polymarket orders directly on heartbeat timeout instead of calling the main daemon's admin `cancel-all` endpoint.

**Architecture:** Keep the heartbeat watchdog and HTTP listener in `poly-notifier/src/guard.rs`, but replace the current admin callback transport with a direct-exchange cancel transport backed by a minimal authenticated Polymarket client. The main LP daemon keeps sending heartbeats exactly as it does today and reconciles any externally canceled orders on its next reconciliation tick.

**Tech Stack:** Rust, tokio, axum, reqwest, pn-polymarket, anyhow, tracing

---

### Task 1: Lock in guard behavior with tests

**Files:**
- Modify: `crates/poly-notifier/src/guard.rs`
- Test: `crates/poly-notifier/src/guard.rs`

**Step 1: Write the failing tests**

Add focused tests for:
- runtime config selecting direct cancel execution metadata from `AppConfig`
- direct cancel triggering once per missed-heartbeat window
- failed direct cancel preserving `cancel_triggered = false` and recording `last_cancel_error`

**Step 2: Run test to verify it fails**

Run: `cargo test -p poly-notifier guard::tests`
Expected: FAIL because guard runtime config and transport still use admin callback fields.

**Step 3: Write minimal implementation**

Replace the admin callback assumptions in `guard.rs` with a generic direct-cancel execution config and adjust tests' fake transport accordingly.

**Step 4: Run test to verify it passes**

Run: `cargo test -p poly-notifier guard::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/poly-notifier/src/guard.rs
git commit -m "feat: switch guard to direct cancel transport"
```

### Task 2: Add a reusable direct-cancel Polymarket client path

**Files:**
- Modify: `crates/pn-polymarket/src/lp.rs`
- Modify: `crates/pn-polymarket/src/lib.rs`

**Step 1: Write the failing test**

Add a small unit test for any pure helper introduced to build the direct-cancel execution config or validate that the direct client reuses LP trading endpoints/chain settings.

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-polymarket lp::tests`
Expected: FAIL because the helper or direct client path does not exist yet.

**Step 3: Write minimal implementation**

Introduce the smallest reusable direct cancel client surface needed by `poly-guard`, reusing existing authentication/env conventions (`POLYMARKET_PRIVATE_KEY`, funder/signature env vars, LP trading config).

**Step 4: Run test to verify it passes**

Run: `cargo test -p pn-polymarket lp::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/pn-polymarket/src/lp.rs crates/pn-polymarket/src/lib.rs
git commit -m "feat: add direct polymarket cancel client"
```

### Task 3: Wire `poly-guard` to the direct client

**Files:**
- Modify: `crates/poly-notifier/src/bin/poly-guard.rs`
- Modify: `crates/poly-notifier/src/guard.rs`

**Step 1: Write the failing test**

Extend a guard runtime test to assert the runtime starts with direct cancel metadata and no longer requires an admin callback URL to execute a timeout cancel.

**Step 2: Run test to verify it fails**

Run: `cargo test -p poly-notifier guard::tests`
Expected: FAIL because the guard binary still builds runtime config around admin callback execution.

**Step 3: Write minimal implementation**

Construct the direct cancel client in `poly-guard`, pass it into `run_guard`, and remove the runtime dependency on `ADMIN_PASSWORD` for guard-triggered cancel execution.

**Step 4: Run test to verify it passes**

Run: `cargo test -p poly-notifier guard::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/poly-notifier/src/bin/poly-guard.rs crates/poly-notifier/src/guard.rs
git commit -m "feat: wire poly-guard to direct exchange cancel"
```

### Task 4: Update operator-facing configuration docs

**Files:**
- Modify: `.env.example`
- Modify: `README.md`

**Step 1: Write the failing doc expectation**

Identify the missing deployment guidance for split-machine guard/main setup and the fact that guard now needs Polymarket trading credentials instead of admin callback access.

**Step 2: Run verification**

Run: `rg -n "GUARD|ADMIN_BASE_URL|POLYMARKET_PRIVATE_KEY|poly-guard" README.md .env.example`
Expected: Missing or stale references to admin callback based guard setup.

**Step 3: Write minimal documentation**

Document:
- main daemon uses `APP__GUARD__BIND_ADDR`/`PORT` only for outbound heartbeat target
- guard daemon needs Polymarket auth env vars
- direct cancel no longer depends on `APP__GUARD__ADMIN_BASE_URL`

**Step 4: Run verification**

Run: `rg -n "GUARD|ADMIN_BASE_URL|POLYMARKET_PRIVATE_KEY|poly-guard" README.md .env.example`
Expected: Updated direct-cancel guidance present.

**Step 5: Commit**

```bash
git add .env.example README.md
git commit -m "docs: update guard deployment configuration"
```

### Task 5: Final verification

**Files:**
- Modify: `crates/poly-notifier/src/guard.rs`
- Modify: `crates/poly-notifier/src/bin/poly-guard.rs`
- Modify: `crates/pn-polymarket/src/lp.rs`
- Modify: `crates/pn-polymarket/src/lib.rs`
- Modify: `.env.example`
- Modify: `README.md`

**Step 1: Run focused tests**

Run: `cargo test -p poly-notifier guard::tests`
Expected: PASS

**Step 2: Run package-level tests for touched crates**

Run: `cargo test -p pn-polymarket`
Run: `cargo test -p poly-notifier`
Expected: PASS

**Step 3: Run formatting**

Run: `cargo fmt --all`
Expected: clean formatting with no diffs after rerun

**Step 4: Re-run critical tests**

Run: `cargo test -p poly-notifier guard::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add docs/plans/2026-03-24-guard-direct-cancel.md
git commit -m "docs: add guard direct cancel implementation plan"
```
