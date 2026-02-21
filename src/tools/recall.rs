use serde_json::Value;

pub fn definition() -> Value {
    serde_json::json!({
        "name": "recall",
        "description": "Retrieve the full output of a previous tool call that was summarised. Use when you need the complete content of an earlier read_file, bash, or search result.",
        "parameters": {
            "type": "object",
            "properties": {
                "tool_call_id": {
                    "type": "string",
                    "description": "The tool_call_id of the result to retrieve (preferred)"
                },
                "tool_name": {
                    "type": "string",
                    "description": "Retrieve the most recent result for this tool name (fallback if tool_call_id unknown)"
                }
            }
        }
    })
}
