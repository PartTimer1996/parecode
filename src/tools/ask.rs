use serde_json::Value;

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
