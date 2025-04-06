use crate::ai_handler;
use crate::bluenoise::BlueNoiseInterjecter;
use crate::config::{Config, RANDOM_INTERJECT_CHANCE, RANDOM_INTERJECT_CHANCE_IF_MENTIONED};
use crate::db::{self, DbConnection};
use anyhow::Result;
use futures::prelude::*;
use irc::client::prelude::*;
use lru::LruCache;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

// Type alias for the image cache: URL -> (MimeType, Base64Data)
type ImageCache = Arc<Mutex<LruCache<String, (String, String)>>>;
const IMAGE_CACHE_SIZE: usize = 20; // Store info for the last 20 image URLs

// Shared state for the bot
#[derive(Clone)]
struct BotState {
    config: Arc<Config>,
    db_conn: DbConnection,
    current_channels: Arc<Mutex<HashSet<String>>>, // Channels bot is currently in
    prompt_path: Arc<std::path::PathBuf>, // Path to the prompt file
    bn_interject: BlueNoiseInterjecter,
    bn_interject_mention: BlueNoiseInterjecter,
    image_cache: ImageCache, // Add the image cache
}

const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300); // 5 minutes

pub async fn run_bot(config: Config, db_conn: DbConnection) -> Result<()> {
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;

    // --- Outer Reconnection Loop ---
    loop {
        tracing::info!(server = %config.server, port = %config.port, nick = %config.nickname, "Attempting to connect to IRC...");

        let irc_config = irc::client::data::Config {
            nickname: Some(config.nickname.clone()),
        nick_password: config.nickserv_password.clone(),
        server: Some(config.server.clone()),
        port: Some(config.port),
        use_tls: Some(config.use_tls),
        version: Some("EmulBotRs v0.1 - https://github.com/baughn/emulbot".to_string()), // Be polite!
            ..irc::client::data::Config::default()
        };

        // --- Connection Attempt ---
        let client_result = Client::from_config(irc_config).await;
        let mut client = match client_result {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to create IRC client config: {}", e);
                sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY); // Exponential backoff
                continue; // Retry connection
            }
        };

        if let Err(e) = client.identify() {
            tracing::error!("Failed to identify/connect to IRC server: {}", e);
            sleep(reconnect_delay).await;
            reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY); // Exponential backoff
            continue; // Retry connection
        }

        tracing::info!("Successfully connected and identified.");
        reconnect_delay = INITIAL_RECONNECT_DELAY; // Reset delay on successful connection

        // --- State Initialization (needs config reference) ---
        // Clone config for the state, original config is moved into state later
        let config_clone_for_state = config.clone();
        let state = BotState {
            config: Arc::new(config_clone_for_state), // Use the cloned config here
            db_conn: db_conn.clone(), // Clone the Arc<Mutex<Connection>>
            current_channels: Arc::new(Mutex::new(HashSet::new())), // Reset channels on reconnect
            prompt_path: Arc::new(config.prompt_path()),
            bn_interject: BlueNoiseInterjecter::new(RANDOM_INTERJECT_CHANCE),
            bn_interject_mention: BlueNoiseInterjecter::new(RANDOM_INTERJECT_CHANCE_IF_MENTIONED),
            image_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(IMAGE_CACHE_SIZE).unwrap(),
            ))), // Initialize the cache
        };

        // --- Stream and Client Arc ---
        let stream_result = client.stream();
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to get IRC stream: {}", e);
                sleep(reconnect_delay).await;
                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                continue; // Retry connection
            }
        };
        let client_arc = Arc::new(client); // Keep original client ownership here for now

        // --- Main Event Loop ---
        loop { // Inner loop for message processing
            match stream.next().await {
                Some(Ok(message)) => {
                // Spawn a task to handle the message concurrently
                    let state_clone = state.clone();
                    let client_clone = client_arc.clone(); // Clone the Arc
                    tokio::spawn(async move {
                        if let Err(e) = handle_message(client_clone, state_clone, message).await {
                            tracing::error!("Error handling message: {:?}", e);
                        }
                    });
                }
                Some(Err(e)) => {
                    tracing::error!("Connection error: {}", e);
                    // Break inner loop to trigger reconnection
                    break;
                }
                None => {
                    tracing::warn!("IRC stream ended unexpectedly.");
                    // Break inner loop to trigger reconnection
                    break;
                }
            }
        } // End of inner message processing loop

        // --- Reconnection Delay ---
        tracing::info!("Disconnected. Waiting {:?} before reconnecting...", reconnect_delay);
        sleep(reconnect_delay).await;
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY); // Exponential backoff

    } // End of outer reconnection loop
    // Note: This loop runs indefinitely, so Ok(()) is never reached unless
    // the program is explicitly terminated elsewhere.
    // If a condition to exit gracefully is needed, it should be added.
}

