use crate::bot::ImageCache; // Import the cache type
use crate::db::LogEntry;
use crate::nyaa_parser;
use readability::extractor; // For HTML content extraction
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _}; // Base64 encoding
// Removed unused: use lru::LruCache;
use rand::Rng;
use serde::{Deserialize, Serialize};
use image::{imageops::FilterType, GenericImageView, ImageFormat}; // Image processing
use serde_json::{json, Value};
// Removed unused: use std::num::NonZeroUsize;
// Removed unused: use std::sync::Arc;
// Removed unused: use tokio::sync::Mutex;
use url::Url; // For parsing URLs
use std::io::Cursor; // For image encoding
use tokio::time::{sleep, timeout, Duration};


const MAX_FUNCTION_CALL_TURNS: usize = 2; // Max rounds of function calls before forcing text
const API_TIMEOUT: Duration = Duration::from_secs(60); // Timeout for each API call attempt
const MAX_API_RETRIES: usize = 3; // Max number of retries for API calls
const INITIAL_BACKOFF_DELAY: Duration = Duration::from_secs(1); // Initial delay for retries
const MAX_IMAGE_SIZE_BYTES: usize = 20 * 1024 * 1024; // Limit image download size (e.g., 20MB)
const MAX_IMAGE_PIXELS: u32 = 1_000_000; // Limit image resolution (1 megapixel)
const MAX_EXTRACTED_TEXT_LENGTH: usize = 15000; // Limit the length of extracted text (chars)

/// Formats chat history for the AI prompt.
/// Consider adding timestamps or adjusting formatting as needed for your AI.
fn format_history(history: &[LogEntry]) -> String {
    history
        .iter()
        .map(|entry| format!("{} {}: {}", entry.channel, entry.nick, entry.message))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reads the system prompt from the specified file path.
async fn read_prompt_file(prompt_path: &std::path::Path) -> Result<String> {
    tokio::fs::read_to_string(prompt_path).await.map_err(|e| {
        anyhow!(
            "Failed to read prompt file {}: {}",
            prompt_path.display(),
            e
        )
    })
}

// --- Structs for Tool Invocation Tracking ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)] // Added derives
pub struct ToolInvocation {
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)] // Added derives
pub struct ChatbotResponse {
    pub text_response: String,
    pub invoked_tools: Vec<ToolInvocation>,
}


// --- Tool Definitions ---

fn get_tools_json() -> Value {
    json!([
        {
            "functionDeclarations": [
                {
                    "name": "roll_dice",
                    "description": "Rolls one or more dice with a specified number of sides. E.g., 3d6 means roll 3 six-sided dice.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "dice_notation": {
                                "type": "string",
                                "description": "The dice notation string (e.g., '1d20', '3d6', '2d10+5'). It must be in the format [number]d[sides][+/-modifier]."
                            }
                        },
                        "required": ["dice_notation"]
                    }
                },
                {
                    "name": "download_torrent",
                    "description": "Downloads a torrent file from a Nyaa.si URL. Extracts the magnet link and initiates the download.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "nyaa_url": {
                                "type": "string",
                                "description": "The full URL of the Nyaa.si torrent page (e.g., 'https://nyaa.si/view/123456')."
                            }
                        },
                        "required": ["nyaa_url"]
                    }
                },
                {
                    "name": "fetch_and_prepare_image",
                    "description": "Downloads an image from a URL, encodes it, and prepares it for the AI to process. Checks a cache first.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "url": {
                                "type": "string",
                                "description": "The full URL of the image file (e.g., ending in .jpg, .png, .webp)."
                            }
                        },
                        "required": ["url"]
                    }
                },
                {
                    "name": "read_webpage_content",
                    "description": "Fetches a webpage URL, extracts the main article text (like reader mode), and returns it.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "url": {
                                "type": "string",
                                "description": "The full URL of the webpage to read."
                            }
                        },
                        "required": ["url"]
                    }
                }
            ]
        }
    ])
}

// --- Tool Implementations ---

