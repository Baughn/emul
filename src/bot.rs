use crate::ai_handler; // Use crate::db for functions
use crate::config::{Config, RANDOM_INTERJECT_CHANCE};
use crate::db::{self, DbConnection}; // Import LogEntry type
use anyhow::{Result, anyhow};
use futures::prelude::*;
use irc::client::prelude::*; // Includes Client, Message, Command etc.
use rand::prelude::*;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex; // Use tokio's Mutex for async code

// Shared state for the bot
#[derive(Clone)]
struct BotState {
    config: Arc<Config>,
    db_conn: DbConnection,
    current_channels: Arc<Mutex<HashSet<String>>>, // Channels bot is currently in
    prompt_path: Arc<std::path::PathBuf>,          // Path to the prompt file
}

pub async fn run_bot(config: Config, db_conn: DbConnection) -> Result<()> {
    tracing::info!(server = %config.server, port = %config.port, nick = %config.nickname, "Connecting to IRC");

    let irc_config = irc::client::data::Config {
        nickname: Some(config.nickname.clone()),
        nick_password: config.nickserv_password.clone(),
        server: Some(config.server.clone()),
        port: Some(config.port),
        use_tls: Some(config.use_tls),
        version: Some("EmulBotRs v0.1 - https://github.com/baughn/emulbot".to_string()), // Be polite!
        ..irc::client::data::Config::default()
    };

    let mut client = Client::from_config(irc_config).await?;
    client.identify()?; // Connects and starts PING/PONG

    let state = BotState {
        config: Arc::new(config),
        db_conn,
        current_channels: Arc::new(Mutex::new(HashSet::new())),
        prompt_path: Arc::new(Config::load()?.prompt_path()), // Load prompt path here
    };

    let mut stream = client.stream()?;
    let client = Arc::new(client);

    // --- Main Event Loop ---
    while let Some(message_result) = stream.next().await {
        match message_result {
            Ok(message) => {
                // Spawn a task to handle the message concurrently
                let state_clone = state.clone();
                let client_clone = client.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_message(client_clone, state_clone, message).await {
                        tracing::error!("Error handling message: {:?}", e);
                    }
                });
            }
            Err(e) => {
                tracing::error!("Connection error: {}", e);
                // Implement reconnection logic here if desired
                tokio::time::sleep(Duration::from_secs(15)).await;
                tracing::info!("Attempting to reconnect...");
                // This basic example exits, real bot needs reconnect loop
                return Err(anyhow!("Connection lost: {}", e));
            }
        }
    }

    Ok(())
}

async fn handle_message(client: Arc<Client>, state: BotState, message: Message) -> Result<()> {
    // Log raw messages for debugging if needed
    tracing::trace!(raw_message = ?message, "Received message");

    match message.command {
        Command::NOTICE(_, ref msg) => {
            let source = message.source_nickname().unwrap_or("unknown");
            tracing::info!(from = %source, %msg, "Received NOTICE");
            // Handle NickServ notices.
            if source == "NickServ" && msg.contains("you are now recognized") {
                // *Now* we can join our channels.
                tracing::info!("NickServ recognized us, joining channels");
                let channels = db::get_channels(&*state.db_conn.lock().await)?;
                for channel in channels {
                    client.send_join(&channel)?;
                }
            }
        },
        Command::NICK(ref new_nick) => {
            let old_nick = message.source_nickname().unwrap_or("");
            // If *our* nick changed (e.g., due to conflict)
            if old_nick == client.current_nickname() {
                tracing::info!(%old_nick, %new_nick, "My nickname changed");
                // No need to update client state, library handles it
            } else {
                // Track other users' nick changes if needed
                tracing::debug!(%old_nick, %new_nick, "User changed nick");
            }
        }

        Command::JOIN(ref channel, _, _) => {
            let joined_nick = message.source_nickname().unwrap_or("");
            if joined_nick == client.current_nickname() {
                tracing::info!(%channel, "Successfully joined");
                let mut current_chans = state.current_channels.lock().await;
                current_chans.insert(channel.clone());
            } else {
                tracing::debug!(user = %joined_nick, %channel, "User joined");
            }
        }

        Command::PART(ref channel, _) | Command::KICK(ref channel, _, _) => {
            let parted_nick = message.source_nickname().unwrap_or("");
            if parted_nick == client.current_nickname() {
                tracing::info!(%channel, "Left channel");
                let mut current_chans = state.current_channels.lock().await;
                current_chans.remove(channel);
            } else {
                tracing::debug!(user = %parted_nick, %channel, "User left");
            }
        }

        Command::PRIVMSG(ref target, ref msg) => {
            let source_nick = message.source_nickname().unwrap_or("unknown");
            tracing::debug!(from = %source_nick, %target, %msg, "PRIVMSG received");

            if target == client.current_nickname() || msg.starts_with("!") {
                // Private message or command
                handle_admin_command(client, state, source_nick, msg).await?;
            } else if target.starts_with('#') {
                // Public message in a channel
                let channel = target;
                // Log the message first
                db::log_message(&*state.db_conn.lock().await, &channel, source_nick, &msg)?;

                // Check if addressed or random chance
                let bot_nick_lower = state.config.nickname.to_lowercase();
                let msg_lower = msg.to_lowercase();
                let is_addressed = msg_lower.starts_with(&format!("{}:", bot_nick_lower))
                    || msg_lower.starts_with(&format!("{},", bot_nick_lower))
                    || msg_lower.split_whitespace().next() == Some(&bot_nick_lower);

                let should_trigger_ai =
                    is_addressed || rand::rng().random_bool(RANDOM_INTERJECT_CHANCE);

                if should_trigger_ai {
                    // Spawn AI task
                    tokio::spawn(handle_ai_request(
                        client.sender(),
                        state.clone(),
                        channel.to_string(),
                        source_nick.to_string(),
                        msg.to_string(), // Pass original message for context if needed
                        is_addressed,
                    ));
                }
            } else {
                tracing::warn!(%target, "Unknown message target");
            }
        }
        // Handle other commands if needed (PING/PONG is automatic)
        Command::PING(ref server1, server2) => {
            tracing::debug!(%server1, ?server2, "Received PING, library should handle PONG");
        }

        _ => { /* Ignore other commands for now */ }
    }

    Ok(())
}

