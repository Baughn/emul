use crate::db::LogEntry;
use anyhow::{Result, anyhow};
use serde_json::json;

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

pub async fn get_ai_response(
    channel: &str,
    triggering_nick: &str,
    triggering_message: &str,
    history: Vec<LogEntry>,
    prompt_path: &std::path::Path, // Pass the path to read the prompt file
                                   // Add any other context you need, like the API key or client instance
) -> Result<String> {
    tracing::info!(channel, nick = triggering_nick, "AI response requested.");

    // --- Placeholder Logic ---
    // 1. Read the system prompt (Emul's personality) from prompt_path
    let system_prompt = read_prompt_file(prompt_path).await?; // You'll need this

    // 2. Format the history
    let formatted_history = format_history(&history);

    // 3. Construct the full prompt/context for Gemini
    //    Combine system_prompt, formatted_history, triggering_nick, triggering_message
    let full_context = format!(
        "History:\n{}\n\n Current Trigger from {}:\n{}",
        formatted_history, triggering_nick, triggering_message
    );
    tracing::debug!(context_size = full_context.len(), "Constructed AI context");
    tracing::trace!(context = %full_context, "Full AI context");

    // 4. Call the Gemini API with the full context
    let response = call_gemini(&system_prompt, &full_context).await?;
    tracing::info!(response_size = response.len(), "Received AI response");
    tracing::info!(response);
    Ok(response)
}

async fn call_gemini(system_prompt: &str, prompt: &str) -> Result<String> {
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro-exp-03-25:generateContent?key={}", dotenvy::var("GEMINI_API_KEY")?);
    let client = reqwest::Client::new();

    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "text": prompt
            }]
        }],
        "systemInstruction": {
            "parts": [{
                "text": system_prompt
            }]
        },
        "generationConfig": {
            "responseMimeType": "text/plain"
        }
    });

    let response: serde_json::Value = client.post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // The response should be at .candidates[0].content.parts.text, but we'll be defensive.
    let response_text = response
        .get("candidates")
        .and_then(|candidates| candidates.get(0))
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(|parts| parts.get(0))
        .and_then(|part| part.get("text"))
        .and_then(|text| text.as_str())
        .ok_or_else(|| anyhow!("Invalid response from Gemini API"))?;

    Ok(response_text.to_string())
}