/// Roll dice based on standard notation (e.g., "2d6", "1d20+3").
fn roll_dice(dice_notation: &str) -> Result<String> {
    let parts: Vec<&str> = dice_notation.split('d').collect();
    if parts.len() != 2 {
        bail!("Invalid dice notation format: {}", dice_notation);
    }

    let num_dice: u32 = parts[0].parse().context("Invalid number of dice")?;
    if num_dice == 0 || num_dice > 100 {
        // Prevent excessive rolls
        bail!("Number of dice must be between 1 and 100.");
    }

    let mut modifier: i32 = 0;
    let sides_part = parts[1];
    let sides: u32;

    if let Some(plus_idx) = sides_part.find('+') {
        sides = sides_part[..plus_idx].parse().context("Invalid number of sides")?;
        modifier = sides_part[plus_idx + 1..].parse().context("Invalid modifier")?;
    } else if let Some(minus_idx) = sides_part.find('-') {
        sides = sides_part[..minus_idx].parse().context("Invalid number of sides")?;
        modifier = -sides_part[minus_idx + 1..].parse().context("Invalid modifier")?;
    } else {
        sides = sides_part.parse().context("Invalid number of sides")?;
    }

    if sides == 0 || sides > 1000 { // Prevent unreasonable dice sizes
        bail!("Number of sides must be between 1 and 1000.");
    }

    let mut rng = rand::rng();
    let mut total = 0;
    let mut rolls = Vec::new();

    for _ in 0..num_dice {
        let roll = rng.random_range(1..=sides);
        rolls.push(roll.to_string());
        total += roll as i32;
    }

    total += modifier;

    let rolls_str = rolls.join(", ");
    let modifier_str = match modifier {
        m if m > 0 => format!(" + {}", m),
        m if m < 0 => format!(" - {}", -m),
        _ => "".to_string(),
    };

    Ok(format!(
        "Rolled {}: [{}] {} = {}",
        dice_notation, rolls_str, modifier_str, total
    ))
}


/// Fetches image data from a URL, using an in-memory cache.
/// Returns (mime_type, base64_data)
async fn fetch_and_prepare_image(
    url: &str,
    cache: &ImageCache,
) -> Result<(String, String)> {
    // 1. Check cache first
    {
        let mut cache_locked = cache.lock().await;
        if let Some((mime_type, data)) = cache_locked.get(url) {
            tracing::info!(%url, "Image cache hit");
            return Ok((mime_type.clone(), data.clone()));
        }
    } // Release lock

    tracing::info!(%url, "Image cache miss, fetching image");

    // 2. Fetch image data if not cached
    let client = reqwest::Client::new();
    let response = client.get(url)
        .timeout(Duration::from_secs(15)) // Add timeout for image download
        .send()
        .await
        .context("Failed to send request for image URL")?
        .error_for_status()
        .context("Image URL returned error status")?;

    // 3. Check Content-Type and Size
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|val| val.to_str().ok())
        .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_lowercase()) // Get primary mime type
        .unwrap_or_default();

    let allowed_mime_types = ["image/jpeg", "image/png", "image/webp", "image/gif"]; // Add gif? Gemini supports it sometimes.
    if !allowed_mime_types.contains(&content_type.as_str()) {
        bail!(
            "Unsupported image Content-Type: {}. Supported types are: {:?}",
            content_type, allowed_mime_types
        );
    }

    let content_length = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|val| val.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    if content_length > MAX_IMAGE_SIZE_BYTES {
        bail!(
            "Image size ({:.2} MB) exceeds the limit of {:.2} MB",
            content_length as f64 / (1024.0 * 1024.0),
            MAX_IMAGE_SIZE_BYTES as f64 / (1024.0 * 1024.0)
        );
    }


    // 4. Read image bytes (with size limit check again if length wasn't available)
    let image_bytes = response
        .bytes()
        .await
        .context("Failed to read image bytes")?;

    if image_bytes.len() > MAX_IMAGE_SIZE_BYTES {
         bail!(
            "Image size ({:.2} MB) exceeds the limit of {:.2} MB (checked after download)",
            image_bytes.len() as f64 / (1024.0 * 1024.0),
            MAX_IMAGE_SIZE_BYTES as f64 / (1024.0 * 1024.0)
        );
    }


    // 5. Encode as Base64
    let base64_data = BASE64_STANDARD.encode(&image_bytes);

    // 6. Store in cache
    {
        let mut cache_locked = cache.lock().await;
        cache_locked.put(url.to_string(), (content_type.clone(), base64_data.clone()));
        tracing::info!(%url, mime_type=%content_type, "Image stored in cache");
    } // Release lock

    // 5. Decode, Resize if necessary, and Re-encode
    let final_image_bytes = match image::load_from_memory(&image_bytes) {
        Ok(img) => {
            let (width, height) = img.dimensions();
            let current_pixels = width * height;

            if current_pixels > MAX_IMAGE_PIXELS {
                tracing::info!(
                    %url,
                    current_width = width,
                    current_height = height,
                    current_pixels,
                    max_pixels = MAX_IMAGE_PIXELS,
                    "Image exceeds pixel limit, resizing."
                );

                let ratio = (MAX_IMAGE_PIXELS as f64 / current_pixels as f64).sqrt();
                let new_width = (width as f64 * ratio).round() as u32;
                let new_height = (height as f64 * ratio).round() as u32;

                // Resize using Lanczos3 for good quality
                let resized_img =
                    image::imageops::resize(&img, new_width, new_height, FilterType::Lanczos3);

                // Re-encode the image back to bytes
                let mut encoded_bytes = Vec::new();
                let format = ImageFormat::from_mime_type(&content_type)
                    .unwrap_or(ImageFormat::Png); // Default to PNG if format unknown/unsupported

                // Convert to RGB8 before encoding if the target is JPEG, as JPEG doesn't support alpha
                if format == ImageFormat::Jpeg {
                    // Convert the potentially RGBA buffer to RGB8 for JPEG encoding
                    let rgb_img = image::DynamicImage::from(resized_img).to_rgb8(); // Convert via DynamicImage
                    rgb_img
                        .write_to(&mut Cursor::new(&mut encoded_bytes), format)
                        .context("Failed to encode resized image as JPEG")?;
                } else {
                    // For other formats (PNG, GIF, WebP), encode directly
                    // Note: resized_img might still be RGBA here if the original was RGBA
                    resized_img
                        .write_to(&mut Cursor::new(&mut encoded_bytes), format)
                        .context("Failed to encode resized image")?;
                }


                tracing::info!(
                    %url,
                    new_width,
                    new_height,
                    new_size_bytes = encoded_bytes.len(),
                    format = ?format,
                    "Image resized and re-encoded."
                );
                encoded_bytes // Use the resized bytes
            } else {
                // Image is within pixel limits, use original bytes
                tracing::debug!(%url, pixels=current_pixels, "Image within pixel limits, using original bytes.");
                image_bytes.to_vec() // Convert Bytes to Vec<u8>
            }
        }
        Err(e) => {
            // If decoding fails, maybe it wasn't a valid image despite mime type?
            tracing::error!(%url, error=%e, "Failed to decode image bytes, skipping resize.");
            // Fallback to using original bytes, maybe the AI can handle it? Or bail?
            // For now, let's proceed with original bytes but log the error.
             image_bytes.to_vec() // Convert Bytes to Vec<u8>
             // Alternatively, bail:
             // bail!("Failed to decode image from URL {}: {}", url, e);
        }
    };


    // 6. Encode final bytes as Base64
    let base64_data = BASE64_STANDARD.encode(&final_image_bytes);

    // 7. Store in cache (using original mime type, but potentially resized data)
    {
        let mut cache_locked = cache.lock().await;
        // Store the original mime type, but the potentially resized base64 data
        cache_locked.put(url.to_string(), (content_type.clone(), base64_data.clone()));
        tracing::info!(%url, mime_type=%content_type, "Image data (potentially resized) stored in cache");
    } // Release lock

    Ok((content_type, base64_data))
}


