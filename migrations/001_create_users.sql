CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    telegram_id INTEGER NOT NULL,
    bot_id TEXT NOT NULL,
    username TEXT,
    tier TEXT NOT NULL DEFAULT 'free' CHECK(tier IN ('free', 'premium', 'unlimited')),
    max_subscriptions INTEGER NOT NULL DEFAULT 5,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    created_at DATETIME NOT NULL DEFAULT (datetime('now')),
    updated_at DATETIME NOT NULL DEFAULT (datetime('now')),
    UNIQUE(telegram_id, bot_id)
);
