use crate::tui::UiEvent;
use serde_json::Value;

/// Execute ask_user — sends a question to the UI and waits for a response.
/// Note: Requires ui_tx (UiEvent sender). Use tools::dispatch in agent.rs which provides this.
pub async fn execute(args: &Value, ui_tx: tokio::sync::mpsc::Sender<UiEvent>) -> Result<String, String> {
    let question = args["question"]
        .as_str()
        .unwrap_or("(no question provided)")
        .to_string();

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<String>();
    ui_tx
        .send(UiEvent::AskUser { question, reply_tx })
        .await
        .map_err(|e| format!("[ask_user: failed to send UI event: {e}]"))?;

    reply_rx
        .await
        .map_err(|_| "[ask_user: no reply received (channel closed)]".to_string())
}

pub fn definition() -> Value {
    serde_json::json!({
        "name": "ask_user",
        "description": "Ask the user a clarifying question. Use only for genuine uncertainty between approaches — not for routine updates.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "Specific, concise question"
                }
            },
            "required": ["question"]
        }
    })
}
