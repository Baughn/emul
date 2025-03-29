use anyhow::{Context, Result};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod ai_handler;
mod bluenoise;
mod bot;
mod config;
mod db;

#[tokio::main]
async fn main() -> Result<()> {
    // Setup Logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("info".parse()?)) // Default to info for our crate
        .init();

    // Setup rustls
    rustls::crypto::ring::default_provider().install_default().expect("Failed to install rustls crypto provider");

    // Load Configuration
    let config = config::Config::load().context("Failed to load configuration")?;
    tracing::debug!(?config, "Configuration loaded");

    // Initialize Database
    let db_conn = db::init_db(config.db_path()).context("Failed to initialize database")?;

    // Add initial admin if needed
    db::add_initial_admin(&*db_conn.lock().await, &config.admin)
        .context("Failed to add initial admin")?;

    // Run the bot's main loop
    if let Err(e) = bot::run_bot(config, db_conn).await {
        tracing::error!("Bot exited with error: {:?}", e);
        // Depending on the error, you might want different exit codes
        return Err(e);
    }

    tracing::info!("Bot shutting down gracefully.");
    Ok(())
}
