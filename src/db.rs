use crate::config::LOG_HISTORY_LINES;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub channel: String,
    pub nick: String,
    pub message: String,
}

pub type DbConnection = Arc<Mutex<rusqlite::Connection>>;

// --- Initialization ---

pub fn init_db(db_path: impl AsRef<Path>) -> Result<DbConnection> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "BEGIN;
        -- Channels to auto-join
        CREATE TABLE IF NOT EXISTS channels (
            channel_name TEXT PRIMARY KEY COLLATE NOCASE
        );
        -- Admin users
        CREATE TABLE IF NOT EXISTS admins (
            nick TEXT PRIMARY KEY COLLATE NOCASE
        );
        -- Message log per channel
        CREATE TABLE IF NOT EXISTS message_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            channel_name TEXT COLLATE NOCASE NOT NULL,
            timestamp INTEGER NOT NULL, -- Unix timestamp (seconds)
            nick TEXT NOT NULL,
            message TEXT NOT NULL
        );
        -- Index for faster log retrieval
        CREATE INDEX IF NOT EXISTS idx_message_log_channel_time
        ON message_log (channel_name, timestamp DESC);
        COMMIT;",
    )?;
    tracing::info!("Database initialized successfully");
    Ok(Arc::new(Mutex::new(conn)))
}

pub fn add_initial_admin(conn: &Connection, admin_nick: &str) -> Result<()> {
    let count: u32 = conn.query_row("SELECT COUNT(*) FROM admins", [], |row| row.get(0))?;
    if count == 0 {
        conn.execute(
            "INSERT OR IGNORE INTO admins (nick) VALUES (?)",
            params![admin_nick],
        )?;
        tracing::info!(initial_admin = %admin_nick, "Initial admin added.");
    } else {
        tracing::debug!("Admin table not empty, skipping initial admin add.");
    }
    Ok(())
}

// --- Channel Management ---

pub fn get_channels(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT channel_name FROM channels ORDER BY channel_name")?;
    let channel_iter = stmt.query_map([], |row| row.get(0))?;
    let mut result = Vec::new();
    for channel in channel_iter {
        result.push(channel?);
    }
    Ok(result)
}

pub fn add_channel(conn: &Connection, channel: &str) -> Result<bool> {
    let changes = conn.execute(
        "INSERT OR IGNORE INTO channels (channel_name) VALUES (?)",
        params![channel],
    )?;
    Ok(changes > 0)
}

pub fn remove_channel(conn: &Connection, channel: &str) -> Result<bool> {
    let changes = conn.execute(
        "DELETE FROM channels WHERE channel_name = ?",
        params![channel],
    )?;
    Ok(changes > 0)
}

// --- Admin Management ---

pub fn is_admin(conn: &Connection, nick: &str) -> Result<bool> {
    let is_admin = conn
        .query_row(
            "SELECT 1 FROM admins WHERE nick = ? COLLATE NOCASE", // Ensure case-insensitive check
            params![nick],
            |_| Ok(true), // If row exists, return true
        )
        .optional()?
        .is_some();
    Ok(is_admin)
}

pub fn add_admin(conn: &Connection, nick: &str) -> Result<bool> {
    let changes = conn.execute(
        "INSERT OR IGNORE INTO admins (nick) VALUES (?)",
        params![nick],
    )?;
    Ok(changes > 0)
}

pub fn remove_admin(conn: &Connection, nick: &str) -> Result<bool> {
    let changes = conn.execute("DELETE FROM admins WHERE nick = ?", params![nick])?;
    Ok(changes > 0)
}

pub fn get_admins(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT nick FROM admins ORDER BY nick")?;
    let admin_iter = stmt.query_map([], |row| row.get(0))?;
    let mut admins = Vec::new();
    for admin in admin_iter {
        admins.push(admin?);
    }
    Ok(admins)
}

// --- Message Logging ---

pub fn log_message(conn: &Connection, channel: &str, nick: &str, message: &str) -> Result<()> {
    let channel = channel.to_string();
    let nick = nick.to_string();
    let message = message.to_string();
    let timestamp = Utc::now().timestamp();

    conn.execute(
        "INSERT INTO message_log (channel_name, timestamp, nick, message) VALUES (?, ?, ?, ?)",
        params![channel, timestamp, nick, message],
    )?;
    // Optional: Add log cleaning here (e.g., DELETE FROM message_log WHERE timestamp < ?)
    Ok(())
}

pub fn get_channel_log(conn: &Connection, channel: &str) -> Result<Vec<LogEntry>> {
    let channel = channel.to_string();
    let limit = LOG_HISTORY_LINES as i64;

    // Fetch in ascending order to reconstruct conversation flow easily
    let mut stmt = conn.prepare(
        "SELECT timestamp, nick, message
            FROM (
                SELECT timestamp, nick, message
                FROM message_log
                WHERE channel_name = ?1
                ORDER BY timestamp DESC
                LIMIT ?2
            ) ORDER BY timestamp ASC",
    )?;
    let entry_iter = stmt.query_map(params![channel, limit], |row| {
        let timestamp_secs: i64 = row.get(0)?;
        Ok(LogEntry {
            // Use timestamp_opt for safe conversion
            timestamp: DateTime::from_timestamp(timestamp_secs, 0).unwrap_or_else(|| Utc::now()), // Fallback if invalid
            channel: channel.clone(),
            nick: row.get(1)?,
            message: row.get(2)?,
        })
    })?;
    let mut result = Vec::new();
    for entry in entry_iter {
        result.push(entry?);
    }
    Ok(result)
}
