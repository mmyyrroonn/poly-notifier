# LP Daemon Review Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix the production-safety issues identified for commit `0baba9a` and verify the LP daemon still compiles, lints, and passes tests.

**Architecture:** Apply the fixes in severity order. For behavior changes, add or update focused regression tests first, run them to see the expected failure, then implement the smallest change needed to pass. Use `cargo check` after each fix batch, then finish with `cargo clippy` and `cargo test`.

**Tech Stack:** Rust, Tokio, Axum, SQLx, rust_decimal, Polymarket SDK

---

### Task 1: Lock Down the Critical Runtime and Risk Fixes

**Files:**
- Modify: `crates/pn-lp/src/service.rs`
- Modify: `crates/pn-lp/src/risk.rs`
- Modify: `crates/pn-lp/src/types.rs`
- Test: `crates/pn-lp/src/risk.rs`
- Test: `crates/pn-lp/src/service.rs` (if practical)

**Step 1: Write the failing test**

- Update `RiskEngine::on_fill` tests so flatten actions use the current net position size from `RuntimeState`, not the individual fill size.
- Add a regression test for any new helper introduced for flatten price derivation or fill storage behavior if that logic is made testable.

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-lp risk::tests::fill_triggers_cancel_pause_and_flatten`

Expected: FAIL because `on_fill` does not accept `RuntimeState` and still uses `fill.size`.

**Step 3: Write minimal implementation**

- Change the main `tokio::select!` loop in `LpService::run()` so transient handler errors are logged and the daemon keeps running.
- Change `RiskEngine::on_fill` to take `&RuntimeState` and flatten using the net position size.
- Remove or cap unbounded `state.fills` accumulation. Prefer removing `fills` from `RuntimeState` if nothing reads it.
- In `execute_flatten`, clear `state.flags.flattening` in `Err(...)` and `Ok(None)`.
- Add price context to `FlattenIntent` or otherwise derive a best-effort price before flattening so flatten acknowledgements do not fabricate `Decimal::ZERO`.

**Step 4: Run targeted verification**

Run: `cargo test -p pn-lp`
Run: `cargo check -p pn-lp`

Expected: the new regression tests pass and the crate still compiles.

### Task 2: Fix Startup Ordering and Config Safety

**Files:**
- Modify: `crates/poly-notifier/src/main.rs`
- Modify: `crates/pn-common/src/config.rs`
- Test: `crates/poly-notifier/src/main.rs` (only if there are practical unit tests)
- Test: `crates/pn-common/src/config.rs` or existing config test module if added

**Step 1: Write the failing test**

- Add a focused regression test for config decimal round-tripping if the `f64` fields stay in place.

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-common`

Expected: FAIL if a new precision regression test is added first.

**Step 3: Write minimal implementation**

- Move startup auto-split handling out of `main.rs` race-prone pre-spawn code. Prefer executing it inside `LpService::run()` before the first quote recomputation.
- Require `ADMIN_PASSWORD` with `std::env::var(...).expect(...)`.
- Either convert financial config values from `f64` to string-backed parsing, or keep `f64` and add explicit documentation/tests that cover expected precision.

**Step 4: Run targeted verification**

Run: `cargo check -p poly-notifier -p pn-common`

Expected: startup wiring still compiles and config parsing behavior is covered.

### Task 3: Fix Exchange Adapter and Admin Handler Robustness

**Files:**
- Modify: `crates/pn-polymarket/src/lp.rs`
- Modify: `crates/pn-admin/src/handlers.rs`
- Test: `crates/pn-polymarket/src/lp.rs`
- Test: `crates/pn-admin/src/handlers.rs`

**Step 1: Write the failing test**

- Add tests for flatten response mapping and any helper used for reconnect backoff if extracted.
- Add handler tests for invalid split/merge amounts returning `400`.
- Add coverage for any helper replacing `.map(...).flatten()`.

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-polymarket`
Run: `cargo test -p pn-admin`

Expected: FAIL before implementation for the new validation / mapping behaviors.

**Step 3: Write minimal implementation**

- Use request price and requested size with a clear status/comment for unconfirmed flatten fills.
- Add exponential reconnect backoff to all four websocket loops.
- Make `side_from_sdk` exhaustive.
- Replace `.map(...).flatten()` with `.and_then(...)`.
- Validate split and merge amounts before enqueuing control commands.

**Step 4: Run targeted verification**

Run: `cargo check -p pn-polymarket -p pn-admin`

Expected: both crates compile and targeted tests pass.

### Task 4: Fix Reconciliation and Final Verification

**Files:**
- Modify: `crates/pn-lp/src/service.rs`
- Test: `crates/pn-lp/src/service.rs` (if practical)

**Step 1: Write the failing test**

- Add a regression test for reconciliation status syncing if a small pure helper can be extracted.

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-lp`

Expected: FAIL before the helper or reconciliation update is implemented.

**Step 3: Write minimal implementation**

- In `handle_reconciliation_tick`, compare prior open orders against the fresh exchange snapshot and mark missing orders with their reconciled terminal status before replacing `state.open_orders`.

**Step 4: Run full verification**

Run: `cargo check`
Run: `cargo clippy --all-targets --all-features -- -D warnings`
Run: `cargo test`

Expected: all commands succeed.
