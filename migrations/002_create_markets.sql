CREATE TABLE IF NOT EXISTS markets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    condition_id TEXT NOT NULL UNIQUE,
    question TEXT NOT NULL,
    slug TEXT,
    outcomes TEXT NOT NULL DEFAULT '["Yes","No"]',  -- JSON array
    token_ids TEXT NOT NULL DEFAULT '[]',             -- JSON array
    last_prices TEXT NOT NULL DEFAULT '[]',           -- JSON array of decimals
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at DATETIME NOT NULL DEFAULT (datetime('now')),
    updated_at DATETIME NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_markets_condition_id ON markets(condition_id);
CREATE INDEX IF NOT EXISTS idx_markets_is_active ON markets(is_active);
