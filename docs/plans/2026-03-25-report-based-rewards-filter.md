# Report-Based Rewards Filter Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a standalone CLI that reads an existing `outputs/rewards/report.json`, filters/ranks markets by reusable rules, and produces a shortlist for high-yield, lower-risk LP decisions.

**Architecture:** Extract the report schema from the existing `poly-rewards` bin into reusable library types, add a report-driven filtering module with derived risk/reward metrics and ranking presets, then expose it through a new `poly-rewards-filter` binary. Keep generation and filtering decoupled so the user can rerun selection logic without network calls.

**Tech Stack:** Rust, serde/serde_json, existing `poly-lp` reward analysis types, CLI parsing via std/env.

---

### Task 1: Extract reusable report schema

**Files:**
- Create: `crates/poly-notifier/src/rewards_report.rs`
- Modify: `crates/poly-notifier/src/lib.rs`
- Modify: `crates/poly-notifier/src/bin/poly-rewards.rs`
- Test: `crates/poly-notifier/src/rewards_report.rs`

**Step 1: Write the failing test**

Add a unit test that deserializes a minimal `report.json` payload into shared report structs and asserts the key fields survive round-trip serialization.

**Step 2: Run test to verify it fails**

Run: `cargo test -p poly-lp rewards_report -- --nocapture`
Expected: FAIL because the shared report module does not exist yet.

**Step 3: Write minimal implementation**

Create reusable `RewardsReport` and `SkippedMarket` structs with `Serialize` and `Deserialize`, then switch `poly-rewards` to use them.

**Step 4: Run test to verify it passes**

Run: `cargo test -p poly-lp rewards_report -- --nocapture`
Expected: PASS.

### Task 2: Add report-based filtering logic

**Files:**
- Create: `crates/poly-notifier/src/rewards_filter.rs`
- Test: `crates/poly-notifier/src/rewards_filter.rs`

**Step 1: Write the failing tests**

Add focused tests for:
- selecting the chosen profile from `MarketAnalysis`
- filtering by ROI / reward / crowdedness / inside ticks / config eligibility
- sorting by `roi_daily`, `daily_reward`, `crowdedness`, `safety`, and `balanced`
- derived `safety_score` / `risk_score` / `balanced_score` behavior for a high-yield crowded market vs. a slightly lower-yield safer market

**Step 2: Run tests to verify they fail**

Run: `cargo test -p poly-lp rewards_filter -- --nocapture`
Expected: FAIL because the module and types do not exist yet.

**Step 3: Write minimal implementation**

Implement reusable filter config, sort keys, profile selection, derived metrics, shortlist entry rendering, and helper output functions.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p poly-lp rewards_filter -- --nocapture`
Expected: PASS.

### Task 3: Add standalone CLI

**Files:**
- Create: `crates/poly-notifier/src/bin/poly-rewards-filter.rs`
- Modify: `crates/poly-notifier/Cargo.toml`
- Test: `crates/poly-notifier/src/bin/poly-rewards-filter.rs`

**Step 1: Write the failing tests**

Add tests for CLI argument parsing covering defaults, invalid enum values, and threshold parsing.

**Step 2: Run tests to verify they fail**

Run: `cargo test -p poly-lp poly_rewards_filter -- --nocapture`
Expected: FAIL because the binary and parser do not exist yet.

**Step 3: Write minimal implementation**

Implement `poly-rewards-filter` with:
- `--report`
- `--profile`
- `--sort`
- `--top-n`
- `--min-roi-daily`
- `--min-daily-reward`
- `--max-crowdedness`
- `--min-inside-ticks`
- `--eligible-only`
- `--out`

Load the existing report, filter/sort it, print a concise ranked summary, and optionally write a JSON shortlist file.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p poly-lp poly_rewards_filter -- --nocapture`
Expected: PASS.

### Task 4: Verify end-to-end on the real report

**Files:**
- Use: `outputs/rewards/report.json`
- Optional output: `outputs/rewards/filtered.json`

**Step 1: Run focused test suite**

Run:
- `cargo test -p poly-lp rewards_report -- --nocapture`
- `cargo test -p poly-lp rewards_filter -- --nocapture`
- `cargo test -p poly-lp poly_rewards_filter -- --nocapture`

Expected: PASS.

**Step 2: Run package tests**

Run: `cargo test -p poly-lp`
Expected: PASS.

**Step 3: Run CLI smoke checks**

Run:
- `cargo run -p poly-lp --bin poly-rewards-filter -- --report outputs/rewards/report.json --profile outer_low_risk --sort balanced --top-n 10`
- `cargo run -p poly-lp --bin poly-rewards-filter -- --report outputs/rewards/report.json --profile outer_low_risk --sort roi_daily --min-inside-ticks 2 --max-crowdedness 20 --eligible-only --out outputs/rewards/filtered.json`

Expected: Both commands succeed and print/write filtered rankings.
