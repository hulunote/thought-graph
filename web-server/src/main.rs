// ThoughtGraph Web Server
//
// A cross-platform HTTP server that exposes the same SQLite-backed store as the
// Tauri desktop app, so the project can be run on a Windows box (or anywhere
// with a Rust toolchain + a modern browser) without the Tauri/macOS toolchain.
//
// Design choices:
// - tiny_http (sync, single-deps) instead of axum/actix — faster cold compiles
//   on Windows and dramatically smaller dep tree.
// - Single dispatch endpoint POST /api/invoke {cmd, args}. The frontend's
//   Tauri-style invoke(cmd, args) survives the port with minimal diff.
// - Reuses graphviz_comment_reply_lib::{db, graph} verbatim — schema and DOT
//   rendering are identical to the desktop app and the SQLite file is
//   interchangeable.
// - The `dot` binary is resolved cross-platform (Windows: `where dot` and
//   common install dirs; Unix: PATH and the hard-coded /usr/local etc).
// - "Open in default app" doesn't make sense for a web server (may be remote),
//   so render/export return URLs under /exports/<file> for browser download.

use graphviz_comment_reply_lib::db::{self, DbState};
use graphviz_comment_reply_lib::graph;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Request, Response, Server};

// ----------------------------------------------------------------------------
// paths
// ----------------------------------------------------------------------------

fn app_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("THOUGHTGRAPH_DATA_DIR") {
        return PathBuf::from(p);
    }
    if let Some(d) = dirs::data_dir() {
        return d.join("thoughtgraph");
    }
    PathBuf::from(".").join("thoughtgraph-data")
}

fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("THOUGHTGRAPH_DB") {
        return PathBuf::from(p);
    }
    app_data_dir().join("thoughtgraph.sqlite3")
}

fn exports_dir() -> PathBuf {
    app_data_dir().join("exports")
}

fn web_src_dir() -> PathBuf {
    // 1. WEB_SRC_DIR env override (useful in production / packaged installs)
    if let Ok(p) = std::env::var("THOUGHTGRAPH_WEB_SRC") {
        return PathBuf::from(p);
    }
    // 2. ../web-src relative to the binary (cargo run / target/debug)
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            let cand = ancestor.join("web-src");
            if cand.join("index.html").exists() {
                return cand;
            }
        }
    }
    // 3. CWD/web-src (when running from repo root)
    let cwd_cand = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("web-src");
    if cwd_cand.join("index.html").exists() {
        return cwd_cand;
    }
    // 4. Last-ditch sibling of CARGO_MANIFEST_DIR
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("web-src"))
        .unwrap_or_else(|| PathBuf::from("web-src"))
}

// ----------------------------------------------------------------------------
// cross-platform `dot` resolution
//
// graph::which_dot is private to the Tauri crate and only knows about macOS
// install paths. We need our own resolver that also handles Windows so the
// "Render → PDF" button works on the company box.
// ----------------------------------------------------------------------------

