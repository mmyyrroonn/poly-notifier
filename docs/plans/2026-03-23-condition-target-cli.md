# Condition Target CLI Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a standalone interactive CLI that accepts a Polymarket event or market link, resolves candidate markets, lets the operator choose one, and writes the selected `lp.trading.condition_id` into `config/default.toml`.

**Architecture:** Keep the feature outside the live daemon path by adding a second binary under the existing `poly-lp` package. Reuse `pn-polymarket::GammaClient` for market discovery, add small pure helpers for input parsing and TOML line replacement, and drive the terminal interaction with `stdin/stdout`.

**Tech Stack:** Rust, Tokio, existing `pn-polymarket` Gamma client, manual stdin/stdout prompts, targeted unit tests.

---

### Task 1: Add failing tests for standalone helpers

**Files:**
- Modify: `crates/poly-notifier/src/bin/poly-target.rs`

**Step 1: Write the failing tests**

Add unit tests that cover:
- extracting a lookup token from `event URL`, `market URL`, plain `slug`, and raw `condition_id`
- replacing `lp.trading.condition_id` inside a TOML string without disturbing other sections
- rejecting config content that has no `[lp.trading]` section or no `condition_id` key inside that section

**Step 2: Run test to verify it fails**

Run: `cargo test -p poly-lp --bin poly-target`
Expected: FAIL because the new binary/helpers do not exist yet.

### Task 2: Implement the standalone CLI

**Files:**
- Create: `crates/poly-notifier/src/bin/poly-target.rs`
- Modify: `crates/poly-notifier/Cargo.toml`

**Step 1: Add the new binary target**

Register a second binary:
- `poly-target` -> `src/bin/poly-target.rs`

**Step 2: Implement minimal helper layer**

Add pure helpers for:
- classifying user input as `condition_id`, URL-derived slug, or plain slug
- formatting candidate markets for terminal display
- updating `config/default.toml` contents by replacing `lp.trading.condition_id`

**Step 3: Implement runtime flow**

Behavior:
- accept a required positional input: Polymarket URL, slug, or condition ID
- if raw `condition_id`, write it directly after confirmation
- otherwise resolve candidates with `GammaClient`
- if exactly one candidate is returned, confirm before writing
- if multiple candidates are returned, print numbered choices and read a selection from stdin
- write back to `config/default.toml`
- print the chosen question, slug, and `condition_id`

### Task 3: Verify end-to-end behavior

**Files:**
- Test: `crates/poly-notifier/src/bin/poly-target.rs`

**Step 1: Run targeted binary tests**

Run: `cargo test -p poly-lp --bin poly-target`
Expected: PASS

**Step 2: Run formatting and full verification**

Run: `cargo fmt`
Expected: PASS

Run: `cargo test`
Expected: PASS

**Step 3: Document operator invocation**

Include example commands in the final handoff, for example:

```bash
cargo run -p poly-lp --bin poly-target -- "https://polymarket.com/event/..."
```
