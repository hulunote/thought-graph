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
    // WAL lets the Tauri app and the MCP server read/write concurrently.
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    conn.pragma_update(None, "synchronous", "NORMAL").ok();
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

        -- Full-text search index over node content + app_id.
        -- `content=` makes this a contentless FTS table that mirrors `nodes`.
        CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
            app_id, content, tokenize = 'unicode61 remove_diacritics 2'
        );

        CREATE TRIGGER IF NOT EXISTS nodes_ai AFTER INSERT ON nodes BEGIN
            INSERT INTO nodes_fts(rowid, app_id, content) VALUES (new.id, new.app_id, new.content);
        END;
        CREATE TRIGGER IF NOT EXISTS nodes_ad AFTER DELETE ON nodes BEGIN
            DELETE FROM nodes_fts WHERE rowid = old.id;
        END;
        CREATE TRIGGER IF NOT EXISTS nodes_au AFTER UPDATE ON nodes BEGIN
            UPDATE nodes_fts SET app_id = new.app_id, content = new.content WHERE rowid = new.id;
        END;
        "#,
    )?;
    // Backfill FTS for any pre-existing nodes (one-time on first upgrade).
    backfill_fts(&conn)?;
    Ok(conn)
}

fn backfill_fts(conn: &Connection) -> Result<()> {
    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes_fts", [], |r| r.get(0))
        .unwrap_or(0);
    let node_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap_or(0);
    if fts_count < node_count {
        conn.execute("DELETE FROM nodes_fts", [])?;
        conn.execute(
            "INSERT INTO nodes_fts(rowid, app_id, content) SELECT id, app_id, content FROM nodes",
            [],
        )?;
    }
    Ok(())
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchHit {
    pub node: Node,
    pub graph_name: String,
    pub snippet: String,
    pub rank: f64,
}

// FTS5 full-text search. If `graph_id` is Some, restrict to that graph.
pub fn search_nodes(
    conn: &Connection,
    query: &str,
    graph_id: Option<i64>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }
    // The fts5 `MATCH` expects a query string; we pass the user's text as-is.
    // snippet(): col=1 is `content`. -1 wraps with [...] markers. 12 = max tokens around match.
    let sql = if graph_id.is_some() {
        "SELECT n.id, n.graph_id, n.app_id, n.content, n.created_at,
                g.name,
                snippet(nodes_fts, 1, '[', ']', '…', 12),
                bm25(nodes_fts)
         FROM nodes_fts
         JOIN nodes n ON n.id = nodes_fts.rowid
         JOIN graphs g ON g.id = n.graph_id
         WHERE nodes_fts MATCH ?1 AND n.graph_id = ?2
         ORDER BY bm25(nodes_fts) ASC
         LIMIT ?3"
    } else {
        "SELECT n.id, n.graph_id, n.app_id, n.content, n.created_at,
                g.name,
                snippet(nodes_fts, 1, '[', ']', '…', 12),
                bm25(nodes_fts)
         FROM nodes_fts
         JOIN nodes n ON n.id = nodes_fts.rowid
         JOIN graphs g ON g.id = n.graph_id
         WHERE nodes_fts MATCH ?1
         ORDER BY bm25(nodes_fts) ASC
         LIMIT ?2"
    };
    let mut stmt = conn.prepare(sql)?;
    let map = |r: &rusqlite::Row| -> rusqlite::Result<SearchHit> {
        Ok(SearchHit {
            node: Node {
                id: r.get(0)?,
                graph_id: r.get(1)?,
                app_id: r.get(2)?,
                content: r.get(3)?,
                created_at: r.get(4)?,
            },
            graph_name: r.get(5)?,
            snippet: r.get(6)?,
            rank: r.get(7)?,
        })
    };
    let rows: Vec<SearchHit> = if let Some(gid) = graph_id {
        stmt.query_map(params![query, gid, limit as i64], map)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(params![query, limit as i64], map)?
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(rows)
}

pub fn graph_by_name(conn: &Connection, name: &str) -> Result<Option<Graph>> {
    let g = conn
        .query_row(
            "SELECT id, name, description, created_at, updated_at FROM graphs WHERE name = ?1",
            params![name],
            |r| {
                Ok(Graph {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(g)
}

pub fn graph_by_id(conn: &Connection, id: i64) -> Result<Option<Graph>> {
    let g = conn
        .query_row(
            "SELECT id, name, description, created_at, updated_at FROM graphs WHERE id = ?1",
            params![id],
            |r| {
                Ok(Graph {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(g)
}

/// Standard on-disk location of the ThoughtGraph SQLite file on macOS.
/// Honours `THOUGHTGRAPH_DB` env var if set (full path to .sqlite3 file).
pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("THOUGHTGRAPH_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/Application Support/com.chanshunli.thoughtgraph/thoughtgraph.sqlite3")
}
