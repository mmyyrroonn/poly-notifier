CREATE TABLE IF NOT EXISTS lp_orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    order_id TEXT NOT NULL UNIQUE,
    client_order_id TEXT,
    condition_id TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    side TEXT NOT NULL,
    price TEXT NOT NULL,
    size TEXT NOT NULL,
    status TEXT NOT NULL,
    strategy_reason TEXT,
    created_at DATETIME NOT NULL DEFAULT (datetime('now')),
    updated_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lp_orders_condition_id ON lp_orders(condition_id);
CREATE INDEX IF NOT EXISTS idx_lp_orders_asset_id ON lp_orders(asset_id);
CREATE INDEX IF NOT EXISTS idx_lp_orders_status ON lp_orders(status);

CREATE TABLE IF NOT EXISTS lp_trades (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    trade_id TEXT NOT NULL UNIQUE,
    order_id TEXT,
    condition_id TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    side TEXT NOT NULL,
    price TEXT NOT NULL,
    size TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT (datetime('now')),
    updated_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lp_trades_condition_id ON lp_trades(condition_id);
CREATE INDEX IF NOT EXISTS idx_lp_trades_asset_id ON lp_trades(asset_id);

CREATE TABLE IF NOT EXISTS lp_positions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    condition_id TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    position_size TEXT NOT NULL,
    avg_price TEXT NOT NULL,
    usdc_balance TEXT NOT NULL,
    snapshot_type TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lp_positions_condition_id ON lp_positions(condition_id);
CREATE INDEX IF NOT EXISTS idx_lp_positions_created_at ON lp_positions(created_at);

CREATE TABLE IF NOT EXISTS lp_risk_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL,
    severity TEXT NOT NULL,
    details_json TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lp_risk_events_created_at ON lp_risk_events(created_at);
CREATE INDEX IF NOT EXISTS idx_lp_risk_events_severity ON lp_risk_events(severity);

CREATE TABLE IF NOT EXISTS lp_heartbeats (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    heartbeat_id TEXT NOT NULL,
    status TEXT NOT NULL,
    note TEXT,
    created_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lp_heartbeats_created_at ON lp_heartbeats(created_at);

CREATE TABLE IF NOT EXISTS lp_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    report_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lp_reports_created_at ON lp_reports(created_at);

CREATE TABLE IF NOT EXISTS lp_control_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    action TEXT NOT NULL,
    reason TEXT,
    created_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_lp_control_actions_created_at ON lp_control_actions(created_at);