fn which_dot() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("DOT_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }

    #[cfg(windows)]
    {
        // Try `where dot` (cmd builtin)
        if let Ok(out) = Command::new("where").arg("dot").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = s.lines().next() {
                    let p = PathBuf::from(line.trim());
                    if p.exists() {
                        return Ok(p);
                    }
                }
            }
        }
        // Common Graphviz install locations on Windows
        for cand in &[
            r"C:\Program Files\Graphviz\bin\dot.exe",
            r"C:\Program Files (x86)\Graphviz\bin\dot.exe",
            r"C:\Graphviz\bin\dot.exe",
        ] {
            let p = PathBuf::from(cand);
            if p.exists() {
                return Ok(p);
            }
        }
    }

    #[cfg(not(windows))]
    {
        for cand in &[
            "/usr/local/bin/dot",
            "/opt/homebrew/bin/dot",
            "/usr/bin/dot",
            "/data/data/com.termux/files/usr/bin/dot",
        ] {
            if Path::new(cand).exists() {
                return Ok(PathBuf::from(cand));
            }
        }
        if let Ok(out) = Command::new("sh").arg("-c").arg("command -v dot").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Ok(PathBuf::from(s));
                }
            }
        }
    }

    Err(anyhow!(
        "graphviz `dot` binary not found. On Windows install from https://graphviz.org/download/, or set the DOT_BIN env var to the full path."
    ))
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn render_graph_to_dir(
    conn: &rusqlite::Connection,
    graph_id: i64,
    graph_name: &str,
    format: &str,
    out_dir: &Path,
) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(out_dir).ok();
    let safe = safe_name(graph_name);
    let gv_path = out_dir.join(format!("{}.gv", safe));
    let img_path = out_dir.join(format!("{}.{}", safe, format));
    graph::export_dot_to_path(conn, graph_id, graph_name, &gv_path)?;
    let dot = which_dot()?;
    let status = Command::new(&dot)
        .arg(format!("-T{}", format))
        .arg(&gv_path)
        .arg("-o")
        .arg(&img_path)
        .status()?;
    if !status.success() {
        return Err(anyhow!("dot exited with status {:?}", status.code()));
    }
    Ok((gv_path, img_path))
}

fn render_paths_to_dir(
    paths: &[graph::PathHit],
    graph_name: &str,
    format: &str,
    out_dir: &Path,
) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(out_dir).ok();
    let dot_src = graph::render_paths_dot(graph_name, paths);
    let safe = safe_name(graph_name);
    let gv_path = out_dir.join(format!("{}_paths.gv", safe));
    let img_path = out_dir.join(format!("{}_paths.{}", safe, format));
    std::fs::write(&gv_path, dot_src.as_bytes())?;
    let dot = which_dot()?;
    let status = Command::new(&dot)
        .arg(format!("-T{}", format))
        .arg(&gv_path)
        .arg("-o")
        .arg(&img_path)
        .status()?;
    if !status.success() {
        return Err(anyhow!("dot exited with status {:?}", status.code()));
    }
    Ok((gv_path, img_path))
}

