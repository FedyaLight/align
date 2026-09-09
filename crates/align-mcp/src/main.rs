//! Small stdio MCP server backed by the adjacent `align-cli` executable.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Map, Value, json};

const LATEST_PROTOCOL: &str = "2025-11-25";
const SUPPORTED_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", LATEST_PROTOCOL];

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::BufWriter::new(std::io::stdout());
    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(request) => handle_request(&request),
                Err(error) => Some(rpc_error(
                    Value::Null,
                    -32700,
                    format!("Parse error: {error}"),
                )),
            },
            Err(error) => {
                eprintln!("align-mcp: stdin: {error}");
                break;
            }
        };
        if let Some(response) = response {
            if serde_json::to_writer(&mut stdout, &response).is_err()
                || stdout.write_all(b"\n").is_err()
                || stdout.flush().is_err()
            {
                break;
            }
        }
    }
}

fn handle_request(request: &Value) -> Option<Value> {
    let object = match request.as_object() {
        Some(object) => object,
        None => return Some(rpc_error(Value::Null, -32600, "Invalid Request")),
    };
    let id = object.get("id")?.clone();
    let method = object.get("method").and_then(Value::as_str);
    let params = object.get("params").unwrap_or(&Value::Null);
    let result = match method {
        Some("initialize") => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(LATEST_PROTOCOL);
            let protocol = if SUPPORTED_PROTOCOLS.contains(&requested) {
                requested
            } else {
                LATEST_PROTOCOL
            };
            Ok(json!({
                "protocolVersion": protocol,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "align",
                    "title": "Align Media Synchronizer",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Inspect media, synchronize recordings, and export editor timelines. Use absolute filesystem paths."
            }))
        }
        Some("ping") => Ok(json!({})),
        Some("tools/list") => Ok(json!({ "tools": tools() })),
        Some("tools/call") => call_tool(params),
        _ => Err((-32601, "Method not found".to_string())),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error(id, code, message),
    })
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "align_inspect",
            "title": "Inspect media and timelines",
            "description": "Inspect media files, folders, XML, FCPXML, or AAF projects before synchronization.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "media_paths": path_array("Absolute paths to media, folders, or timeline projects.")
                },
                "required": ["media_paths"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "align_sync",
            "title": "Synchronize media",
            "description": "Synchronize recordings by waveform and timing evidence. Returns an Align SyncResult as JSON.",
            "inputSchema": sync_schema(false)
        }),
        json!({
            "name": "align_export",
            "title": "Synchronize and export timelines",
            "description": "Synchronize media and write timelines for video editors into an existing output directory.",
            "inputSchema": sync_schema(true)
        }),
    ]
}

fn path_array(description: &str) -> Value {
    json!({
        "type": "array",
        "description": description,
        "items": { "type": "string" },
        "minItems": 1
    })
}

fn sync_schema(export: bool) -> Value {
    let mut properties = Map::new();
    properties.insert(
        "media_paths".into(),
        path_array("Absolute paths to media, folders, or timeline projects."),
    );
    properties.insert(
        "sequence".into(),
        json!({ "type": "integer", "minimum": 1, "description": "One-based sequence number from a timeline project." }),
    );
    properties.insert(
        "all_sequences".into(),
        json!({ "type": "boolean", "default": false }),
    );
    properties.insert(
        "search_accuracy".into(),
        json!({ "type": "string", "enum": ["fast", "balanced", "thorough", "deep", "exhaustive"], "default": "balanced" }),
    );
    properties.insert(
        "time_source".into(),
        json!({ "type": "string", "enum": ["auto", "rec-start", "rec-stop", "timecode"], "default": "auto" }),
    );
    properties.insert(
        "match_threshold".into(),
        json!({ "type": "string", "enum": ["permissive", "balanced", "conservative"], "default": "balanced" }),
    );
    properties.insert(
        "clip_order".into(),
        json!({ "type": "string", "enum": ["auto", "alternate-auto", "as-imported", "by-date-time", "by-file-name", "ignore"], "default": "auto" }),
    );
    properties.insert(
        "track_content".into(),
        json!({ "type": "string", "enum": ["auto", "linear", "takes"], "default": "auto" }),
    );
    if export {
        properties.insert(
            "output_directory".into(),
            json!({ "type": "string", "description": "Absolute path to an existing output directory." }),
        );
        properties.insert(
            "correct_drift".into(),
            json!({ "type": "boolean", "default": true }),
        );
        properties.insert(
            "export_media".into(),
            json!({ "type": "boolean", "default": false, "description": "Also render media files with external audio." }),
        );
        properties.insert(
            "aaf_only".into(),
            json!({ "type": "boolean", "default": false, "description": "Export AAF instead of the default editor timeline formats." }),
        );
    }
    let required = if export {
        json!(["media_paths", "output_directory"])
    } else {
        json!(["media_paths"])
    };
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn call_tool(params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "tools/call requires a tool name".to_string()))?;
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let args = match name {
        "align_inspect" => media_paths(&arguments),
        "align_sync" => sync_args(&arguments, false),
        "align_export" => sync_args(&arguments, true),
        _ => return Err((-32602, format!("Unknown tool: {name}"))),
    };
    let args = match args {
        Ok(args) => args,
        Err(message) => return Ok(tool_error(message)),
    };
    Ok(run_cli(args))
}

