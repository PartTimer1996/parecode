use serde_json::Value;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "ask_user",
        "description": "Ask the user a clarifying question and wait for their response. Use this when you are uncertain about the correct approach, need to choose between multiple valid alternatives, or want to confirm an assumption before proceeding with a potentially costly or irreversible action. Do NOT use this for routine progress updates — only for genuine decisions that affect the outcome.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user. Be specific and concise. If presenting options, list them clearly."
                }
            },
            "required": ["question"]
        }
    })
}
