use crate::Ctx;
use anyhow::{anyhow, Context, Result};
use graphviz_comment_reply_lib::{db, graph as graphmod};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::PathBuf;

// -------- tool registry --------

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "list_graphs",
            "List all thought-graphs in memory, newest first.",
            json!({
                "type": "object",
                "properties": {
                    "name_query": { "type": "string", "description": "Optional case-insensitive substring filter on graph name." }
                }
            }),
        ),
        tool(
            "create_graph",
            "Create a new empty thought-graph. Use this to start capturing a new chain of reasoning.",
            json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "Short graph title, unique-ish." },
                    "description": { "type": "string", "description": "One-line purpose of this graph." }
                }
            }),
        ),
        tool(
            "delete_graph",
            "Delete a graph and all its nodes and edges. Destructive.",
            json!({
                "type": "object",
                "required": ["graph"],
                "properties": {
                    "graph": { "type": ["string", "integer"], "description": "Graph name or id." }
                }
            }),
        ),
        tool(
            "get_graph",
            "Return a graph's full contents as Markdown: metadata, every node, and every edge.",
            json!({
                "type": "object",
                "required": ["graph"],
                "properties": {
                    "graph": { "type": ["string", "integer"], "description": "Graph name or id." }
                }
            }),
        ),
        tool(
            "add_node",
            "Add a comment or reply to a graph. Provide `parent_app_id` to attach it as a reply, omit it for a top-level comment.",
            json!({
                "type": "object",
                "required": ["graph", "app_id", "content"],
                "properties": {
                    "graph": { "type": ["string", "integer"] },
                    "app_id": { "type": "string", "description": "Unique within the graph. Choose a short memorable slug." },
                    "content": { "type": "string" },
                    "parent_app_id": { "type": "string", "description": "Existing app_id to reply to. Omit for a root node." }
                }
            }),
        ),
        tool(
            "update_node",
            "Replace a node's content (app_id and edges unchanged).",
            json!({
                "type": "object",
                "required": ["graph", "app_id", "content"],
                "properties": {
                    "graph": { "type": ["string", "integer"] },
                    "app_id": { "type": "string" },
                    "content": { "type": "string" }
                }
            }),
        ),
        tool(
            "delete_node",
            "Delete a node and all its descendants (replies) and incident edges.",
            json!({
                "type": "object",
                "required": ["graph", "app_id"],
                "properties": {
                    "graph": { "type": ["string", "integer"] },
                    "app_id": { "type": "string" }
                }
            }),
        ),
        tool(
            "add_reference",
            "Add a reference edge (kind='ref') from one node to another existing node — the way to introduce cycles for recursive thinking.",
            json!({
                "type": "object",
                "required": ["graph", "from_app_id", "to_app_id"],
                "properties": {
                    "graph": { "type": ["string", "integer"] },
                    "from_app_id": { "type": "string" },
                    "to_app_id": { "type": "string" },
                    "label": { "type": "string", "description": "Optional edge label (e.g. 'depends-on', 'contradicts')." }
                }
            }),
        ),
        tool(
            "search_nodes",
            "Full-text search (FTS5 with BM25 ranking) across node content and app_ids. Returns the best matches with snippets and parent graph names.",
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "FTS5 query string. Supports phrases (\"...\"), prefix (foo*), AND/OR/NOT." },
                    "graph": { "type": ["string", "integer"], "description": "Optional: restrict to one graph." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 }
                }
            }),
        ),
        tool(
            "find_paths",
            "BFS over the directed graph for the shortest path(s) between two app_ids. Treats reply and ref edges uniformly.",
            json!({
                "type": "object",
                "required": ["graph", "from_app_id", "to_app_id"],
                "properties": {
                    "graph": { "type": ["string", "integer"] },
                    "from_app_id": { "type": "string" },
                    "to_app_id": { "type": "string" },
                    "max_paths": { "type": "integer", "default": 5, "minimum": 1, "maximum": 20 }
                }
            }),
        ),
        tool(
            "export_dot",
            "Return the GraphViz DOT source for a graph.",
            json!({
                "type": "object",
                "required": ["graph"],
                "properties": { "graph": { "type": ["string", "integer"] } }
            }),
        ),
        tool(
            "render_graph",
            "Render a graph with the system `dot` binary and return the absolute output path.",
            json!({
                "type": "object",
                "required": ["graph"],
                "properties": {
                    "graph": { "type": ["string", "integer"] },
                    "format": { "type": "string", "enum": ["pdf", "png", "svg"], "default": "pdf" }
                }
            }),
        ),
        tool(
            "stats",
            "Quick stats: total graphs, total nodes, total edges, top graphs by node count.",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

fn tool(name: &str, desc: &str, schema: Value) -> Value {
    json!({ "name": name, "description": desc, "inputSchema": schema })
}

pub fn resource_definitions() -> Vec<Value> {
    vec![
        json!({
            "uri": "thoughtgraph://index",
            "name": "Graph index",
            "description": "List of every graph with id, name, and last-updated timestamp.",
            "mimeType": "application/json"
        }),
    ]
}

// -------- dispatcher --------

pub fn call(ctx: &Ctx, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let text = match name {
        "list_graphs" => do_list_graphs(ctx, &args)?,
        "create_graph" => do_create_graph(ctx, &args)?,
        "delete_graph" => do_delete_graph(ctx, &args)?,
        "get_graph" => do_get_graph(ctx, &args)?,
        "add_node" => do_add_node(ctx, &args)?,
        "update_node" => do_update_node(ctx, &args)?,
        "delete_node" => do_delete_node(ctx, &args)?,
        "add_reference" => do_add_reference(ctx, &args)?,
        "search_nodes" => do_search_nodes(ctx, &args)?,
        "find_paths" => do_find_paths(ctx, &args)?,
        "export_dot" => do_export_dot(ctx, &args)?,
        "render_graph" => do_render_graph(ctx, &args)?,
        "stats" => do_stats(ctx)?,
        other => return Err(anyhow!("unknown tool: {other}")),
    };

    Ok(json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": false
    }))
}

