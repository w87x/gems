//! JSON-RPC 2.0 envelope handling: request routing, response/error
//! framing, and the MCP-specific `initialize`/`tools/list`/`tools/call`
//! methods. Tool execution itself lives in `tools.rs`.

use gems_json::Value;

use crate::tools;

const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Handle one JSON-RPC request line, returning the response line to write
/// (or `None` for a notification, which per JSON-RPC has no `id` and
/// expects no response).
pub fn dispatch(line: &str) -> Option<String> {
    let request = match gems_json::parse(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(Value::Null, PARSE_ERROR, &e.to_string()).to_string())
        }
    };

    let has_id = request.get("id").is_some();
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");

    if !has_id {
        // A notification: MCP/JSON-RPC clients send these (e.g.
        // "notifications/initialized") expecting no reply at all.
        return None;
    }

    let response = match method {
        "initialize" => success_response(id, initialize_result()),
        "tools/list" => success_response(id, tools_list_result()),
        "tools/call" => match request.get("params") {
            Some(params) => success_response(id, handle_tools_call(params)),
            None => error_response(id, INVALID_PARAMS, "tools/call requires params"),
        },
        other => error_response(id, METHOD_NOT_FOUND, &format!("unknown method: {other}")),
    };
    Some(response.to_string())
}

fn success_response(id: Value, result: Value) -> Value {
    let mut v = Value::object();
    v.set("jsonrpc", "2.0");
    v.set("id", id);
    v.set("result", result);
    v
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    let mut error = Value::object();
    error.set("code", code);
    error.set("message", message);
    let mut v = Value::object();
    v.set("jsonrpc", "2.0");
    v.set("id", id);
    v.set("error", error);
    v
}

fn initialize_result() -> Value {
    let mut capabilities = Value::object();
    capabilities.set("tools", Value::object());

    let mut server_info = Value::object();
    server_info.set("name", "gems-mcp");
    server_info.set("version", env!("CARGO_PKG_VERSION"));

    let mut result = Value::object();
    result.set("protocolVersion", "2024-11-05");
    result.set("capabilities", capabilities);
    result.set("serverInfo", server_info);
    result
}

fn tool_schema(name: &str, description: &str, properties: Value, required: Vec<&str>) -> Value {
    let mut schema = Value::object();
    schema.set("type", "object");
    schema.set("properties", properties);
    schema.set(
        "required",
        required.into_iter().map(String::from).collect::<Vec<_>>(),
    );

    let mut tool = Value::object();
    tool.set("name", name);
    tool.set("description", description);
    tool.set("inputSchema", schema);
    tool
}

fn string_prop(description: &str) -> Value {
    let mut p = Value::object();
    p.set("type", "string");
    p.set("description", description);
    p
}

fn tools_list_result() -> Value {
    let mut query_props = Value::object();
    query_props.set("store_dir", string_prop("Path to the store directory"));
    query_props.set(
        "query",
        string_prop("A SELECT ... FROM entities ... query string"),
    );
    query_props.set(
        "subject",
        string_prop("Optional: acting subject's hex id, for ABAC-enforced results"),
    );

    let mut get_entity_props = Value::object();
    get_entity_props.set("store_dir", string_prop("Path to the store directory"));
    get_entity_props.set("id", string_prop("Entity id, as 48 hex characters"));
    get_entity_props.set(
        "subject",
        string_prop("Optional: acting subject's hex id, for ABAC-enforced results"),
    );

    let mut list_types_props = Value::object();
    list_types_props.set("store_dir", string_prop("Path to the store directory"));

    let tools = vec![
        tool_schema(
            "query",
            "Run a SQL-subset query against an entity store",
            query_props,
            vec!["store_dir", "query"],
        ),
        tool_schema(
            "get_entity",
            "Fetch one entity by id",
            get_entity_props,
            vec!["store_dir", "id"],
        ),
        tool_schema(
            "list_entity_types",
            "List every EntityType defined in a store",
            list_types_props,
            vec!["store_dir"],
        ),
    ];

    let mut result = Value::object();
    result.set("tools", tools);
    result
}

fn handle_tools_call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str);
    let empty_args = Value::object();
    let args = params.get("arguments").unwrap_or(&empty_args);

    let tool_result = match name {
        Some("query") => tools::query(args),
        Some("get_entity") => tools::get_entity(args),
        Some("list_entity_types") => tools::list_entity_types(args),
        Some(other) => Err(format!("unknown tool: {other}")),
        None => Err("tools/call requires a tool name".to_string()),
    };

    let mut text_block = Value::object();
    text_block.set("type", "text");

    let mut result = Value::object();
    match tool_result {
        Ok(value) => {
            text_block.set("text", value.to_string());
            result.set("content", vec![text_block]);
        }
        Err(message) => {
            text_block.set("text", message);
            result.set("content", vec![text_block]);
            result.set("isError", true);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_without_id_gets_no_response() {
        assert_eq!(
            dispatch(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
    }

    #[test]
    fn unknown_method_is_a_json_rpc_error() {
        let response = dispatch(r#"{"jsonrpc":"2.0","id":1,"method":"bogus"}"#).unwrap();
        let parsed = gems_json::parse(&response).unwrap();
        assert_eq!(parsed.get("id"), Some(&Value::Number(1.0)));
        assert!(parsed.get("error").is_some());
        assert_eq!(
            parsed.get("error").unwrap().get("code"),
            Some(&Value::Number(METHOD_NOT_FOUND as f64))
        );
    }

    #[test]
    fn initialize_returns_server_info() {
        let response = dispatch(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        let parsed = gems_json::parse(&response).unwrap();
        let result = parsed.get("result").unwrap();
        assert_eq!(
            result
                .get("serverInfo")
                .unwrap()
                .get("name")
                .unwrap()
                .as_str(),
            Some("gems-mcp")
        );
    }

    #[test]
    fn tools_list_returns_three_tools() {
        let response = dispatch(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        let parsed = gems_json::parse(&response).unwrap();
        let tools = parsed.get("result").unwrap().get("tools").unwrap();
        assert_eq!(tools.as_array().unwrap().len(), 3);
    }

    #[test]
    fn tools_call_with_unknown_tool_is_a_content_level_error_not_a_protocol_error() {
        let response = dispatch(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        )
        .unwrap();
        let parsed = gems_json::parse(&response).unwrap();
        assert!(
            parsed.get("error").is_none(),
            "must be a normal JSON-RPC result"
        );
        let result = parsed.get("result").unwrap();
        assert_eq!(result.get("isError"), Some(&Value::Bool(true)));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let response = dispatch("{not json").unwrap();
        let parsed = gems_json::parse(&response).unwrap();
        assert_eq!(
            parsed.get("error").unwrap().get("code"),
            Some(&Value::Number(PARSE_ERROR as f64))
        );
    }
}
