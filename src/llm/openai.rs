use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f32,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Option<Vec<Choice>>,
    error: Option<OpenAIError>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct OpenAIError {
    message: String,
}

pub async fn complete(
    http: &Client,
    model: &str,
    api_key: &str,
    system: &str,
    user: &str,
    temperature: f32,
) -> Result<String> {
    let url = "https://api.openai.com/v1/chat/completions";

    let body = ChatRequest {
        model: model.to_owned(),
        messages: vec![
            Message { role: "system".to_owned(), content: system.to_owned() },
            Message { role: "user".to_owned(), content: user.to_owned() },
        ],
        temperature,
    };

    let resp = http
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .context("OpenAI HTTP request failed")?;

    let status = resp.status();
    let text = resp.text().await.context("failed to read OpenAI response body")?;

    if !status.is_success() {
        bail!("OpenAI API returned HTTP {status}: {text}");
    }

    let parsed: ChatResponse = serde_json::from_str(&text)
        .context("failed to parse OpenAI response JSON")?;

    if let Some(err) = parsed.error {
        bail!("OpenAI API error: {}", err.message);
    }

    let result_text = parsed.choices
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.message.content)
        .unwrap_or_default();

    Ok(result_text)
}