pub fn resource_read(ctx: &Ctx, params: &Value) -> Result<Value> {
    let uri = params
        .get("uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing uri"))?;
    let text = match uri {
        "thoughtgraph://index" => {
            let conn = ctx.conn.lock().unwrap();
            let graphs = db::list_graphs(&conn)?;
            serde_json::to_string_pretty(&graphs)?
        }
        u if u.starts_with("thoughtgraph://graph/") => {
            let key = u.trim_start_matches("thoughtgraph://graph/");
            let conn = ctx.conn.lock().unwrap();
            let g = resolve_graph_value(&conn, &json!(key))?;
            render_graph_markdown(&conn, &g)?
        }
        _ => return Err(anyhow!("unknown resource uri: {uri}")),
    };
    Ok(json!({
        "contents": [
            { "uri": uri, "mimeType": "application/json", "text": text }
        ]
    }))
}

// -------- helpers --------

fn resolve_graph_value(conn: &Connection, v: &Value) -> Result<db::Graph> {
    // Accept either an integer id or a string name.
    if let Some(id) = v.as_i64() {
        return db::graph_by_id(conn, id)?
            .ok_or_else(|| anyhow!("no graph with id {id}"));
    }
    if let Some(s) = v.as_str() {
        if let Ok(id) = s.parse::<i64>() {
            if let Some(g) = db::graph_by_id(conn, id)? {
                return Ok(g);
            }
        }
        return db::graph_by_name(conn, s)?
            .ok_or_else(|| anyhow!("no graph named '{s}'"));
    }
    Err(anyhow!("graph identifier must be a string or integer"))
}

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("missing required string `{key}`"))
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

// -------- tool impls --------

fn do_list_graphs(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let mut gs = db::list_graphs(&conn)?;
    if let Some(q) = optional_str(args, "name_query") {
        let ql = q.to_lowercase();
        gs.retain(|g| g.name.to_lowercase().contains(&ql));
    }
    if gs.is_empty() {
        return Ok("No graphs yet.".into());
    }
    let mut out = format!("{} graph(s):\n\n", gs.len());
    for g in &gs {
        // count nodes per graph quickly
        let nc: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE graph_id = ?1",
            [g.id],
            |r| r.get(0),
        )?;
        out.push_str(&format!(
            "- **{}** (id={}, {} node{}) — updated {}\n  {}\n",
            g.name,
            g.id,
            nc,
            if nc == 1 { "" } else { "s" },
            g.updated_at,
            if g.description.is_empty() {
                "(no description)".into()
            } else {
                g.description.clone()
            }
        ));
    }
    Ok(out)
}