async fn handle_message(client: Arc<Client>, state: BotState, message: Message) -> Result<()> {
    // Log raw messages for debugging if needed
    tracing::trace!(raw_message = ?message, "Received message");

    match message.command {
        Command::NOTICE(_, ref msg) => {
            let source = message.source_nickname().unwrap_or("unknown");
            tracing::info!(from = %source, %msg, "Received NOTICE");
            // Handle NickServ notices.
            if source == "NickServ" && (msg.contains("you are now recognized") || msg.contains("is not a registered nickname")) {
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

            if target == client.current_nickname() {
                // Private message or command
                handle_admin_command(client, state, source_nick, msg).await?;
            } else if target.starts_with('#') {
                // Public message in a channel
                let channel = target;
                // Log the message first
                db::log_message(&*state.db_conn.lock().await, channel, source_nick, msg)?;

                // Check if addressed or random chance
                let bot_nick_lower = state.config.nickname.to_lowercase();
                let msg_lower = msg.to_lowercase();
                let is_addressed = msg_lower.starts_with(&format!("{}:", bot_nick_lower))
                    || msg_lower.starts_with(&format!("{},", bot_nick_lower))
                    || msg_lower.split_whitespace().next() == Some(&bot_nick_lower)
                    || (msg.to_lowercase().contains(format!(" {}", bot_nick_lower).as_str())
                        && (state.bn_interject_mention.should_interject() || ai_handler::chatbot_mentioned(&state.config.nickname, msg).await?));

                let should_trigger_ai =
                    is_addressed || state.bn_interject.should_interject();

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
    let ai_result = ai_handler::call_chatbot(
        &channel,
        &triggering_nick,
        &triggering_message,
        history,
        &state.prompt_path,
        was_addressed,
    )
    .await;

    // 3. Send Response
    match ai_result {
        Ok(response) => {
            tracing::info!(%channel, "Sending AI response");
            // Store the AI response's text part in the database
            db::log_message(&*state.db_conn.lock().await, &channel, &state.config.nickname, &response.text_response)
                .unwrap_or_else(|e| tracing::error!("Failed to log AI response: {:?}", e));
            // Split the text response for sending
            let lines = split_response(430, &response.text_response);
            for line in lines {
                if let Err(e) = sender.send_privmsg(&channel, line) {
                    tracing::error!(%channel, "Failed to send AI response chunk: {}", e);
                    // Avoid infinite loops if sending fails repeatedly
                    break;
                }
                tokio::time::sleep(Duration::from_millis(600)).await; // Small delay between lines
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
    if !db::is_admin(&*state.db_conn.lock().await, nick)? {
        tracing::warn!(%nick, "Non-admin PM command attempt");
        client.send_privmsg(
            nick,
            "Sorry, I only take commands from registered admins, desu~",
        )?;
        return Ok(());
    }

    let parts: Vec<&str> = msg.split_whitespace().collect();
    let command = parts.first().map(|s| s.to_lowercase());

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
                        nick,
                        format!("Okay! Added {} and joining now!", channel),
                    )?;
                    client.send_join(&channel)?; // Attempt to join immediately
                } else {
                    client.send_privmsg(nick, format!("I already know about {}!", channel))?;
                }
            } else {
                client.send_privmsg(nick, "Usage: !join #channel")?;
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
                        nick,
                        format!(
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
                            nick,
                            format!(
                                "Okay, leaving {} for this session (wasn't set to auto-join).",
                                channel
                            ),
                        )?;
                        client.send_part(&channel)?;
                        current.remove(&channel); // Update runtime state
                    } else {
                        client.send_privmsg(
                            nick,
                            format!("I wasn't set to auto-join {} anyway.", channel),
                        )?;
                    }
                }
            } else {
                client.send_privmsg(nick, "Usage: !part #channel")?;
            }
        }
        Some("!add_admin") => {
            if let Some(new_admin) = parts.get(1) {
                if db::add_admin(&*state.db_conn.lock().await, new_admin)? {
                    tracing::info!(admin = %nick, new_admin, "Added new admin");
                    client
                        .send_privmsg(nick, format!("Okay, '{}' is now an admin!", new_admin))?;
                } else {
                    client.send_privmsg(
                        nick,
                        format!("Failed to add '{}' (maybe already an admin?).", new_admin),
                    )?;
                }
            } else {
                client.send_privmsg(nick, "Usage: !add_admin <nickname>")?;
            }
        }
        Some("!del_admin") => {
            if let Some(admin_to_remove) = parts.get(1) {
                if admin_to_remove.eq_ignore_ascii_case(nick) {
                    client.send_privmsg(nick, "You can't remove yourself, silly!")?;
                    return Ok(());
                }
                if db::remove_admin(&*state.db_conn.lock().await, admin_to_remove)? {
                    tracing::info!(admin = %nick, removed = admin_to_remove, "Removed admin");
                    client.send_privmsg(
                        nick,
                        format!("Okay, '{}' is no longer an admin.", admin_to_remove),
                    )?;
                } else {
                    client.send_privmsg(
                        nick,
                        format!(
                            "Failed to remove '{}' (maybe not an admin?).",
                            admin_to_remove
                        ),
                    )?;
                }
            } else {
                client.send_privmsg(nick, "Usage: !del_admin <nickname>")?;
            }
        }
        Some("!admins") => match db::get_admins(&*state.db_conn.lock().await) {
            Ok(admins) => {
                if admins.is_empty() {
                    client.send_privmsg(nick, "There are no registered admins!")?;
                } else {
                    client.send_privmsg(
                        nick,
                        format!("Registered admins: {}", admins.join(", ")),
                    )?;
                }
            }
            Err(e) => {
                tracing::error!("Failed to fetch admins: {:?}", e);
                client.send_privmsg(nick, "Oops, couldn't check the admin list right now.")?;
            }
        },
        Some("!channels") => match db::get_channels(&*state.db_conn.lock().await) {
            Ok(channels) => {
                if channels.is_empty() {
                    client.send_privmsg(nick, "I'm not set to auto-join any channels.")?;
                } else {
                    client.send_privmsg(
                        nick,
                        format!("Auto-join channels: {}", channels.join(", ")),
                    )?;
                }
            }
            Err(e) => {
                tracing::error!("Failed to fetch channels: {:?}", e);
                client.send_privmsg(nick, "Oops, couldn't check the channel list right now.")?;
            }
        },
        Some("!interject") => {
            // Use the correct function name after rename
            state.bn_interject.force_next_interjection();
            client.send_privmsg(nick, "Okay, I'll try to interject soon!")?; // Adjusted message slightly
        },
        Some("!help") => {
            client.send_privmsg(nick, "Admin commands: !join <#chan>, !part <#chan>, !add_admin <nick>, !del_admin <nick>, !admins, !channels, !help")?;
        }
        _ => {
            client.send_privmsg(nick, "Hmm? Unknown command or format. Try !help.")?;
        }
    }

    Ok(())
}


/// Split a long response into multiple messages.
/// This means one message per line, but also splitting long lines.
fn split_response(limit: usize, response: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    for line in response.lines() {
        let mut remaining = line;
        while !remaining.is_empty() {
            if remaining.len() <= limit {
                parts.push(remaining);
                break;
            } else {
                // Thius is the hard bit. Find the last space before the limit, if any; otherwise, split at the limit.
                let split_at = remaining[..limit].rfind(' ').unwrap_or(limit);
                parts.push(&remaining[..split_at]);
                remaining = remaining[split_at..].trim_start();
            }
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_response() {
        let response = "This is a test response. It should be split into multiple\nmessages.";
        let parts = split_response(500, response);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "This is a test response. It should be split into multiple");
        assert_eq!(parts[1], "messages.");
    }

    #[test]
    fn test_split_long_line() {
        let response = "This is a test response. It should be split into multiple messages. This line is long enough to be split into multiple parts.";
        let parts = split_response(60, response);
        assert_eq!(parts[0], "This is a test response. It should be split into multiple");
        assert_eq!(parts[1], "messages. This line is long enough to be split into");
        assert_eq!(parts[2], "multiple parts.");
    }
}