/// Fetches a webpage, extracts the main content text using readability.
async fn read_webpage_content(page_url: &str) -> Result<String> {
    tracing::info!(url = %page_url, "Attempting to read webpage content");

    // Parse the URL to provide a base for readability
    let url = Url::parse(page_url).context("Invalid URL provided for reading")?;

    // 1. Fetch page content
    let client = reqwest::Client::new();
    let response = client.get(url.clone()) // Use the parsed URL
        .timeout(Duration::from_secs(20)) // Timeout for fetching HTML
        .send()
        .await
        .context("Failed to send request for webpage URL")?
        .error_for_status() // Ensure success status (2xx)
        .context("Webpage URL returned error status")?;

    // 2. Check Content-Type (optional but recommended)
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|val| val.to_str().ok())
        .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_lowercase())
        .unwrap_or_default();

    if !content_type.starts_with("text/html") {
        bail!("URL does not appear to be an HTML page (Content-Type: {})", content_type);
    }

    // 3. Read HTML content
    // Consider adding a size limit here too if very large pages are a concern
    let html_content = response
        .text()
        .await
        .context("Failed to read webpage content as text")?;

    // 4. Extract content using readability
    // Use Cursor to provide Read trait input
    let product = extractor::extract(&mut Cursor::new(html_content), &url)
        .map_err(|e| anyhow!("Failed to extract content using readability: {}", e))?;

    tracing::info!(url = %page_url, extracted_chars = product.text.len(), "Successfully extracted content");

    // 5. Truncate if necessary
    let mut extracted_text = product.text;
    if extracted_text.len() > MAX_EXTRACTED_TEXT_LENGTH {
        tracing::warn!(url = %page_url, original_len = extracted_text.len(), max_len = MAX_EXTRACTED_TEXT_LENGTH, "Truncating extracted text");
        extracted_text.truncate(MAX_EXTRACTED_TEXT_LENGTH);
        extracted_text.push_str("... [truncated]");
    }

    Ok(extracted_text)
}


/// Placeholder for initiating a torrent download.
/// In a real implementation, this would likely send a message to a download manager/client.
/// For now, it extracts the magnet link and confirms initiation.
async fn download_torrent(nyaa_url: &str) -> Result<String> {
    tracing::info!(url = %nyaa_url, "Attempting to start torrent download");
    match nyaa_parser::fetch_and_extract_magnet_url(nyaa_url).await {
        Ok(magnet_url) => {
            tracing::info!(magnet = %magnet_url, "Extracted magnet link");
            // TODO: Here you would actually trigger the download process
            // This might involve sending the magnet URL to another service/thread.
            // For now, just confirm we *would* start it.
            Ok(format!(
                "Okay, I found the magnet link for {} and will start the download.",
                nyaa_url
            ))
        }
        Err(e) => {
            tracing::error!(url = %nyaa_url, error = %e, "Failed to get magnet link");
            Err(anyhow!("Failed to get magnet link for {}: {}", nyaa_url, e))
        }
    }
}


