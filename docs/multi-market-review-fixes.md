# Multi-Market LP Code Review Fixes

Review of the `lp-branch` multi-market LP changes. All items below need to be addressed.

## Context

The branch adds multi-market LP support: `[[lp.markets]]` config array, shared WS streams via `SharedEventFanout`, `BuyReservationLedger` for cross-market USDC coordination, `MultiLpControlHandle` for admin API, and DB schema migration adding `condition_id` columns.

The code compiles and passes all tests (2 pre-existing failures in `pn-lp` are unrelated).

---

## P0 — Must Fix

### 1. `resolve_target` does not validate condition_id existence — will panic in production

**File:** `crates/pn-admin/src/handlers.rs`, function `resolve_target` (around line 551-564)

**Problem:** When a user passes `?condition_id=0xnonexistent`, `resolve_target` returns `TargetSelection::Single("0xnonexistent")` without checking if the handle map contains it. Callers `lp_health` and `lp_state` then call `handles.snapshot(condition_id).expect("snapshot for known market")` which **panics**.

**Fix:** In `resolve_target`, when `condition_id` is `Some`, verify it exists in `handles` before returning `Single`. Return a 404 response if not found:

```rust
fn resolve_target<'a>(
    handles: &'a MultiLpControlHandle,
    condition_id: Option<&'a str>,
) -> Result<TargetSelection<'a>, Response> {
    if let Some(condition_id) = condition_id {
        // ADD THIS CHECK:
        if handles.snapshot(condition_id).is_none() {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("unknown lp market {condition_id}") })),
            ).into_response());
        }
        return Ok(TargetSelection::Single(condition_id));
    }
    // ... rest unchanged
}
```

Also remove the `.expect("snapshot for known market")` calls in `lp_health` and `lp_state` — either use `unwrap_or` with a proper error, or since `resolve_target` now validates, switch to `unwrap()` with a comment explaining the invariant.

Alternatively, add a `pub fn contains(&self, condition_id: &str) -> bool` method to `MultiLpControlHandle` for cleaner checking.

### 2. `broadcast` partially executes on failure — some markets get the command, others don't

**File:** `crates/pn-lp/src/control.rs`, function `broadcast` (around line 384-391)

**Problem:** The loop sends to each handle sequentially. If the 3rd of 5 handles fails, markets 1-2 already received the command (e.g., Pause) but 3-5 did not. The caller gets an `Err` but has no idea which markets were affected.

**Fix:** Change to best-effort — send to all, collect errors, return a combined result:

```rust
pub fn broadcast(&self, command: ControlCommand) -> Result<Vec<String>, String> {
    let mut targets = Vec::with_capacity(self.handles.len());
    let mut errors = Vec::new();
    for (condition_id, handle) in self.handles.iter() {
        match handle.send(command.clone()) {
            Ok(()) => targets.push(condition_id.clone()),
            Err(e) => errors.push(format!("{condition_id}: {e}")),
        }
    }
    if errors.is_empty() {
        Ok(targets)
    } else {
        Err(format!(
            "partial broadcast failure — succeeded: [{}], failed: [{}]",
            targets.join(", "),
            errors.join("; ")
        ))
    }
}
```

### 3. Dead code should be deleted, not `#[allow(dead_code)]`

**File:** `crates/pn-polymarket/src/lp.rs`

**Problem:** Four methods on `PolymarketExecutionClient` are no longer called after the refactor to free functions:
- `start_market_stream` (line ~1083)
- `start_tick_size_stream` (line ~1157)
- `start_order_stream` (line ~1223)
- `start_trade_stream` (line ~1304)

They were replaced by the standalone `spawn_market_stream`, `spawn_tick_size_stream`, `spawn_order_stream`, `spawn_trade_stream` functions. The old methods just have `#[allow(dead_code)]` slapped on them.

**Fix:** Delete all four `start_*_stream` methods entirely. They are ~300 lines of dead code duplicating the new free functions.

---

## P1 — Should Fix

### 4. TOML generation doesn't escape newlines in `question` field

**File:** `crates/poly-notifier/src/rewards_filter.rs`, function `render_multi_market_config`

**Problem:** The `question` field is escaped for `\` and `"`, but not for newlines, tabs, or other control characters. If a Polymarket question contains a newline (unlikely but possible), the generated TOML will be invalid.

**Fix:** Either:
- Also replace `\n` → `\\n`, `\t` → `\\t`, `\r` → `\\r`
- Or use a TOML serialization library (e.g. `toml::to_string`) for the value instead of manual string formatting

### 5. Confirm `cancel_market_orders(None)` semantics

**File:** `crates/poly-notifier/src/main.rs`, line ~181

**Problem:** `cancel_all` implementation was changed from `self.client.cancel_all()` to `self.client.cancel_market_orders(None)`. Need to verify that `cancel_market_orders(None)` means "cancel orders for this client's configured market" and NOT "cancel all orders across the entire account". The whole point of multi-market is that one market's cancel-all shouldn't nuke other markets' orders.

**Fix:** Read the `cancel_market_orders` implementation and confirm. If `None` means account-wide cancel, this needs to be changed to pass the specific condition_id/asset_ids.

### 6. `build_service_config` (parameterless) is fragile

**File:** `crates/poly-notifier/src/main.rs`, around line 3000-3007

**Problem:** This function exists only for tests. It calls `resolved_markets(config)?.into_iter().next().expect(...)` which will panic if config has no `condition_id` and no `markets`. It's also wasteful — resolves all markets just to take the first.

**Fix:** Either:
- Remove it entirely and have tests call `build_service_config_for_market` directly
- Or add a `#[cfg(test)]` attribute and a clearer doc comment

---

## P2 — Nice to Have

### 7. Deduplicate the four `spawn_*_stream` functions

**File:** `crates/pn-polymarket/src/lp.rs`

The four `spawn_*_stream` free functions have identical structure: backoff loop → subscribe → select → reconnect. ~300 lines of near-identical code. Could be extracted into a generic helper:

```rust
fn spawn_reconnecting_stream<S, T, F, M>(
    subscribe_fn: F,
    map_fn: M,
    event_tx: mpsc::UnboundedSender<StreamEvent>,
    cancel: CancellationToken,
    stream_name: &'static str,
)
```

This is a refactor and not blocking — do it separately if desired.

### 8. `SharedEventFanout` subscriber list grows unboundedly

**File:** `crates/poly-notifier/src/shared_streams.rs`

`register` pushes senders into a `Vec` per asset_id. `dispatch` retains only live senders, but if `register` is called multiple times for the same asset_id (e.g., during reconnection), stale senders accumulate until the next `dispatch`. Not a real problem in practice since `register` is only called once per market at startup.

---

## Files to Touch (summary)

| File | Changes |
|------|---------|
| `crates/pn-admin/src/handlers.rs` | Fix #1: validate condition_id in `resolve_target` |
| `crates/pn-lp/src/control.rs` | Fix #2: best-effort `broadcast` |
| `crates/pn-polymarket/src/lp.rs` | Fix #3: delete 4 dead `start_*_stream` methods |
| `crates/poly-notifier/src/rewards_filter.rs` | Fix #4: escape newlines in TOML question field |
| `crates/poly-notifier/src/main.rs` | Fix #5: verify cancel semantics; Fix #6: clean up test helper |