/// Task to handle fetching history, calling AI, and sending response
async fn handle_ai_request(
    sender: irc::client::Sender,
    state: BotState,
    channel: String,
    triggering_nick: String,
    triggering_message: String,
    was_addressed: bool, // Could be used to adjust AI prompt/behaviour
) {
    tracing::info!(%channel, nick=%triggering_nick, addressed=%was_addressed, "Handling AI request");

    // 1. Fetch History
    let history_result = db::get_channel_log(&*state.db_conn.lock().await, &channel);
    if let Err(e) = history_result {
        tracing::error!(%channel, "Failed to fetch channel history: {:?}", e);
        // Maybe send an error message to the channel?
        // let _ = client.send_privmsg(&channel, "Wawa~ I couldn't remember what we were talking about!");
        return;
    }
    let history = history_result.unwrap();

    // 2. Call the AI Handler (your implementation)
    let ai_result = ai_handler::get_ai_response(
        &channel,
        &triggering_nick,
        &triggering_message,
        history,
        &state.prompt_path, // Pass prompt path
                            // Pass API key/client if needed from state
    )
    .await;

    // 3. Send Response
    match ai_result {
        Ok(response) => {
            tracing::info!(%channel, "Sending AI response");
            // Split long messages if necessary (IRC limit is ~512 bytes including overhead)
            const MAX_LEN: usize = 430; // Conservative limit for message part
            let mut remaining_msg = response.as_str();
            while !remaining_msg.is_empty() {
                let (chunk, rest) = remaining_msg.split_at(
                    remaining_msg
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|&i| i >= MAX_LEN)
                        .unwrap_or(remaining_msg.len()),
                );
                if let Err(e) = sender.send_privmsg(&channel, chunk) {
                    tracing::error!(%channel, "Failed to send AI response chunk: {}", e);
                    // Avoid infinite loops if sending fails repeatedly
                    break;
                }
                remaining_msg = rest;
                if !remaining_msg.is_empty() {
                    tokio::time::sleep(Duration::from_millis(600)).await; // Small delay between lines
                }
            }
        }
        Err(e) => {
            tracing::error!(%channel, "AI handler failed: {:?}", e);
            // Optionally send a generic error message to the channel
            let _ = sender.send_privmsg(
                &channel,
                format!(
                    "{}: Eeep! I had trouble thinking about that...",
                    triggering_nick
                ),
            );
        }
    }
}

