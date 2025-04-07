use crate::ai_handler;
use crate::bluenoise::BlueNoiseInterjecter;
use crate::config::{Config, RANDOM_INTERJECT_CHANCE, RANDOM_INTERJECT_CHANCE_IF_MENTIONED};
use crate::db::{self, DbConnection};
use anyhow::Result;
use futures::prelude::*;
use irc::client::prelude::*;
use lru::LruCache;
use std::collections::{HashMap, HashSet}; // Added HashMap
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant}; // Added Instant
use tokio::sync::Mutex;
use tokio::time::sleep;

// Type alias for the image cache: URL -> (MimeType, Base64Data)
pub type ImageCache = Arc<Mutex<LruCache<String, (String, String)>>>; // Make public
const IMAGE_CACHE_SIZE: usize = 20; // Store info for the last 20 image URLs
const MESSAGE_BUFFER_TIMEOUT: Duration = Duration::from_millis(1500); // 1.5 seconds
const MESSAGE_SWEEPER_INTERVAL: Duration = Duration::from_millis(500); // Check every 0.5 seconds

// Holds message fragments while waiting for potential continuations
struct BufferedMessage {
    message: String,
    last_arrival: Instant,
}

// Shared state for the bot
#[derive(Clone)]
pub struct BotState { // Make struct public too, as ImageCache is used in its field
    config: Arc<Config>,
    db_conn: DbConnection,
    current_channels: Arc<Mutex<HashSet<String>>>, // Channels bot is currently in
    prompt_path: Arc<std::path::PathBuf>, // Path to the prompt file
    bn_interject: BlueNoiseInterjecter,
    bn_interject_mention: BlueNoiseInterjecter,
    image_cache: ImageCache,
    // Buffer for potentially fragmented messages: (Channel, Nick) -> BufferedMessage
    message_buffer: Arc<Mutex<HashMap<(String, String), BufferedMessage>>>,
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
            ))),
            message_buffer: Arc::new(Mutex::new(HashMap::new())), // Initialize buffer
        };

        // --- Stream, Client Arc, and Sweeper Task ---
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
        let sender = client_arc.sender(); // Get sender for sweeper

        // --- Start Message Buffer Sweeper Task ---
        let state_for_sweeper = state.clone();
        tokio::spawn(async move {
            message_buffer_sweeper(sender, state_for_sweeper).await;
        });

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
                let nick = source_nick;

                // --- Message Buffering Logic ---
                let mut buffer = state.message_buffer.lock().await;
                let key = (channel.to_string(), nick.to_string());
                let now = Instant::now();

                buffer
                    .entry(key)
                    .and_modify(|entry| {
                        entry.message.push(' '); // Add space between fragments
                        entry.message.push_str(msg);
                        entry.last_arrival = now;
                        tracing::trace!(%channel, %nick, "Appended message fragment");
                    })
                    .or_insert_with(|| {
                        tracing::trace!(%channel, %nick, "Started buffering message");
                        BufferedMessage {
                            message: msg.to_string(),
                            last_arrival: now,
                        }
                    });
                // Drop the lock explicitly before any potential await points if needed later
                drop(buffer);
                // --- End Message Buffering Logic ---
                // NOTE: Actual processing (logging, AI trigger) is now handled by the sweeper task
            } else {
                tracing::warn!(%target, "Unknown message target type");
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

// --- New Function: Background task to process completed messages from buffer ---
async fn message_buffer_sweeper(sender: Sender, state: BotState) {
    tracing::debug!("Message buffer sweeper task started.");
    loop {
        tokio::time::sleep(MESSAGE_SWEEPER_INTERVAL).await;

        let mut buffer = state.message_buffer.lock().await;
        let now = Instant::now();
        let mut messages_to_process = Vec::new();

        // Identify and collect messages that have timed out
        buffer.retain(|(channel, nick), buffered_msg| {
            if now.duration_since(buffered_msg.last_arrival) > MESSAGE_BUFFER_TIMEOUT {
                tracing::debug!(%channel, %nick, "Message buffer timed out, processing.");
                messages_to_process.push((
                    channel.clone(),
                    nick.clone(),
                    buffered_msg.message.clone(), // Clone message to process outside lock
                ));
                false // Remove from buffer
            } else {
                true // Keep in buffer
            }
        });

        // Drop the lock before potentially long-running processing
        drop(buffer);

        // Spawn processing tasks for each completed message
        for (channel, nick, message) in messages_to_process {
            let sender_clone = sender.clone();
            let state_clone = state.clone();
            tokio::spawn(async move {
                 if let Err(e) = process_complete_message(sender_clone, state_clone, channel, nick, message).await {
                     tracing::error!("Error processing completed message: {:?}", e);
                 }
            });
        }
    }
    // Note: This loop runs indefinitely. If graceful shutdown is needed, add cancellation logic.
}


// --- New Function: Process a fully assembled message ---
async fn process_complete_message(
    sender: Sender,
    state: BotState,
    channel: String,
    nick: String,
    complete_message: String,
) -> Result<()> {
    tracing::debug!(%channel, %nick, msg=%complete_message, "Processing complete message");

    // 1. Log the complete message
    // Use a separate connection lock scope
    {
        let conn = state.db_conn.lock().await;
        db::log_message(&conn, &channel, &nick, &complete_message)?;
    } // Lock released here

    // 2. Check if AI should be triggered
    let bot_nick_lower = state.config.nickname.to_lowercase();
    let msg_lower = complete_message.to_lowercase();
    // Re-evaluate addressing based on the complete message
    let is_addressed = msg_lower.starts_with(&format!("{}:", bot_nick_lower))
        || msg_lower.starts_with(&format!("{},", bot_nick_lower))
        || msg_lower.split_whitespace().next() == Some(&bot_nick_lower)
        || (msg_lower.contains(format!(" {}", bot_nick_lower).as_str())
            && (state.bn_interject_mention.should_interject()
                || ai_handler::chatbot_mentioned(&state.config.nickname, &complete_message).await?)); // Pass complete message

    let should_trigger_ai = is_addressed || state.bn_interject.should_interject();

    // 3. Spawn AI task if needed
    if should_trigger_ai {
        tracing::info!(%channel, %nick, addressed=%is_addressed, "Triggering AI for completed message");
        // Spawn AI task, passing the complete message
        tokio::spawn(handle_ai_request(
            sender, // Pass the sender clone
            state,  // Pass the state clone
            channel, // Pass channel ownership
            nick,    // Pass nick ownership
            complete_message, // Pass the complete message
            is_addressed,
        ));
    } else {
        tracing::debug!(%channel, %nick, "No AI trigger for completed message");
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
        &state.image_cache, // Pass the image cache
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