fn do_create_graph(ctx: &Ctx, args: &Value) -> Result<String> {
    let name = require_str(args, "name")?;
    let desc = optional_str(args, "description").unwrap_or("");
    let conn = ctx.conn.lock().unwrap();
    let g = db::create_graph(&conn, name, desc)?;
    Ok(format!(
        "Created graph **{}** (id={}). Add the first node with `add_node`.",
        g.name, g.id
    ))
}

fn do_delete_graph(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    db::delete_graph(&conn, g.id)?;
    Ok(format!("Deleted graph **{}** (id={}).", g.name, g.id))
}

fn do_get_graph(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    render_graph_markdown(&conn, &g)
}

fn render_graph_markdown(conn: &Connection, g: &db::Graph) -> Result<String> {
    let nodes = db::list_nodes(conn, g.id)?;
    let edges = db::list_edges(conn, g.id)?;
    let id_to_app: std::collections::HashMap<i64, String> =
        nodes.iter().map(|n| (n.id, n.app_id.clone())).collect();

    let mut out = String::new();
    out.push_str(&format!("# Graph: {}\n\n", g.name));
    if !g.description.is_empty() {
        out.push_str(&format!("> {}\n\n", g.description));
    }
    out.push_str(&format!(
        "_id={}_, _{} nodes_, _{} edges_, _updated {}_\n\n",
        g.id,
        nodes.len(),
        edges.len(),
        g.updated_at
    ));

    out.push_str("## Nodes\n\n");
    if nodes.is_empty() {
        out.push_str("_(none)_\n\n");
    } else {
        for n in &nodes {
            out.push_str(&format!("### `{}`\n{}\n\n", n.app_id, n.content));
        }
    }

    out.push_str("## Edges\n\n");
    if edges.is_empty() {
        out.push_str("_(none)_\n");
    } else {
        for e in &edges {
            let from = id_to_app.get(&e.from_node_id).cloned().unwrap_or_default();
            let to = id_to_app.get(&e.to_node_id).cloned().unwrap_or_default();
            let arrow = if e.kind == "ref" { "⟲" } else { "↳" };
            let label = if e.label.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.label)
            };
            out.push_str(&format!("- `{}` {} `{}`{}\n", from, arrow, to, label));
        }
    }
    Ok(out)
}

fn do_add_node(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    let app_id = require_str(args, "app_id")?;
    let content = require_str(args, "content")?;
    let parent_id = if let Some(p) = optional_str(args, "parent_app_id") {
        let pn = db::node_by_app_id(&conn, g.id, p)?
            .ok_or_else(|| anyhow!("parent app_id '{}' not found in graph", p))?;
        Some(pn.id)
    } else {
        None
    };
    let n = db::create_node(&conn, g.id, app_id, content, parent_id)?;
    Ok(format!(
        "Added node `{}` (id={}) to graph **{}**{}.",
        n.app_id,
        n.id,
        g.name,
        if parent_id.is_some() {
            " as a reply"
        } else {
            " as a root"
        }
    ))
}

fn do_update_node(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    let app_id = require_str(args, "app_id")?;
    let content = require_str(args, "content")?;
    let n = db::node_by_app_id(&conn, g.id, app_id)?
        .ok_or_else(|| anyhow!("no node with app_id '{}' in graph '{}'", app_id, g.name))?;
    db::update_node(&conn, n.id, content)?;
    Ok(format!("Updated content of `{}` in **{}**.", n.app_id, g.name))
}

fn do_delete_node(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    let app_id = require_str(args, "app_id")?;
    let n = db::node_by_app_id(&conn, g.id, app_id)?
        .ok_or_else(|| anyhow!("no node with app_id '{}'", app_id))?;
    db::delete_node(&conn, n.id)?;
    Ok(format!("Deleted node `{}` from **{}**.", app_id, g.name))
}

fn do_add_reference(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    let from = require_str(args, "from_app_id")?;
    let to = require_str(args, "to_app_id")?;
    let label = optional_str(args, "label").unwrap_or("");
    let fn_ = db::node_by_app_id(&conn, g.id, from)?
        .ok_or_else(|| anyhow!("no `from` node with app_id '{}'", from))?;
    db::add_ref_edge(&conn, g.id, fn_.id, to, label)?;
    Ok(format!(
        "Added ref edge `{}` ⟲ `{}` in **{}**{}.",
        from,
        to,
        g.name,
        if label.is_empty() {
            String::new()
        } else {
            format!(" [{}]", label)
        }
    ))
}