// --- Core AI Interaction Logic ---

/// For a less obvious mention such as "I wonder what Emul thinks", this does a cheap check to see if Emul ought to respond.
pub async fn chatbot_mentioned(
    chatbot_name: &str,
    triggering_message: &str,
) -> Result<bool> {
    let system_prompt = format!("You are {}. Check if the provided message is aimed at {}, or if it is merely a mention. Respond with a single word, \"respond\" or \"mention\".", chatbot_name, chatbot_name);

    // Use fast_gemini which should return text directly for this simple case
    let response_text = fast_gemini(&system_prompt, triggering_message).await?;
    tracing::trace!(response = %response_text, message = %triggering_message);

    if response_text.to_lowercase().contains("respond") {
        Ok(true)
    } else if response_text.to_lowercase().contains("mention") {
        Ok(false)
    } else {
        // It's possible the model returns slightly different phrasing.
        // We could add more robust parsing or logging here if needed.
        tracing::warn!(response = %response_text, "Unexpected response format from chatbot_mentioned check");
        // Default to false (mention) if unsure, to avoid unnecessary interruptions.
        Ok(false)
        // Or bail if strict adherence is required:
        // bail!("chatbot_mentioned failed to parse response: {}", response_text)
    }
}

pub async fn call_chatbot(
    channel: &str,
    triggering_nick: &str,
    triggering_message: &str,
    history: Vec<LogEntry>,
    prompt_path: &std::path::Path,
    was_addressed: bool,
    image_cache: &ImageCache, // Add cache parameter
) -> Result<ChatbotResponse> {
    tracing::info!(channel, nick = triggering_nick, "AI response requested.");

    let mut invoked_tools: Vec<ToolInvocation> = Vec::new();

    // 1. Read the system prompt
    let system_prompt = read_prompt_file(prompt_path).await?;

    // 2. Prepare initial history/context for the first API call
    let mut current_history = history; // Take ownership or clone if needed elsewhere
    if !was_addressed {
        // Add the triggering message if it wasn't a direct address
        current_history.push(LogEntry {
            channel: channel.to_string(),
            nick: triggering_nick.to_string(),
            message: triggering_message.to_string(),
        });
    }
    let formatted_history = format_history(&current_history);

    // Construct the prompt text based on whether the bot was addressed
    let prompt_text = if was_addressed {
        format!(
            "History:\n{}\n\n Current Trigger from {}:\n{}",
            formatted_history, triggering_nick, triggering_message
        )
    } else {
        format!(
            "History:\n{}\n\n Current trigger: Random chance (interject your opinion in the current conversation)",
            formatted_history
        )
    };
    tracing::debug!(context_size = prompt_text.len(), "Constructed initial AI context");
    tracing::trace!(context_lines = %prompt_text.lines().count(), "Context size");

    // --- Multi-Turn Function Calling Loop ---
    let mut conversation_history: Vec<Value> =
        vec![json!({"role": "user", "parts": [{"text": prompt_text}]})];
    let available_tools = get_tools_json(); // Define tools once

    for turn in 0..=MAX_FUNCTION_CALL_TURNS {
        let use_tools = turn < MAX_FUNCTION_CALL_TURNS; // Only use tools for the allowed number of turns
        let tools_param = if use_tools { Some(&available_tools) } else { None };

        tracing::info!(turn = turn + 1, use_tools, "Starting AI turn");

        // 3. Call Gemini API (with retry logic)
        let response_json = match call_gemini_with_retry(
            &system_prompt,
            &mut conversation_history, // Pass mutable ref to potentially update history inside
            "gemini-2.5-pro-exp-03-25",
            tools_param,
        )
        .await
        {
            Ok(res) => res,
            Err(e) => {
                tracing::error!(error = %e, "Gemini API call failed after retries");
                // Append an error message to history? Or just bail?
                // For now, bail.
                return Err(e.context("Gemini API call failed after retries"));
            }
        };


        // --- Process Response ---

        // Extract the model's response part(s) to add to history
        let model_response_parts = response_json["candidates"][0]["content"]["parts"].clone();
        conversation_history.push(json!({"role": "model", "parts": model_response_parts.clone()})); // Add model's turn to history

        // Check for Function Call(s)
        let function_calls: Vec<&Value> = model_response_parts
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("functionCall"))
                    .collect()
            })
            .unwrap_or_default();

        if function_calls.is_empty() {
            // 5a. No function call - Extract direct text response
            let response_text = model_response_parts
                .get(0)
                .and_then(|p0| p0.get("text"))
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow!("Gemini response missing text part"))?;

            tracing::info!(response_size = response_text.len(), "Received final AI text response");
            tracing::info!(response = %response_text);
            // Return final response along with any tools invoked in previous turns
            return Ok(ChatbotResponse {
                text_response: response_text.to_string(),
                invoked_tools,
            });
        } else {
            // 5b. Function call(s) detected
            tracing::info!(count = function_calls.len(), "Function call(s) detected, executing...");

            if !use_tools {
                // Should not happen if MAX_FUNCTION_CALL_TURNS is respected, but safety check
                tracing::error!("Function call detected but tools were disabled (turn limit exceeded).");
                return Err(anyhow!(
                    "Function call loop exceeded limit but model still requested calls"
                ));
            }

            // Add the model's function call turn to history FIRST
            conversation_history.push(json!({"role": "model", "parts": model_response_parts.clone()}));

            let mut function_responses_for_api = Vec::new(); // To build the final functionResponse part
            let mut image_data_to_inject: Option<(String, String)> = None; // Option<(mime_type, base64_data)>

            for func_call_json in function_calls {
                let name = func_call_json["name"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Function call missing name"))?;
                let args = func_call_json.get("args").cloned().unwrap_or(json!({})); // Keep args as Value

                tracing::info!(function_name = %name, args = %args, "Executing function call");

                // Record the invocation *before* executing
                invoked_tools.push(ToolInvocation {
                    name: name.to_string(),
                    args: args.clone(), // Clone args for storage
                });

                // Execute the corresponding local function
                let result_content_for_api; // This will hold the JSON for the functionResponse part

                match name {
                     "fetch_and_prepare_image" => {
                        let url = args["url"].as_str().ok_or_else(|| {
                            anyhow!("Missing 'url' argument for fetch_and_prepare_image")
                        })?;
                        match fetch_and_prepare_image(url, image_cache).await { // Pass cache
                            Ok((mime_type, base64_data)) => {
                                // Store image data to inject later
                                image_data_to_inject = Some((mime_type, base64_data));
                                // Prepare the standard success response for the API
                                result_content_for_api = json!({
                                    "result": "Image fetched successfully. Please refer to the provided image data."
                                });
                                tracing::info!("Image fetched and prepared for injection.");
                            }
                            Err(e) => {
                                // Handle download error - prepare standard error response
                                result_content_for_api = json!({ "error": e.to_string() });
                                tracing::warn!("Image fetch failed: {}", e);
                            }
                        }
                    }
                    "roll_dice" => {
                        let notation = args["dice_notation"].as_str().ok_or_else(|| {
                            anyhow!("Missing 'dice_notation' argument for roll_dice")
                        })?;
                        result_content_for_api = match roll_dice(notation) {
                            Ok(result) => json!({ "result": result }),
                            Err(e) => json!({ "error": e.to_string() }),
                        };
                    }
                    "download_torrent" => {
                        let url = args["nyaa_url"].as_str().ok_or_else(|| {
                            anyhow!("Missing 'nyaa_url' argument for download_torrent")
                        })?;
                         result_content_for_api = match download_torrent(url).await {
                            Ok(result) => json!({ "result": result }),
                            Err(e) => json!({ "error": e.to_string() }),
                        };
                    }
                    "read_webpage_content" => {
                        let url = args["url"].as_str().ok_or_else(|| {
                            anyhow!("Missing 'url' argument for read_webpage_content")
                        })?;
                        result_content_for_api = match read_webpage_content(url).await {
                            Ok(text) => json!({ "result": text }), // Return the extracted text
                            Err(e) => json!({ "error": e.to_string() }),
                        };
                    }
                    _ => {
                        tracing::warn!(function_name = %name, "Unknown function called");
                        result_content_for_api = json!({ "error": format!("Unknown function: {}", name) });
                    }
                }

                 // Add the result for this specific function call to the list for the API response turn
                 function_responses_for_api.push(json!({
                    "functionResponse": {
                        "name": name,
                        "response": result_content_for_api // Use the prepared result/error
                    }
                }));

            } // End loop over function calls in this turn


            // --- Inject Image Data if Present ---
            if let Some((mime_type, base64_data)) = image_data_to_inject {
                conversation_history.push(json!({
                    "role": "user",
                    "parts": [{
                        "inline_data": {
                            "mime_type": mime_type,
                            "data": base64_data
                        }
                    }]
                }));
                tracing::info!("Injected image data message into history.");
            }

            // --- Add the Function Response Turn ---
            // This turn contains the results/errors for ALL function calls made in the previous model turn
            conversation_history.push(json!({
                "role": "user",
                "parts": function_responses_for_api // Contains results/errors for all executed functions
            }));
            tracing::info!("Added function response message to history.");

            // Continue the loop - the history is now augmented
            continue; // Skip to next iteration
        }
    } // End of function calling loop

    // If loop finishes without returning a text response (e.g., only function calls within limit)
    tracing::error!("AI interaction finished without a final text response after {} turns.", MAX_FUNCTION_CALL_TURNS + 1);
    Err(anyhow!(
        "AI failed to provide a text response after function call iterations"
    ))
}


