use crate::db::LogEntry;
use anyhow::{anyhow, bail, Result};
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

/// For a less obvious mention such as "I wonder what Emul thinks", this does a cheap check to see if Emul ought to respond.
pub async fn chatbot_mentioned(
    chatbot_name: &str,
    triggering_message: &str,
) -> Result<bool> {
    let system_prompt = format!("You are {}. Check if the provided message is aimed at {}, or if it is merely a mention. Respond with a single word, \"respond\" or \"mention\".", chatbot_name, chatbot_name);
    let response = fast_gemini(&system_prompt, triggering_message).await?;
    tracing::trace!(response = %response, message = %triggering_message);
    if response.contains("respond") {
        Ok(true)
    } else if response.contains("mention") {
        Ok(false)
    } else {
        bail!("chatbot_mentioned failed to parse response")
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

    // 1. Read the system prompt (Emul's personality) from prompt_path
    let system_prompt = read_prompt_file(prompt_path).await?;

    // 2. Format the history
    let history = if was_addressed {
        history
    } else {
        let mut history = history.clone();
        history.push(LogEntry {
            channel: channel.to_string(),
            nick: triggering_nick.to_string(),
            message: triggering_message.to_string(),
        });
        history
    };
    let formatted_history = format_history(&history);

    // 3. Construct the full prompt/context for Gemini
    //    Combine system_prompt, formatted_history, triggering_nick, triggering_message
    let full_context = if was_addressed {
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
    tracing::debug!(context_size = full_context.len(), "Constructed AI context");
    tracing::trace!(context_lines = %full_context.lines().count(), "Context size");

    // 4. Call the Gemini API with the full context
    let response = smart_gemini(&system_prompt, &full_context).await?;
    tracing::info!(response_size = response.len(), "Received AI response");
    tracing::info!(response);
    Ok(response)
}

async fn call_gemini(system_prompt: &str, prompt: &str, model_version: &str) -> Result<String> {
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}", model_version, dotenvy::var("GEMINI_API_KEY")?);
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

async fn smart_gemini(system_prompt: &str, prompt: &str) -> Result<String> {
    call_gemini(system_prompt, prompt, "gemini-2.5-pro-exp-03-25").await
}

async fn fast_gemini(system_prompt: &str, prompt: &str) -> Result<String> {
    call_gemini(system_prompt, prompt, "gemini-2.5-pro-exp-03-25").await
}
