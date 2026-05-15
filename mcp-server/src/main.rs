// ThoughtGraph MCP server — JSON-RPC over stdio.
// Protocol: Model Context Protocol 2024-11-05.
//
// Claude Desktop spawns this binary as a child process. We exchange newline-
// delimited JSON-RPC 2.0 messages on stdin/stdout. Logs (anything not protocol
// traffic) MUST go to stderr; writing them to stdout would corrupt the stream.

mod tools;

use anyhow::Result;
use graphviz_comment_reply_lib::db;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{BufRead, BufWriter, Write};
use std::sync::Mutex;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "thoughtgraph-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Ctx {
    pub conn: Mutex<Connection>,
}

fn main() -> Result<()> {
    let db_path = db::default_db_path();
    eprintln!("[thoughtgraph-mcp] db = {}", db_path.display());
    let conn = db::init(&db_path)?;
    let ctx = Ctx {
        conn: Mutex::new(conn),
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[thoughtgraph-mcp] stdin error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[thoughtgraph-mcp] parse error: {e}: {line}");
                continue;
            }
        };
        if let Some(resp) = handle(&ctx, &msg) {
            let s = serde_json::to_string(&resp)?;
            writeln!(out, "{s}")?;
            out.flush()?;
        }
    }
    Ok(())
}

fn handle(ctx: &Ctx, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no id) get no response.
    if id.is_none() {
        match method {
            "notifications/initialized" => {}
            "notifications/cancelled" => {}
            _ => eprintln!("[thoughtgraph-mcp] unknown notification: {method}"),
        }
        return None;
    }

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "listChanged": false, "subscribe": false }
            },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::definitions() })),
        "tools/call" => tools::call(ctx, &params).map_err(|e| (-32000, e.to_string())),
        "resources/list" => Ok(json!({ "resources": tools::resource_definitions() })),
        "resources/read" => {
            tools::resource_read(ctx, &params).map_err(|e| (-32000, e.to_string()))
        }
        "prompts/list" => Ok(json!({ "prompts": [] })),
        _ => Err((-32601, format!("Method not found: {method}"))),
    };

    let body = match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    };
    Some(body)
}