/// Calls the Gemini API with retry logic and exponential backoff.
async fn call_gemini_with_retry(
    system_prompt: &str,
    history: &mut Vec<Value>,
    model_version: &str,
    tools: Option<&Value>,
) -> Result<Value> {
    let mut attempts = 0;
    let mut delay = INITIAL_BACKOFF_DELAY;

    loop {
        attempts += 1;
        tracing::debug!(attempt = attempts, max_attempts = MAX_API_RETRIES + 1, "Attempting Gemini API call");

        match timeout(API_TIMEOUT, call_gemini_with_history_attempt(
            system_prompt,
            history, // Pass mutable ref down
            model_version,
            tools,
        )).await {
            Ok(Ok(response)) => return Ok(response), // Success within timeout
            Ok(Err(e)) => { // Inner function returned an error
                tracing::warn!(attempt = attempts, error = %e, "Gemini API attempt failed");
                if attempts > MAX_API_RETRIES {
                    tracing::error!("Gemini API call failed after {} attempts.", attempts);
                    return Err(e.context(format!("Gemini API call failed after {} attempts", attempts)));
                }
                // Decide if retryable (could be more sophisticated based on error type)
                // For now, retry on most errors except perhaps validation errors if identifiable.
                // The "Missing candidates" error is handled inside call_gemini_with_history_attempt
                // but other errors like network issues will trigger retry here.
            }
            Err(_) => { // Timeout occurred
                tracing::warn!(attempt = attempts, timeout = ?API_TIMEOUT, "Gemini API attempt timed out");
                 if attempts > MAX_API_RETRIES {
                    tracing::error!("Gemini API call timed out after {} attempts.", attempts);
                    return Err(anyhow!("Gemini API call timed out after {} attempts", attempts));
                }
                // Timeout is considered retryable
            }
        }

        // If we reach here, we need to retry
        tracing::info!(delay = ?delay, "Waiting before next Gemini API retry");
        sleep(delay).await;
        delay *= 2; // Exponential backoff
    }
}


