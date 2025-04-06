use crate::db::LogEntry;
use crate::nyaa_parser; // Import the nyaa parser
use anyhow::{anyhow, bail, Context, Result};
use rand::Rng; // Import rand for dice rolling
use serde_json::{json, Value}; // Import Value for handling JSON

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

    let mut rng = rand::thread_rng();
    let mut total = 0;
    let mut rolls = Vec::new();

    for _ in 0..num_dice {
        let roll = rng.gen_range(1..=sides);
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
) -> Result<String> {
    tracing::info!(channel, nick = triggering_nick, "AI response requested.");

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

    // Define tools for this call
    let tools = get_tools_json();

    // --- Multi-Turn Function Calling Loop ---
    // We might need multiple turns if the model calls functions.
    // For simplicity, this implementation handles one round of function calls.
    // A more robust version might loop until a text response is received.

    // 3. First API Call (with tools)
    let initial_response = smart_gemini(&system_prompt, &prompt_text, Some(&tools)).await?;

    // 4. Check for Function Call(s) in the response
    let function_calls = initial_response
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("content"))
        .and_then(|con| con.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("functionCall"))
                .collect::<Vec<&Value>>()
        })
        .unwrap_or_default();

    if function_calls.is_empty() {
        // 5a. No function call - Extract direct text response
        let response_text = initial_response
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("content"))
            .and_then(|con| con.get("parts"))
            .and_then(|p| p.get(0))
            .and_then(|p0| p0.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Gemini response missing text part"))?;

        tracing::info!(response_size = response_text.len(), "Received direct AI response");
        tracing::info!(response = %response_text);
        Ok(response_text.to_string())
    } else {
        // 5b. Function call(s) detected - Execute functions and respond
        tracing::info!(count = function_calls.len(), "Function call(s) detected");

        let mut function_responses = Vec::new();

        // Store the model's request containing the function calls
        let model_request_part = initial_response["candidates"][0]["content"]["parts"].clone();

        for func_call_json in function_calls {
            let name = func_call_json["name"]
                .as_str()
                .ok_or_else(|| anyhow!("Function call missing name"))?;
            let args = func_call_json
                .get("args")
                .cloned()
                .unwrap_or(json!({})); // Default to empty object if args are missing

            tracing::info!(function_name = %name, args = %args, "Executing function call");

            // Execute the corresponding local function
            let result_content = match name {
                "roll_dice" => {
                    let notation = args["dice_notation"]
                        .as_str()
                        .ok_or_else(|| anyhow!("Missing 'dice_notation' argument for roll_dice"))?;
                    match roll_dice(notation) {
                        Ok(result) => json!({ "result": result }),
                        Err(e) => json!({ "error": e.to_string() }),
                    }
                }
                "download_torrent" => {
                    let url = args["nyaa_url"]
                        .as_str()
                        .ok_or_else(|| anyhow!("Missing 'nyaa_url' argument for download_torrent"))?;
                    match download_torrent(url).await {
                        Ok(result) => json!({ "result": result }),
                        Err(e) => json!({ "error": e.to_string() }),
                    }
                }
                _ => {
                    tracing::warn!(function_name = %name, "Unknown function called");
                    json!({ "error": format!("Unknown function: {}", name) })
                }
            };

            // Add the result to the list of responses to send back
            function_responses.push(json!({
                "functionResponse": {
                    "name": name,
                    "response": result_content // Send back the JSON result/error
                }
            }));
        }

        // 6. Construct the second API call context
        //    Need to include: original user prompt, model's function call request, our function responses
        let mut history_for_final_call = vec![
            json!({"role": "user", "parts": [{"text": prompt_text}]}), // Original user prompt context
            json!({"role": "model", "parts": model_request_part}), // Model's request with function calls
            json!({"role": "user", "parts": function_responses}), // Our function execution results
        ];

        // 7. Second API Call (sending function results)
        //    No tools needed here, we expect a text response.
        let final_response_json = call_gemini_with_history(
            &system_prompt,
            &mut history_for_final_call, // Pass the constructed history
            "gemini-2.5-pro-exp-03-25", // Use the appropriate model
            None, // No tools needed for the final response generation
        )
        .await?;

        // 8. Extract final text response
        let final_text = final_response_json
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("content"))
            .and_then(|con| con.get("parts"))
            .and_then(|p| p.get(0))
            .and_then(|p0| p0.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("Gemini final response missing text part after function call"))?;

        tracing::info!(response_size = final_text.len(), "Received final AI response after function call");
        tracing::info!(response = %final_text);
        Ok(final_text.to_string())
    }
}


/// Generic function to call the Gemini API.
/// Can handle simple text prompts or multi-turn history including function calls/responses.
async fn call_gemini_with_history(
    system_prompt: &str,
    history: &mut Vec<Value>, // Use Value for flexibility with history parts
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

/// Calls the 'smart' Gemini model, potentially using tools.
async fn smart_gemini(
    system_prompt: &str,
    prompt: &str,
    tools: Option<&Value>,
) -> Result<Value> {
    // For a single prompt, create a simple history
    let mut history = vec![json!({"role": "user", "parts": [{"text": prompt}]})];
    call_gemini_with_history(system_prompt, &mut history, "gemini-2.5-pro-exp-03-25", tools).await
}

/// Calls the 'fast' Gemini model, primarily for simple text generation (no tools used by default).
/// Returns the extracted text directly for convenience in simple cases like chatbot_mentioned.
async fn fast_gemini(system_prompt: &str, prompt: &str) -> Result<String> {
    let mut history = vec![json!({"role": "user", "parts": [{"text": prompt}]})];
    // Call without tools
    let response_json = call_gemini_with_history(system_prompt, &mut history, "gemini-2.5-pro-exp-03-25", None).await?;

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
