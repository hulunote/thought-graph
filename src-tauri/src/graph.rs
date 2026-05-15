use crate::db::{list_edges, list_nodes, node_by_app_id, Edge, Node};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

pub fn render_dot(graph_name: &str, nodes: &[Node], edges: &[Edge]) -> String {
    let mut out = String::new();
    out.push_str(&format!("digraph \"{}\" {{\n", escape_label(graph_name)));
    out.push_str("  rankdir=LR;\n");
    out.push_str("  graph [splines=true, overlap=false, bgcolor=\"#fafafa\"];\n");
    out.push_str("  node  [shape=box, style=\"rounded,filled\", fillcolor=\"#ffffff\", color=\"#888888\", fontname=\"Helvetica\"];\n");
    out.push_str("  edge  [color=\"#888888\", fontname=\"Helvetica\", fontsize=10];\n\n");

    for n in nodes {
        let preview = truncate(&n.content.replace('\n', " "), 80);
        let label = format!("{}\\n{}", escape_label(&n.app_id), escape_label(&preview));
        out.push_str(&format!(
            "  n{} [label=\"{}\", tooltip=\"{}\"];\n",
            n.id,
            label,
            escape_label(&n.content)
        ));
    }
    out.push('\n');

    for e in edges {
        let style = if e.kind == "ref" {
            ", style=dashed, color=\"#cc5555\", constraint=false"
        } else {
            ""
        };
        let label_attr = if e.label.is_empty() {
            String::new()
        } else {
            format!(", label=\"{}\"", escape_label(&e.label))
        };
        out.push_str(&format!(
            "  n{} -> n{} [arrowhead=vee{}{}];\n",
            e.from_node_id, e.to_node_id, style, label_attr
        ));
    }
    out.push_str("}\n");
    out
}

pub fn export_dot_to_path(
    conn: &Connection,
    graph_id: i64,
    graph_name: &str,
    out_path: &PathBuf,
) -> Result<()> {
    let nodes = list_nodes(conn, graph_id)?;
    let edges = list_edges(conn, graph_id)?;
    let dot = render_dot(graph_name, &nodes, &edges);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::File::create(out_path)?;
    f.write_all(dot.as_bytes())?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenderResult {
    pub gv_path: String,
    pub image_path: String,
}

pub fn render_and_save(
    conn: &Connection,
    graph_id: i64,
    graph_name: &str,
    out_dir: &PathBuf,
    format: &str,
) -> Result<RenderResult> {
    std::fs::create_dir_all(out_dir).ok();
    let safe = graph_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();
    let gv_path = out_dir.join(format!("{}.gv", safe));
    let img_path = out_dir.join(format!("{}.{}", safe, format));
    export_dot_to_path(conn, graph_id, graph_name, &gv_path)?;

    let dot_bin = which_dot()?;
    let status = Command::new(&dot_bin)
        .arg(format!("-T{}", format))
        .arg(&gv_path)
        .arg("-o")
        .arg(&img_path)
        .status()?;
    if !status.success() {
        return Err(anyhow!("dot exited with status {:?}", status.code()));
    }
    Ok(RenderResult {
        gv_path: gv_path.to_string_lossy().into_owned(),
        image_path: img_path.to_string_lossy().into_owned(),
    })
}

fn which_dot() -> Result<String> {
    for candidate in &["/usr/local/bin/dot", "/opt/homebrew/bin/dot", "/usr/bin/dot"] {
        if std::path::Path::new(candidate).exists() {
            return Ok((*candidate).to_string());
        }
    }
    // fall back to PATH lookup
    let out = Command::new("sh").arg("-c").arg("command -v dot").output()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    Err(anyhow!(
        "graphviz `dot` not found. Install with `brew install graphviz`."
    ))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PathHit {
    pub nodes: Vec<Node>,
    pub edge_kinds: Vec<String>,
}

// BFS shortest path from `from_app_id` to `to_app_id`, treating all edges as
// directed. Returns up to `max_paths` shortest paths (same length tier).
pub fn find_paths(
    conn: &Connection,
    graph_id: i64,
    from_app_id: &str,
    to_app_id: &str,
    max_paths: usize,
) -> Result<Vec<PathHit>> {
    let start = node_by_app_id(conn, graph_id, from_app_id)?
        .ok_or_else(|| anyhow!("from app_id not found: {}", from_app_id))?;
    let end = node_by_app_id(conn, graph_id, to_app_id)?
        .ok_or_else(|| anyhow!("to app_id not found: {}", to_app_id))?;

    let nodes = list_nodes(conn, graph_id)?;
    let edges = list_edges(conn, graph_id)?;
    let node_map: HashMap<i64, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
    let mut adj: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    for e in &edges {
        adj.entry(e.from_node_id)
            .or_default()
            .push((e.to_node_id, e.kind.clone()));
    }

    // BFS layered, enumerate shortest paths.
    let mut q: VecDeque<Vec<(i64, String)>> = VecDeque::new();
    q.push_back(vec![(start.id, "start".into())]);
    let mut hits: Vec<Vec<(i64, String)>> = Vec::new();
    let mut shortest_len: Option<usize> = None;
    let max_depth = 64usize;

    while let Some(path) = q.pop_front() {
        let (cur, _) = path.last().cloned().unwrap();
        if cur == end.id {
            let l = path.len();
            match shortest_len {
                None => shortest_len = Some(l),
                Some(sl) if l > sl => break,
                _ => {}
            }
            hits.push(path);
            if hits.len() >= max_paths {
                break;
            }
            continue;
        }
        if path.len() >= max_depth {
            continue;
        }
        if let Some(neis) = adj.get(&cur) {
            for (nb, kind) in neis {
                // avoid revisiting within same path (prevents infinite cycle expansion)
                if path.iter().any(|(nid, _)| nid == nb) {
                    continue;
                }
                let mut np = path.clone();
                np.push((*nb, kind.clone()));
                q.push_back(np);
            }
        }
    }

    let result = hits
        .into_iter()
        .map(|p| {
            let mut ns = Vec::with_capacity(p.len());
            let mut ks = Vec::with_capacity(p.len().saturating_sub(1));
            for (i, (nid, kind)) in p.iter().enumerate() {
                if let Some(n) = node_map.get(nid) {
                    ns.push(n.clone());
                }
                if i > 0 {
                    ks.push(kind.clone());
                }
            }
            PathHit {
                nodes: ns,
                edge_kinds: ks,
            }
        })
        .collect();
    Ok(result)
}
