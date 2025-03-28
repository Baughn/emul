use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

pub const DB_FILE_PATH: &str = "emul_bot_memory.sqlite";
pub const PROMPT_FILE_PATH: &str = "vorpal_bunny_prompt.txt";
pub const LOG_HISTORY_LINES: usize = 500;
pub const RANDOM_INTERJECT_CHANCE: f64 = 0.02; // 2% chance
pub const RANDOM_INTERJECT_CHANCE_IF_MENTIONED: f64 = 0.2;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Config {
    /// IRC server address
    #[arg(long)]
    pub server: String,

    /// IRC server port
    #[arg(long, default_value_t = 6697)] // Default to common SSL port
    pub port: u16,

    /// Bot's nickname
    #[arg(long, short, default_value = "Emul")]
    pub nickname: String,

    /// Initial admin nickname (can also be set via EMUL_BOT_ADMIN env var)
    #[arg(long, env = "EMUL_BOT_ADMIN", default_value = "Baughn")]
    pub admin: String,

    /// Optional NickServ password (can also be set via NICKSERV_PASSWORD env var)
    #[arg(long, env = "NICKSERV_PASSWORD")]
    pub nickserv_password: Option<String>,

    /// Use TLS (SSL) for the connection
    #[arg(long, default_value_t = true)]
    pub use_tls: bool,
}

impl Config {
    pub fn load() -> Result<Self> {
        // Load .env file if present
        dotenvy::dotenv().ok(); // Ignore error if .env doesn't exist

        Ok(Config::parse())
    }

    pub fn db_path(&self) -> PathBuf {
        PathBuf::from(DB_FILE_PATH)
    }

    pub fn prompt_path(&self) -> PathBuf {
        PathBuf::from(PROMPT_FILE_PATH)
    }
}