fn do_search_nodes(ctx: &Ctx, args: &Value) -> Result<String> {
    let q = require_str(args, "query")?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;
    let conn = ctx.conn.lock().unwrap();
    let graph_id = if let Some(gv) = args.get("graph") {
        if gv.is_null() {
            None
        } else {
            Some(resolve_graph_value(&conn, gv)?.id)
        }
    } else {
        None
    };
    let hits = db::search_nodes(&conn, q, graph_id, limit)?;
    if hits.is_empty() {
        return Ok(format!("No matches for `{q}`."));
    }
    let mut out = format!("{} match(es) for `{q}`:\n\n", hits.len());
    for h in &hits {
        out.push_str(&format!(
            "- **{}** / `{}` (rank={:.2})\n  {}\n",
            h.graph_name, h.node.app_id, h.rank, h.snippet
        ));
    }
    Ok(out)
}

fn do_find_paths(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    let from = require_str(args, "from_app_id")?;
    let to = require_str(args, "to_app_id")?;
    let max = args
        .get("max_paths")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as usize;
    let paths = graphmod::find_paths(&conn, g.id, from, to, max)?;
    if paths.is_empty() {
        return Ok(format!(
            "No path from `{from}` to `{to}` in **{}**.",
            g.name
        ));
    }
    let mut out = format!(
        "Found {} path(s) from `{from}` to `{to}` in **{}**:\n\n",
        paths.len(),
        g.name
    );
    for (i, p) in paths.iter().enumerate() {
        out.push_str(&format!("**Path {}** ({} step(s)):\n", i + 1, p.nodes.len() - 1));
        for (j, n) in p.nodes.iter().enumerate() {
            if j == 0 {
                out.push_str(&format!("  `{}`", n.app_id));
            } else {
                let kind = &p.edge_kinds[j - 1];
                let arrow = if kind == "ref" { "⟲" } else { "↳" };
                out.push_str(&format!(" {} `{}`", arrow, n.app_id));
            }
        }
        out.push_str("\n\n");
    }
    Ok(out)
}

fn do_export_dot(ctx: &Ctx, args: &Value) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    let nodes = db::list_nodes(&conn, g.id)?;
    let edges = db::list_edges(&conn, g.id)?;
    let dot = graphmod::render_dot(&g.name, &nodes, &edges);
    Ok(format!("```dot\n{}\n```", dot))
}

fn do_render_graph(ctx: &Ctx, args: &Value) -> Result<String> {
    let format = optional_str(args, "format").unwrap_or("pdf").to_string();
    let conn = ctx.conn.lock().unwrap();
    let g = resolve_graph_value(&conn, args.get("graph").unwrap_or(&Value::Null))?;
    let out_dir = export_dir();
    let res = graphmod::render_and_save(&conn, g.id, &g.name, &out_dir, &format)?;
    Ok(format!(
        "Rendered **{}**:\n- DOT: `{}`\n- {}: `{}`",
        g.name,
        res.gv_path,
        format.to_uppercase(),
        res.image_path
    ))
}

fn do_stats(ctx: &Ctx) -> Result<String> {
    let conn = ctx.conn.lock().unwrap();
    let total_graphs: i64 = conn.query_row("SELECT COUNT(*) FROM graphs", [], |r| r.get(0))?;
    let total_nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
    let total_edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
    let ref_edges: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges WHERE kind = 'ref'", [], |r| r.get(0))?;
    let mut stmt = conn.prepare(
        "SELECT g.name, COUNT(n.id) as cnt
         FROM graphs g LEFT JOIN nodes n ON n.graph_id = g.id
         GROUP BY g.id ORDER BY cnt DESC LIMIT 5",
    )?;
    let top: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = format!(
        "**ThoughtGraph stats**\n- graphs: {}\n- nodes: {}\n- edges: {} (of which ref/cycle: {})\n",
        total_graphs, total_nodes, total_edges, ref_edges
    );
    if !top.is_empty() {
        out.push_str("\n**Top graphs by node count:**\n");
        for (name, cnt) in top {
            out.push_str(&format!("- {}: {}\n", name, cnt));
        }
    }
    Ok(out)
}

fn export_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/Application Support/com.chanshunli.thoughtgraph/exports")
}