/// Represents a single attempt to call the Gemini API. Called by `call_gemini_with_retry`.
/// Handles the actual HTTP request and basic response validation.
async fn call_gemini_with_history_attempt(
    system_prompt: &str,
    history: &mut Vec<Value>, // Use Value for flexibility with history parts - still mutable if needed later
    model_version: &str,
    tools: Option<&Value>, // Optional tools configuration
) -> Result<Value> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model_version,
        dotenvy::var("GEMINI_API_KEY")?
    );
    let client = reqwest::Client::new();

    // Construct the main body
    let mut body = json!({
        "contents": history, // Use the provided history directly
        "systemInstruction": {
            "parts": [{"text": system_prompt}]
        },
        "generationConfig": {
            // Ensure response is text, even if function calling happens
             "responseMimeType": "text/plain"
        }
    });

    // Add tools if provided
    if let Some(tool_config) = tools {
        body["tools"] = tool_config.clone();
        // Optionally add tool_config for modes like ANY/NONE if needed later
        // body["tool_config"] = json!({"function_calling_config": {"mode": "AUTO"}});
    }

    tracing::trace!(request_body = %body, "Sending request to Gemini");

    let response: Value = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()
        .context("Gemini API request failed")?
        .json()
        .await
        .context("Failed to parse Gemini JSON response")?;

    tracing::trace!(response_body = %response, "Received response from Gemini");

    // Basic validation: Check if candidates exist
    if response.get("candidates").is_none() {
        // Log the full error response from Gemini if available
        if let Some(error_info) = response.get("error") {
             tracing::error!(gemini_error = %error_info, "Gemini API returned an error");
             bail!("Gemini API error: {}", error_info);
        } else {
             tracing::error!(full_response = %response, "Gemini response missing 'candidates'");
             bail!("Invalid response structure from Gemini API: Missing 'candidates'");
        }
    }


    Ok(response)
}


// --- Specific Model Wrappers ---

/// Calls the 'fast' Gemini model, primarily for simple text generation (no tools used).
/// Returns the extracted text directly for convenience in simple cases like chatbot_mentioned.
async fn fast_gemini(system_prompt: &str, prompt: &str) -> Result<String> {
    // For a single prompt, create a simple history
    let mut history = vec![json!({"role": "user", "parts": [{"text": prompt}]})];
    // Call with retry logic, but without tools
    let response_json = call_gemini_with_retry(system_prompt, &mut history, "gemini-2.5-pro-exp-03-25", None).await?;

    // Extract text part, assuming no function call for this simple use case
    let response_text = response_json
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("content"))
        .and_then(|con| con.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p0| p0.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("Fast Gemini response missing text part"))?;

    Ok(response_text.to_string())
}


