# Reward-Aware Low-Fill LP Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make the LP daemon quote only when a manually selected market has active Polymarket rewards, place a single low-fill qualifying order on the safest side, and cancel it as soon as reward eligibility or inside protection degrades.

**Architecture:** Add a reward fetch layer in `pn-polymarket` that reads market reward configuration from the CLOB rewards endpoint with retry support. Thread that reward state into `pn-lp` runtime state, then extend the decision engine so quote generation is reward-qualified, single-sided, and driven by inside-book protection rather than the current generic join/inside/outside behavior alone. Refresh rewards periodically in the service loop without breaking the existing LP event flow, and treat stale or missing rewards as “do not quote”.

**Tech Stack:** Rust, Tokio, reqwest, rust_decimal, serde, Polymarket CLOB/Gamma APIs

---

### Task 1: Add Reward API Types and Client Coverage

**Files:**
- Modify: `crates/pn-polymarket/src/lib.rs`
- Modify: `crates/pn-polymarket/src/types.rs`
- Create: `crates/pn-polymarket/src/rewards.rs`
- Test: `crates/pn-polymarket/src/rewards.rs`

**Step 1: Write the failing test**

- Add focused parsing tests for the raw rewards payload returned by `GET /rewards/markets/{condition_id}`.
- Cover active-vs-expired config filtering, daily-rate aggregation, and token price extraction.
- Add a retry helper test that proves transient failures are retried before success.

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-polymarket rewards`

Expected: FAIL because `rewards.rs` and the reward parsing/retry helpers do not exist yet.

**Step 3: Write minimal implementation**

- Create reward wire/domain structs for:
  - `rewards_max_spread`
  - `rewards_min_size`
  - `tokens[].token_id / outcome / price`
  - `rewards_config[].start_date / end_date / rate_per_day`
- Implement a small reward client using `reqwest` against `https://clob.polymarket.com/rewards/markets/{condition_id}`.
- Add retry support with bounded attempts and backoff.
- Normalize the response into a domain snapshot that only exposes the active reward view needed by the LP daemon.

**Step 4: Run targeted verification**

Run: `cargo test -p pn-polymarket rewards`

Expected: PASS with reward parsing and retry behavior covered.

### Task 2: Thread Reward State Through Config and Runtime

**Files:**
- Modify: `crates/pn-common/src/config.rs`
- Modify: `config/default.toml`
- Modify: `crates/pn-polymarket/src/lp.rs`
- Modify: `crates/pn-lp/src/types.rs`
- Modify: `crates/pn-lp/src/service.rs`
- Modify: `crates/poly-notifier/src/main.rs`
- Test: `crates/poly-notifier/src/main.rs`
- Test: `crates/pn-lp/src/service.rs`

**Step 1: Write the failing test**

- Add config/build tests for the new reward-related strategy parameters.
- Add service/runtime tests that prove reward state can be absent, active, stale, or expired without crashing the loop.

**Step 2: Run test to verify it fails**

Run: `cargo test -p poly-notifier build_service_config`
Run: `cargo test -p pn-lp service`

Expected: FAIL because reward settings and runtime reward state do not exist yet.

**Step 3: Write minimal implementation**

- Add config for:
  - reward refresh interval
  - reward stale timeout
  - reward fetch retries/backoff
  - inside protection thresholds
- Extend the execution client with a reward fetch method using the new reward client.
- Extend LP runtime state with a reward snapshot and last-refresh timestamps.
- Refresh reward state on startup and on a periodic service timer; if reward refresh fails, keep the previous value until the stale timeout is exceeded.
- Preserve compatibility with the existing startup/bootstrap/reconcile flow.

**Step 4: Run targeted verification**

Run: `cargo test -p poly-notifier build_service_config`
Run: `cargo test -p pn-lp service`

Expected: PASS with reward settings wired into runtime state.

### Task 3: Add Reward-Aware Single-Sided Decision Logic

**Files:**
- Modify: `crates/pn-lp/src/decision.rs`
- Modify: `crates/pn-lp/src/types.rs`
- Test: `crates/pn-lp/src/decision.rs`

**Step 1: Write the failing test**

- Add tests for:
  - adjusted midpoint calculation using reward min size
  - qualifying bid/ask boundary rounding to tick size
  - no quotes when reward state is missing/stale/inactive
  - single-sided candidate selection
  - dynamic side selection preferring the safer side
  - inside-protection cancellation when better-priced depth drops below the threshold

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-lp decision`

Expected: FAIL because the decision engine currently ignores rewards and emits generic two-sided quotes.

**Step 3: Write minimal implementation**

- Add a reward-aware candidate builder that:
  - computes adjusted midpoint from the live order book and `rewards_min_size`
  - computes the outermost qualifying quote per side from `rewards_max_spread`
  - supports single-sided scoring when midpoint is in `[0.10, 0.90]`
  - builds only one desired quote for the safest candidate
- Implement a deterministic safety score using:
  - inside better-price ticks
  - inside cumulative depth vs quote size
  - inventory-aware bias favoring the safer side
- Keep the existing quoting engine structure intact so non-reward runtime behavior remains compatible.

**Step 4: Run targeted verification**

Run: `cargo test -p pn-lp decision`

Expected: PASS with the new reward-aware quote selection behavior covered.

### Task 4: Integrate Reward Refresh Into the Quote Loop and Final Verification

**Files:**
- Modify: `crates/pn-lp/src/service.rs`
- Modify: `crates/pn-polymarket/src/lp.rs`
- Modify: `crates/poly-notifier/src/main.rs`
- Test: `crates/pn-lp/src/service.rs`

**Step 1: Write the failing test**

- Add a service-level regression test proving the daemon cancels quotes after reward state becomes stale or inactive.
- Add coverage for quote refresh behavior when the safest side changes.

**Step 2: Run test to verify it fails**

Run: `cargo test -p pn-lp service`

Expected: FAIL because the service loop does not currently refresh reward state or use it to cancel quotes.

**Step 3: Write minimal implementation**

- Add a reward refresh timer to `LpService::run()`.
- Update quote recomputation so stale or inactive reward state results in `cancel_all` with a clear reason.
- Keep existing reconciliation, heartbeat, and stream handling intact.
- Log reward transitions and quote suppression reasons clearly enough for operator debugging.

**Step 4: Run full verification**

Run: `cargo test -p pn-polymarket`
Run: `cargo test -p pn-lp`
Run: `cargo test -p poly-notifier`
Run: `cargo test`

Expected: all targeted crates and the full workspace pass with the reward-aware path enabled by configuration.
