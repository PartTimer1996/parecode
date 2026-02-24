use serde_json::Value;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "recall",
        "description": "Retrieve full output of a previous tool call that was summarised in history.",
        "parameters": {
            "type": "object",
            "properties": {
                "tool_call_id": {
                    "type": "string",
                    "description": "ID of the result to retrieve"
                },
                "tool_name": {
                    "type": "string",
                    "description": "Tool name (fallback — retrieves most recent)"
                }
            }
        }
    })
}