fn sync_args(arguments: &Map<String, Value>, export: bool) -> Result<Vec<String>, String> {
    let mut args = vec![if export { "export" } else { "sync" }.to_string()];
    if let Some(sequence) = arguments.get("sequence") {
        let sequence = sequence
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| "sequence must be a positive integer".to_string())?;
        args.extend(["--sequence".into(), sequence.to_string()]);
    }
    push_bool_flag(arguments, "all_sequences", "--all-sequences", &mut args)?;
    for (field, flag, allowed) in [
        (
            "search_accuracy",
            "--search-accuracy",
            &["fast", "balanced", "thorough", "deep", "exhaustive"][..],
        ),
        (
            "time_source",
            "--time-source",
            &["auto", "rec-start", "rec-stop", "timecode"][..],
        ),
        (
            "match_threshold",
            "--match-threshold",
            &["permissive", "balanced", "conservative"][..],
        ),
        (
            "clip_order",
            "--clip-order",
            &[
                "auto",
                "alternate-auto",
                "as-imported",
                "by-date-time",
                "by-file-name",
                "ignore",
            ][..],
        ),
        (
            "track_content",
            "--track-content",
            &["auto", "linear", "takes"][..],
        ),
    ] {
        push_enum(arguments, field, flag, allowed, &mut args)?;
    }
    if export {
        if arguments.get("correct_drift") == Some(&Value::Bool(false)) {
            args.push("--no-drift".into());
        }
        push_bool_flag(arguments, "export_media", "--export-media", &mut args)?;
        push_bool_flag(arguments, "aaf_only", "--aaf", &mut args)?;
        let output = arguments
            .get("output_directory")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "output_directory is required".to_string())?;
        args.push(output.to_string());
    }
    args.extend(media_paths(arguments)?);
    Ok(args)
}

fn media_paths(arguments: &Map<String, Value>) -> Result<Vec<String>, String> {
    let paths = arguments
        .get("media_paths")
        .and_then(Value::as_array)
        .ok_or_else(|| "media_paths must be a non-empty array".to_string())?;
    if paths.is_empty() {
        return Err("media_paths must be a non-empty array".into());
    }
    paths
        .iter()
        .map(|path| {
            path.as_str()
                .filter(|path| !path.is_empty())
                .map(str::to_string)
                .ok_or_else(|| "every media path must be a non-empty string".to_string())
        })
        .collect()
}

fn push_bool_flag(
    arguments: &Map<String, Value>,
    field: &str,
    flag: &str,
    args: &mut Vec<String>,
) -> Result<(), String> {
    match arguments.get(field) {
        Some(Value::Bool(true)) => args.push(flag.into()),
        Some(Value::Bool(false)) | None => {}
        Some(_) => return Err(format!("{field} must be a boolean")),
    }
    Ok(())
}

fn push_enum(
    arguments: &Map<String, Value>,
    field: &str,
    flag: &str,
    allowed: &[&str],
    args: &mut Vec<String>,
) -> Result<(), String> {
    let Some(value) = arguments.get(field) else {
        return Ok(());
    };
    let value = value
        .as_str()
        .filter(|value| allowed.contains(value))
        .ok_or_else(|| format!("{field} must be one of: {}", allowed.join(", ")))?;
    args.extend([flag.into(), value.into()]);
    Ok(())
}

fn cli_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ALIGN_CLI") {
        return path.into();
    }
    let name = if cfg!(windows) {
        "align-cli.exe"
    } else {
        "align-cli"
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn run_cli(args: Vec<String>) -> Value {
    let output = match Command::new(cli_path()).args(&args).output() {
        Ok(output) => output,
        Err(error) => return tool_error(format!("Could not start align-cli: {error}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let message = stderr.trim().trim_start_matches("error: ");
        return tool_error(if message.is_empty() {
            format!("align-cli exited with {}", output.status)
        } else {
            message.chars().take(16_000).collect()
        });
    }
    match serde_json::from_str::<Value>(&stdout) {
        Ok(data) => json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&data).unwrap_or_default() }],
            "structuredContent": data,
            "isError": false
        }),
        Err(error) => tool_error(format!("align-cli returned invalid JSON: {error}")),
    }
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_negotiates_a_supported_protocol() {
        let response = handle_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }))
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["result"]["capabilities"], json!({ "tools": {} }));
    }

    #[test]
    fn export_arguments_map_to_the_cli() {
        let arguments = json!({
            "media_paths": ["/media/a.wav", "/media/b.wav"],
            "output_directory": "/output",
            "search_accuracy": "thorough",
            "correct_drift": false,
            "export_media": true
        });
        let args = sync_args(arguments.as_object().unwrap(), true).unwrap();
        assert_eq!(
            args,
            [
                "export",
                "--search-accuracy",
                "thorough",
                "--no-drift",
                "--export-media",
                "/output",
                "/media/a.wav",
                "/media/b.wav"
            ]
        );
    }

    #[test]
    fn unknown_tools_are_protocol_errors() {
        let response = handle_request(&json!({
            "jsonrpc": "2.0",
            "id": "x",
            "method": "tools/call",
            "params": { "name": "missing" }
        }))
        .unwrap();
        assert_eq!(response["error"]["code"], -32602);
    }
}