// ----------------------------------------------------------------------------
// command argument structs
//
// Tauri serialises Rust snake_case as JSON camelCase, and the frontend was
// written to that convention. We keep the same shape so main.js needs only
// the invoke() function swapped — no per-call rewrites.
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateGraphArgs { name: String, description: String }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameGraphArgs { id: i64, name: String, description: String }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphIdArgs { id: i64 }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRefArgs { graph_id: i64 }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateNodeArgs {
    graph_id: i64,
    app_id: String,
    content: String,
    parent_node_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateNodeArgs { node_id: i64, content: String }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeIdArgs { node_id: i64 }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddRefArgs {
    graph_id: i64,
    from_node_id: i64,
    to_app_id: String,
    label: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EdgeIdArgs { edge_id: i64 }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderArgs { graph_id: i64, format: Option<String> }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindPathsArgs {
    graph_id: i64,
    from_app_id: String,
    to_app_id: String,
    max_paths: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindPathsKwArgs {
    graph_id: i64,
    from_keyword: String,
    to_keyword: String,
    max_paths: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderPathsArgs {
    graph_id: i64,
    from_app_id: String,
    to_app_id: String,
    max_paths: Option<usize>,
    format: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderPathsKwArgs {
    graph_id: i64,
    from_keyword: String,
    to_keyword: String,
    max_paths: Option<usize>,
    format: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchArgs {
    graph_id: Option<i64>,
    query: String,
    limit: Option<usize>,
}

// ----------------------------------------------------------------------------
// dispatcher
// ----------------------------------------------------------------------------

fn graph_name_for(conn: &rusqlite::Connection, graph_id: i64) -> Result<String> {
    let g = db::graph_by_id(conn, graph_id)?
        .ok_or_else(|| anyhow!("graph {} not found", graph_id))?;
    Ok(g.name)
}

fn dispatch(state: &DbState, cmd: &str, args: Value) -> Result<Value> {
    let conn = state
        .conn
        .lock()
        .map_err(|e| anyhow!("db mutex poisoned: {}", e))?;

    match cmd {
        "create_graph" => {
            let a: CreateGraphArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(db::create_graph(&conn, &a.name, &a.description)?)?)
        }
        "list_graphs" => Ok(serde_json::to_value(db::list_graphs(&conn)?)?),
        "rename_graph" => {
            let a: RenameGraphArgs = serde_json::from_value(args)?;
            db::rename_graph(&conn, a.id, &a.name, &a.description)?;
            Ok(Value::Null)
        }
        "delete_graph" => {
            let a: GraphIdArgs = serde_json::from_value(args)?;
            db::delete_graph(&conn, a.id)?;
            Ok(Value::Null)
        }
        "create_node" => {
            let a: CreateNodeArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(db::create_node(
                &conn, a.graph_id, &a.app_id, &a.content, a.parent_node_id,
            )?)?)
        }
        "update_node" => {
            let a: UpdateNodeArgs = serde_json::from_value(args)?;
            db::update_node(&conn, a.node_id, &a.content)?;
            Ok(Value::Null)
        }
        "delete_node" => {
            let a: NodeIdArgs = serde_json::from_value(args)?;
            db::delete_node(&conn, a.node_id)?;
            Ok(Value::Null)
        }
        "list_nodes" => {
            let a: GraphRefArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(db::list_nodes(&conn, a.graph_id)?)?)
        }
        "list_edges" => {
            let a: GraphRefArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(db::list_edges(&conn, a.graph_id)?)?)
        }
        "add_ref_edge" => {
            let a: AddRefArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(db::add_ref_edge(
                &conn, a.graph_id, a.from_node_id, &a.to_app_id, &a.label,
            )?)?)
        }
        "delete_edge" => {
            let a: EdgeIdArgs = serde_json::from_value(args)?;
            db::delete_edge(&conn, a.edge_id)?;
            Ok(Value::Null)
        }
        "preview_dot" => {
            let a: GraphRefArgs = serde_json::from_value(args)?;
            let g_name = graph_name_for(&conn, a.graph_id)?;
            let nodes = db::list_nodes(&conn, a.graph_id)?;
            let edges = db::list_edges(&conn, a.graph_id)?;
            Ok(Value::String(graph::render_dot(&g_name, &nodes, &edges)))
        }
        "export_gv" => {
            let a: GraphRefArgs = serde_json::from_value(args)?;
            let g_name = graph_name_for(&conn, a.graph_id)?;
            let safe = safe_name(&g_name);
            let dir = exports_dir();
            std::fs::create_dir_all(&dir).ok();
            let path = dir.join(format!("{}.gv", safe));
            graph::export_dot_to_path(&conn, a.graph_id, &g_name, &path)?;
            Ok(json!({
                "path": path.to_string_lossy(),
                "url": format!("/exports/{}.gv", safe),
                "filename": format!("{}.gv", safe),
            }))
        }
        "render_and_open" => {
            // Web variant: no "open" — return a URL the browser can fetch.
            let a: RenderArgs = serde_json::from_value(args)?;
            let fmt = a.format.unwrap_or_else(|| "pdf".into());
            let g_name = graph_name_for(&conn, a.graph_id)?;
            let (gv, img) = render_graph_to_dir(&conn, a.graph_id, &g_name, &fmt, &exports_dir())?;
            Ok(json!({
                "gv_path": gv.to_string_lossy(),
                "image_path": img.to_string_lossy(),
                "gv_url": format!("/exports/{}", gv.file_name().unwrap().to_string_lossy()),
                "image_url": format!("/exports/{}", img.file_name().unwrap().to_string_lossy()),
            }))
        }
        "open_in_graphviz_app" => {
            // No Graphviz.app on Windows; serve the .gv as a download URL.
            let a: GraphRefArgs = serde_json::from_value(args)?;
            let g_name = graph_name_for(&conn, a.graph_id)?;
            let safe = safe_name(&g_name);
            let dir = exports_dir();
            std::fs::create_dir_all(&dir).ok();
            let path = dir.join(format!("{}.gv", safe));
            graph::export_dot_to_path(&conn, a.graph_id, &g_name, &path)?;
            Ok(json!({
                "path": path.to_string_lossy(),
                "url": format!("/exports/{}.gv", safe),
                "filename": format!("{}.gv", safe),
            }))
        }
        "find_paths" => {
            let a: FindPathsArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(graph::find_paths(
                &conn, a.graph_id, &a.from_app_id, &a.to_app_id, a.max_paths.unwrap_or(10),
            )?)?)
        }
        "find_paths_by_keyword" => {
            let a: FindPathsKwArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(graph::find_paths_by_keyword(
                &conn, a.graph_id, &a.from_keyword, &a.to_keyword, a.max_paths.unwrap_or(50),
            )?)?)
        }
        "render_paths_and_open" => {
            let a: RenderPathsArgs = serde_json::from_value(args)?;
            let fmt = a.format.unwrap_or_else(|| "pdf".into());
            let g_name = graph_name_for(&conn, a.graph_id)?;
            let paths = graph::find_paths(
                &conn, a.graph_id, &a.from_app_id, &a.to_app_id, a.max_paths.unwrap_or(10),
            )?;
            if paths.is_empty() {
                return Err(anyhow!("No paths to render — the two nodes are not connected."));
            }
            let (gv, img) = render_paths_to_dir(&paths, &g_name, &fmt, &exports_dir())?;
            Ok(json!({
                "gv_path": gv.to_string_lossy(),
                "image_path": img.to_string_lossy(),
                "gv_url": format!("/exports/{}", gv.file_name().unwrap().to_string_lossy()),
                "image_url": format!("/exports/{}", img.file_name().unwrap().to_string_lossy()),
            }))
        }
        "render_paths_by_keyword_and_open" => {
            let a: RenderPathsKwArgs = serde_json::from_value(args)?;
            let fmt = a.format.unwrap_or_else(|| "pdf".into());
            let g_name = graph_name_for(&conn, a.graph_id)?;
            let paths = graph::find_paths_by_keyword(
                &conn, a.graph_id, &a.from_keyword, &a.to_keyword, a.max_paths.unwrap_or(50),
            )?;
            if paths.is_empty() {
                return Err(anyhow!("No paths found for those keywords."));
            }
            let (gv, img) = render_paths_to_dir(&paths, &g_name, &fmt, &exports_dir())?;
            Ok(json!({
                "gv_path": gv.to_string_lossy(),
                "image_path": img.to_string_lossy(),
                "gv_url": format!("/exports/{}", gv.file_name().unwrap().to_string_lossy()),
                "image_url": format!("/exports/{}", img.file_name().unwrap().to_string_lossy()),
            }))
        }
        "search_nodes" => {
            let a: SearchArgs = serde_json::from_value(args)?;
            Ok(serde_json::to_value(db::search_nodes(
                &conn, &a.query, a.graph_id, a.limit.unwrap_or(30),
            )?)?)
        }
        other => Err(anyhow!("unknown command: {}", other)),
    }
}

