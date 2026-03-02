use serde_json::Value;
use crate::history::History;

/// Execute recall — retrieves full output from history by tool_call_id or tool_name.
/// Note: Requires &History reference. Use tools::dispatch in agent.rs which provides this.
pub fn execute(args: &Value, history: &History) -> Result<String, String> {
    // Try by tool_call_id first, then by tool_name
    if let Some(id) = args["tool_call_id"].as_str() {
        if let Some(full) = history.recall(id) {
            return Ok(full.to_string());
        }
    }

    if let Some(name) = args["tool_name"].as_str() {
        if let Some(full) = history.recall_by_name(name) {
            return Ok(full.to_string());
        }
    }

    Err("[recall: no matching tool result found]".to_string())
}
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