/// Handle commands received via private message
async fn handle_admin_command(
    client: Arc<Client>,
    state: BotState,
    nick: &str,
    msg: &str,
) -> Result<()> {
    tracing::info!(from = %nick, %msg, "Admin command received");

    // Check if sender is admin
    if !db::is_admin(&*state.db_conn.lock().await, &nick)? {
        tracing::warn!(%nick, "Non-admin PM command attempt");
        client.send_privmsg(
            &nick,
            "Sorry, I only take commands from registered admins, desu~",
        )?;
        return Ok(());
    }

    let parts: Vec<&str> = msg.trim().split_whitespace().collect();
    let command = parts.get(0).map(|s| s.to_lowercase());

    match command.as_deref() {
        Some("!join") => {
            if let Some(channel) = parts.get(1) {
                let channel = if !channel.starts_with('#') {
                    format!("#{}", channel)
                } else {
                    channel.to_string()
                };
                if db::add_channel(&*state.db_conn.lock().await, &channel)? {
                    tracing::info!(admin = %nick, %channel, "Added channel via command. Joining.");
                    client.send_privmsg(
                        &nick,
                        &format!("Okay! Added {} and joining now!", channel),
                    )?;
                    client.send_join(&channel)?; // Attempt to join immediately
                } else {
                    client.send_privmsg(&nick, &format!("I already know about {}!", channel))?;
                }
            } else {
                client.send_privmsg(&nick, "Usage: !join #channel")?;
            }
        }
        Some("!part") => {
            if let Some(channel) = parts.get(1) {
                let channel = if !channel.starts_with('#') {
                    format!("#{}", channel)
                } else {
                    channel.to_string()
                };
                if db::remove_channel(&*state.db_conn.lock().await, &channel)? {
                    tracing::info!(admin = %nick, %channel, "Removed channel via command. Parting.");
                    client.send_privmsg(
                        &nick,
                        &format!(
                            "Got it! Leaving {} and won't rejoin automatically.",
                            channel
                        ),
                    )?;
                    client.send_part(&channel)?; // Part immediately
                } else {
                    // Still part if currently in? Let's check current_channels
                    let mut current = state.current_channels.lock().await;
                    if current.contains(&channel) {
                        client.send_privmsg(
                            &nick,
                            &format!(
                                "Okay, leaving {} for this session (wasn't set to auto-join).",
                                channel
                            ),
                        )?;
                        client.send_part(&channel)?;
                        current.remove(&channel); // Update runtime state
                    } else {
                        client.send_privmsg(
                            &nick,
                            &format!("I wasn't set to auto-join {} anyway.", channel),
                        )?;
                    }
                }
            } else {
                client.send_privmsg(&nick, "Usage: !part #channel")?;
            }
        }
        Some("!add_admin") => {
            if let Some(new_admin) = parts.get(1) {
                if db::add_admin(&*state.db_conn.lock().await, new_admin)? {
                    tracing::info!(admin = %nick, new_admin, "Added new admin");
                    client
                        .send_privmsg(&nick, &format!("Okay, '{}' is now an admin!", new_admin))?;
                } else {
                    client.send_privmsg(
                        &nick,
                        &format!("Failed to add '{}' (maybe already an admin?).", new_admin),
                    )?;
                }
            } else {
                client.send_privmsg(&nick, "Usage: !add_admin <nickname>")?;
            }
        }
        Some("!del_admin") => {
            if let Some(admin_to_remove) = parts.get(1) {
                if admin_to_remove.eq_ignore_ascii_case(&nick) {
                    client.send_privmsg(&nick, "You can't remove yourself, silly!")?;
                    return Ok(());
                }
                if db::remove_admin(&*state.db_conn.lock().await, admin_to_remove)? {
                    tracing::info!(admin = %nick, removed = admin_to_remove, "Removed admin");
                    client.send_privmsg(
                        &nick,
                        &format!("Okay, '{}' is no longer an admin.", admin_to_remove),
                    )?;
                } else {
                    client.send_privmsg(
                        &nick,
                        &format!(
                            "Failed to remove '{}' (maybe not an admin?).",
                            admin_to_remove
                        ),
                    )?;
                }
            } else {
                client.send_privmsg(&nick, "Usage: !del_admin <nickname>")?;
            }
        }
        Some("!admins") => match db::get_admins(&*state.db_conn.lock().await) {
            Ok(admins) => {
                if admins.is_empty() {
                    client.send_privmsg(&nick, "There are no registered admins!")?;
                } else {
                    client.send_privmsg(
                        &nick,
                        &format!("Registered admins: {}", admins.join(", ")),
                    )?;
                }
            }
            Err(e) => {
                tracing::error!("Failed to fetch admins: {:?}", e);
                client.send_privmsg(&nick, "Oops, couldn't check the admin list right now.")?;
            }
        },
        Some("!channels") => match db::get_channels(&*state.db_conn.lock().await) {
            Ok(channels) => {
                if channels.is_empty() {
                    client.send_privmsg(&nick, "I'm not set to auto-join any channels.")?;
                } else {
                    client.send_privmsg(
                        &nick,
                        &format!("Auto-join channels: {}", channels.join(", ")),
                    )?;
                }
            }
            Err(e) => {
                tracing::error!("Failed to fetch channels: {:?}", e);
                client.send_privmsg(&nick, "Oops, couldn't check the channel list right now.")?;
            }
        },
        Some("!help") => {
            client.send_privmsg(&nick, "Admin commands: !join <#chan>, !part <#chan>, !add_admin <nick>, !del_admin <nick>, !admins, !channels, !help")?;
        }
        _ => {
            client.send_privmsg(&nick, "Hmm? Unknown command or format. Try !help.")?;
        }
    }

    Ok(())
}