// ----------------------------------------------------------------------------
// HTTP plumbing
// ----------------------------------------------------------------------------

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn json_response(status: u16, body: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(bytes)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
}

fn text_response(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        Some("gv") | Some("dot") => "text/vnd.graphviz; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// Path-traversal-safe join: rel must not contain `..` segments or be absolute.
fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return None;
    }
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            ParentDir | RootDir | Prefix(_) => return None,
            _ => {}
        }
    }
    Some(base.join(p))
}

fn serve_file(req: Request, path: &Path) -> std::io::Result<()> {
    if !path.is_file() {
        return req.respond(text_response(404, "Not Found"));
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return req.respond(text_response(500, &format!("read error: {}", e))),
    };
    let ct = content_type_for(path);
    let resp = Response::from_data(data)
        .with_status_code(200)
        .with_header(header("Content-Type", ct))
        .with_header(header("Cache-Control", "no-cache"));
    req.respond(resp)
}

fn handle_api_invoke(mut req: Request, state: Arc<DbState>) -> std::io::Result<()> {
    let mut body = String::new();
    if let Err(e) = req.as_reader().read_to_string(&mut body) {
        return req.respond(json_response(400, json!({"error": format!("body read: {}", e)})));
    }
    #[derive(Deserialize)]
    struct Envelope {
        cmd: String,
        #[serde(default)]
        args: Value,
    }
    let env: Envelope = match serde_json::from_str(&body) {
        Ok(e) => e,
        Err(e) => return req.respond(json_response(400, json!({"error": format!("bad json: {}", e)}))),
    };
    let result = dispatch(&state, &env.cmd, env.args);
    match result {
        Ok(v) => req.respond(json_response(200, v)),
        Err(e) => {
            eprintln!("[invoke {}] error: {}", env.cmd, e);
            req.respond(json_response(500, json!({"error": e.to_string()})))
        }
    }
}