// --- Tests ---
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::ImageCache; // Import the type alias
    use lru::LruCache;
    use serde_json::json;
    use std::num::NonZeroUsize;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::NamedTempFile;
    use tokio::sync::Mutex;

    // Helper to ensure API key is set (tests will panic if not)
    fn ensure_api_key() {
        dotenvy::dotenv().ok(); // Load .env if present
        std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY must be set for integration tests");
    }

    // Helper to create a dummy prompt file
    async fn create_dummy_prompt_file() -> Result<(NamedTempFile, PathBuf)> {
        let temp_file = NamedTempFile::new()?;
        let path = temp_file.path().to_path_buf();
        tokio::fs::write(&path, "You are a helpful test assistant. When using tools, first check if you already have the result you need.").await?;
        Ok((temp_file, path))
    }

    #[tokio::test]
    #[ignore] // Ignored by default as it calls the real API
    async fn test_fast_gemini_live() {
        ensure_api_key();
        let system_prompt = "You are a test bot.";
        let prompt = "Briefly explain what a large language model is.";

        let result = fast_gemini(system_prompt, prompt).await;
        println!("fast_gemini result: {:?}", result); // Print for debugging

        assert!(result.is_ok());
        let response_text = result.unwrap();
        assert!(!response_text.is_empty());
        assert!(response_text.to_lowercase().contains("language model"));
    }

    #[tokio::test]
    #[ignore] // Ignored by default as it calls the real API
    async fn test_call_chatbot_roll_dice_live() {
        ensure_api_key();
        let (_temp_file, prompt_path) = create_dummy_prompt_file().await.unwrap();
        let channel = "#test";
        let nick = "tester";
        let message = "Please roll 3d6+2 for me.";
        let history = Vec::new(); // Empty history for simplicity
        // Create a dummy cache for the test
        let image_cache: ImageCache = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(1).unwrap(), // Minimal cache size for test
        )));

        let result = call_chatbot(channel, nick, message, history, &prompt_path, true, &image_cache).await;
        println!("call_chatbot (dice) result: {:?}", result); // Print for debugging

        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(!response.text_response.is_empty());

        // Check that the correct tool was invoked with the correct arguments
        assert_eq!(response.invoked_tools.len(), 1);
        let tool_call = &response.invoked_tools[0];
        assert_eq!(tool_call.name, "roll_dice");
        assert_eq!(
            tool_call.args,
            json!({"dice_notation": "3d6+2"}) // Use json! macro for comparison
        );
    }

     #[tokio::test]
     #[ignore] // Ignored by default as it calls the real API and external sites
     async fn test_call_chatbot_download_torrent_live() {
         ensure_api_key();
         let (_temp_file, prompt_path) = create_dummy_prompt_file().await.unwrap();
         let channel = "#test";
         let nick = "tester";
         // Use a known valid (or recently valid) Nyaa URL for testing
         // NOTE: This URL might become invalid over time. Replace if needed.
         let nyaa_url = "https://nyaa.si/view/1955613"; // Example URL from nyaa_parser tests
         let message = format!("Hey, can you download this for me? {}", nyaa_url);
         let history = Vec::new();
         // Create a dummy cache for the test
         let image_cache: ImageCache = Arc::new(Mutex::new(LruCache::new(
             NonZeroUsize::new(1).unwrap(), // Minimal cache size for test
         )));

         let result = call_chatbot(channel, nick, &message, history, &prompt_path, true, &image_cache).await;
         println!("call_chatbot (torrent) result: {:?}", result); // Print for debugging

         assert!(result.is_ok());
         let response = result.unwrap();
         assert!(!response.text_response.is_empty());

         // Check that the correct tool was invoked with the correct arguments
         assert_eq!(response.invoked_tools.len(), 1);
         let tool_call = &response.invoked_tools[0];
         assert_eq!(tool_call.name, "download_torrent");
         assert_eq!(
             tool_call.args,
             json!({"nyaa_url": nyaa_url}) // Use json! macro for comparison
         );
     }

     #[tokio::test]
     #[ignore] // Ignored by default as it calls the real API
     async fn test_chatbot_mentioned_live_respond() {
         ensure_api_key();
         let bot_name = "TestBot";
         let message = "Hey TestBot, what do you think?";

         let result = chatbot_mentioned(bot_name, message).await;
         println!("chatbot_mentioned (respond) result: {:?}", result);

         assert!(result.is_ok());
         assert!(result.unwrap()); // Should be true (respond)
     }

     #[tokio::test]
     #[ignore] // Ignored by default as it calls the real API
     async fn test_chatbot_mentioned_live_mention() {
         ensure_api_key();
         let bot_name = "TestBot";
         let message = "I saw TestBot in the channel earlier.";

         let result = chatbot_mentioned(bot_name, message).await;
         println!("chatbot_mentioned (mention) result: {:?}", result);

         assert!(result.is_ok());
         assert!(!result.unwrap()); // Should be false (mention)
     }
 
     #[tokio::test]
     #[ignore] // Ignored by default as it calls the real API and external sites
     async fn test_call_chatbot_read_webpage_live() {
         ensure_api_key();
         let (_temp_file, prompt_path) = create_dummy_prompt_file().await.unwrap();
         let channel = "#test";
         let nick = "tester";
         // Use the file listing page provided by the user
         let page_url = "https://blog.rust-lang.org/2025/04/03/Rust-1.86.0.html";
         let message = format!("Is trait upcasting mentiong on {}? Answer only yes or no, unless there's an error.", page_url);
         let history = Vec::new();
         let image_cache: ImageCache = Arc::new(Mutex::new(LruCache::new(
             NonZeroUsize::new(10).unwrap(),
         )));
 
         let result = call_chatbot(channel, nick, &message, history, &prompt_path, true, &image_cache).await;
         println!("call_chatbot (read webpage) result: {:?}", result); // Print for debugging
 
         assert!(result.is_ok());
         let response = result.unwrap();
 
         // Check that the correct tool was invoked
         assert!(!response.invoked_tools.is_empty());
         let read_tool_call = response.invoked_tools.iter().find(|t| t.name == "read_webpage_content");
         assert!(read_tool_call.is_some(), "read_webpage_content tool was not invoked");
         assert_eq!(
             read_tool_call.unwrap().args,
             json!({"url": page_url})
         );
 
         // Check that the final text response seems relevant
         assert!(!response.text_response.is_empty());
         let lower_response = response.text_response.to_lowercase();
         assert!(
             lower_response.contains("yes"),
             "Response doesn't seem related to the file listing page content. Response: {}", response.text_response
         );
     }
 
    #[tokio::test]
    #[ignore] // Ignored by default as it calls external URLs
    async fn test_fetch_and_prepare_image_live() {
        // No API key needed here, but good practice for consistency if other helpers use it
        // ensure_api_key();
        let image_url = "https://brage.info/GAN/ganbot2/cd41b2a5-d982-468e-b927-c324a05ba20e.0.jpeg";
        let cache: ImageCache = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(10).unwrap(), // Cache size 10
        )));

        // 1. First call (cache miss)
        let result1 = fetch_and_prepare_image(image_url, &cache).await;
        println!("fetch_and_prepare_image (1st call) result: {:?}", result1);
        assert!(result1.is_ok());
        let (mime1, data1) = result1.unwrap();
        assert_eq!(mime1, "image/jpeg");
        assert!(!data1.is_empty());

        // 2. Second call (cache hit)
        let result2 = fetch_and_prepare_image(image_url, &cache).await;
        println!("fetch_and_prepare_image (2nd call) result: {:?}", result2);
        assert!(result2.is_ok());
        let (mime2, data2) = result2.unwrap();
        assert_eq!(mime2, "image/jpeg");
        assert_eq!(data1, data2); // Data should be identical from cache

        // 3. Check cache state (optional, confirms item is present)
        {
            let cache_locked = cache.lock().await;
            assert!(cache_locked.contains(image_url));
        }
    }

    #[tokio::test]
    #[ignore] // Ignored by default as it calls the real API and external URLs
    async fn test_call_chatbot_with_image_live() {
        ensure_api_key();
        let (_temp_file, prompt_path) = create_dummy_prompt_file().await.unwrap();
        let channel = "#test";
        let nick = "tester";
        let image_url = "https://brage.info/GAN/ganbot2/cd41b2a5-d982-468e-b927-c324a05ba20e.0.jpeg";
        let message = format!("What animal is in this picture? {}", image_url);
        let history = Vec::new();
        let image_cache: ImageCache = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(10).unwrap(),
        )));

        let result = call_chatbot(channel, nick, &message, history, &prompt_path, true, &image_cache).await;
        println!("call_chatbot (image) result: {:?}", result); // Print for debugging

        assert!(result.is_ok());
        let response = result.unwrap();

        // Check that the image tool was invoked
        assert!(!response.invoked_tools.is_empty());
        let image_tool_call = response.invoked_tools.iter().find(|t| t.name == "fetch_and_prepare_image");
        assert!(image_tool_call.is_some());
        assert_eq!(
            image_tool_call.unwrap().args,
            json!({"url": image_url})
        );

        // Check that the final text response mentions the animal
        assert!(!response.text_response.is_empty());
        let lower_response = response.text_response.to_lowercase();
        // Allow for variations like "rabbit", "bunny", "hare"
        assert!(
            lower_response.contains("rabbit") || lower_response.contains("bunny") || lower_response.contains("hare"),
            "Response did not mention the expected animal. Response: {}", response.text_response
        );
    }
}
