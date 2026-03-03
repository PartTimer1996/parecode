use crate::tui::UiEvent;
use serde_json::Value;

/// Execute ask_user — sends a question to the UI and waits for a response.
/// Note: Requires ui_tx (UiEvent sender). Use tools::dispatch in agent.rs which provides this.
pub async fn execute(args: &Value, ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>) -> Result<String, String> {
    let question = args["question"]
        .as_str()
        .unwrap_or("(no question provided)")
        .to_string();

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<String>();
    ui_tx
        .send(UiEvent::AskUser { question, reply_tx })
        .map_err(|e| format!("[ask_user: failed to send UI event: {e}]"))?;

    reply_rx
        .await
        .map_err(|_| "[ask_user: no reply received (channel closed)]".to_string())
}

pub fn definition() -> Value {
    serde_json::json!({
        "name": "ask_user",
        "description": "Ask the user a clarifying question and wait for their answer before proceeding.\n\nUSE THIS BEFORE reading files or exploring when:\n- The task scope is vague (\"review this\", \"improve this\", \"fix the issues\") — ask what specifically to target\n- Multiple files could be affected and you're not sure which — ask which area to focus on\n- There are two valid approaches with different tradeoffs — ask which they prefer\n- You're about to make a large or hard-to-reverse change — confirm the intent\n- The task mentions a concept not visible in the project graph — ask where it lives\n\nA single question saves more tokens than any number of reads. Ask early, not late.\n\nDO NOT ask about: routine implementation details you can decide yourself, things visible in the project graph, things that don't change the outcome.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "A single specific question. If you have multiple unknowns, ask the most important one first."
                }
            },
            "required": ["question"]
        }
    })
}