fn handle_request(req: Request, state: Arc<DbState>, web_dir: PathBuf, exp_dir: PathBuf) {
    let url = req.url().to_string();
    let method = req.method().clone();
    let path = url.split('?').next().unwrap_or("/").to_string();

    eprintln!("[http] {} {}", method.as_str(), path);

    let res: std::io::Result<()> = if method == Method::Post && path == "/api/invoke" {
        handle_api_invoke(req, state)
    } else if method == Method::Get {
        if path == "/" || path == "/index.html" {
            serve_file(req, &web_dir.join("index.html"))
        } else if let Some(rest) = path.strip_prefix("/exports/") {
            match safe_join(&exp_dir, rest) {
                Some(p) => serve_file(req, &p),
                None => req.respond(text_response(400, "Bad path")),
            }
        } else if path == "/healthz" {
            req.respond(text_response(200, "ok"))
        } else {
            // Default: try to serve from web-src
            match safe_join(&web_dir, &path) {
                Some(p) if p.is_file() => serve_file(req, &p),
                _ => req.respond(text_response(404, "Not Found")),
            }
        }
    } else {
        req.respond(text_response(405, "Method Not Allowed"))
    };

    if let Err(e) = res {
        eprintln!("[http] response error: {}", e);
    }
}

// ----------------------------------------------------------------------------
// main
// ----------------------------------------------------------------------------

fn parse_addr() -> SocketAddr {
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8888);
    let bind = format!("{}:{}", host, port);
    bind.parse().unwrap_or_else(|_| "127.0.0.1:8888".parse().unwrap())
}

fn main() {
    let dbp = db_path();
    if let Some(parent) = dbp.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::create_dir_all(exports_dir()).ok();

    let conn = db::init(&dbp).expect("failed to initialise database");
    let state = Arc::new(DbState { conn: Mutex::new(conn) });

    let web_dir = web_src_dir();
    let exp_dir = exports_dir();
    let addr = parse_addr();

    eprintln!("ThoughtGraph Web server");
    eprintln!("  database : {}", dbp.display());
    eprintln!("  exports  : {}", exp_dir.display());
    eprintln!("  web-src  : {}", web_dir.display());
    eprintln!("  listen   : http://{}", addr);
    eprintln!();
    eprintln!("Open http://{} in your browser.", addr);

    let server = Server::http(addr).unwrap_or_else(|e| {
        eprintln!("failed to bind {}: {}", addr, e);
        std::process::exit(1);
    });

    // One worker thread per request — single shared Mutex<Connection>.
    // SQLite + WAL handles the serialisation; this is fine for a single user.
    for req in server.incoming_requests() {
        let s = Arc::clone(&state);
        let w = web_dir.clone();
        let e = exp_dir.clone();
        std::thread::spawn(move || handle_request(req, s, w, e));
    }
}
