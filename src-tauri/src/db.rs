use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct DbState {
    pub conn: Mutex<Connection>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Graph {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Node {
    pub id: i64,
    pub graph_id: i64,
    pub app_id: String,
    pub content: String,
    pub created_at: String,
}

// kind: "reply" (parent comment -> child reply) or "ref" (reference, may form cycle)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Edge {
    pub id: i64,
    pub graph_id: i64,
    pub from_node_id: i64,
    pub to_node_id: i64,
    pub kind: String,
    pub label: String,
}

pub fn init(path: &PathBuf) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS graphs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS nodes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            graph_id INTEGER NOT NULL REFERENCES graphs(id) ON DELETE CASCADE,
            app_id TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE(graph_id, app_id)
        );
        CREATE TABLE IF NOT EXISTS edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            graph_id INTEGER NOT NULL REFERENCES graphs(id) ON DELETE CASCADE,
            from_node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            to_node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            label TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_nodes_graph ON nodes(graph_id);
        CREATE INDEX IF NOT EXISTS idx_edges_graph ON edges(graph_id);
        CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_node_id);
        CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_node_id);
        "#,
    )?;
    Ok(conn)
}

pub fn now() -> String {
    Utc::now().to_rfc3339()
}

pub fn create_graph(conn: &Connection, name: &str, description: &str) -> Result<Graph> {
    let ts = now();
    conn.execute(
        "INSERT INTO graphs (name, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![name, description, ts],
    )?;
    let id = conn.last_insert_rowid();
    Ok(Graph {
        id,
        name: name.to_string(),
        description: description.to_string(),
        created_at: ts.clone(),
        updated_at: ts,
    })
}

pub fn list_graphs(conn: &Connection) -> Result<Vec<Graph>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, created_at, updated_at FROM graphs ORDER BY updated_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Graph {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn delete_graph(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM graphs WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn rename_graph(conn: &Connection, id: i64, name: &str, description: &str) -> Result<()> {
    conn.execute(
        "UPDATE graphs SET name = ?1, description = ?2, updated_at = ?3 WHERE id = ?4",
        params![name, description, now(), id],
    )?;
    Ok(())
}

fn touch_graph(conn: &Connection, graph_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE graphs SET updated_at = ?1 WHERE id = ?2",
        params![now(), graph_id],
    )?;
    Ok(())
}

pub fn node_by_app_id(conn: &Connection, graph_id: i64, app_id: &str) -> Result<Option<Node>> {
    let n = conn
        .query_row(
            "SELECT id, graph_id, app_id, content, created_at FROM nodes WHERE graph_id = ?1 AND app_id = ?2",
            params![graph_id, app_id],
            |r| {
                Ok(Node {
                    id: r.get(0)?,
                    graph_id: r.get(1)?,
                    app_id: r.get(2)?,
                    content: r.get(3)?,
                    created_at: r.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(n)
}

pub fn create_node(
    conn: &Connection,
    graph_id: i64,
    app_id: &str,
    content: &str,
    parent_node_id: Option<i64>,
) -> Result<Node> {
    if app_id.trim().is_empty() {
        return Err(anyhow!("app_id cannot be empty"));
    }
    if node_by_app_id(conn, graph_id, app_id)?.is_some() {
        return Err(anyhow!(
            "app_id '{}' already exists in this graph",
            app_id
        ));
    }
    let ts = now();
    conn.execute(
        "INSERT INTO nodes (graph_id, app_id, content, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![graph_id, app_id, content, ts],
    )?;
    let id = conn.last_insert_rowid();
    if let Some(pid) = parent_node_id {
        conn.execute(
            "INSERT INTO edges (graph_id, from_node_id, to_node_id, kind, label) VALUES (?1, ?2, ?3, 'reply', '')",
            params![graph_id, pid, id],
        )?;
    }
    touch_graph(conn, graph_id)?;
    Ok(Node {
        id,
        graph_id,
        app_id: app_id.to_string(),
        content: content.to_string(),
        created_at: ts,
    })
}

pub fn update_node(conn: &Connection, node_id: i64, content: &str) -> Result<()> {
    let graph_id: i64 = conn.query_row(
        "SELECT graph_id FROM nodes WHERE id = ?1",
        params![node_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "UPDATE nodes SET content = ?1 WHERE id = ?2",
        params![content, node_id],
    )?;
    touch_graph(conn, graph_id)?;
    Ok(())
}

pub fn delete_node(conn: &Connection, node_id: i64) -> Result<()> {
    let graph_id: i64 = conn.query_row(
        "SELECT graph_id FROM nodes WHERE id = ?1",
        params![node_id],
        |r| r.get(0),
    )?;
    conn.execute("DELETE FROM nodes WHERE id = ?1", params![node_id])?;
    touch_graph(conn, graph_id)?;
    Ok(())
}

pub fn list_nodes(conn: &Connection, graph_id: i64) -> Result<Vec<Node>> {
    let mut stmt = conn.prepare(
        "SELECT id, graph_id, app_id, content, created_at FROM nodes WHERE graph_id = ?1 ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map(params![graph_id], |r| {
            Ok(Node {
                id: r.get(0)?,
                graph_id: r.get(1)?,
                app_id: r.get(2)?,
                content: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn list_edges(conn: &Connection, graph_id: i64) -> Result<Vec<Edge>> {
    let mut stmt = conn.prepare(
        "SELECT id, graph_id, from_node_id, to_node_id, kind, label FROM edges WHERE graph_id = ?1",
    )?;
    let rows = stmt
        .query_map(params![graph_id], |r| {
            Ok(Edge {
                id: r.get(0)?,
                graph_id: r.get(1)?,
                from_node_id: r.get(2)?,
                to_node_id: r.get(3)?,
                kind: r.get(4)?,
                label: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn add_ref_edge(
    conn: &Connection,
    graph_id: i64,
    from_node_id: i64,
    to_app_id: &str,
    label: &str,
) -> Result<Edge> {
    let target = node_by_app_id(conn, graph_id, to_app_id)?
        .ok_or_else(|| anyhow!("No node with app_id '{}' in this graph", to_app_id))?;
    conn.execute(
        "INSERT INTO edges (graph_id, from_node_id, to_node_id, kind, label) VALUES (?1, ?2, ?3, 'ref', ?4)",
        params![graph_id, from_node_id, target.id, label],
    )?;
    let id = conn.last_insert_rowid();
    touch_graph(conn, graph_id)?;
    Ok(Edge {
        id,
        graph_id,
        from_node_id,
        to_node_id: target.id,
        kind: "ref".into(),
        label: label.into(),
    })
}

pub fn delete_edge(conn: &Connection, edge_id: i64) -> Result<()> {
    let graph_id: i64 = conn.query_row(
        "SELECT graph_id FROM edges WHERE id = ?1",
        params![edge_id],
        |r| r.get(0),
    )?;
    conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_id])?;
    touch_graph(conn, graph_id)?;
    Ok(())
}
