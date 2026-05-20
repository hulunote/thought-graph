#!/usr/bin/env python3
# ThoughtGraph — pure-Python web edition.
#
# Why this exists: the Rust web-server cannot be compiled on locked-down
# corporate Windows boxes (no MSVC toolchain / AV blocking link.exe / no
# permission to create .exe). Python ships pre-installed (or via Microsoft
# Store) on virtually every Windows machine, runs from source without
# compilation, and produces no binary artifact.
#
# Functionally this mirrors web-server/src/main.rs:
#   - same SQLite schema (graphs / nodes / edges + nodes_fts FTS5 + triggers)
#   - same DOT rendering (STHeiti font, wrap_label width 16, dashed-red ref
#     edges with constraint=false so cycles do not distort layout)
#   - same BFS / DFS path search semantics as graph.rs
#   - same HTTP shape: POST /api/invoke {cmd, args} dispatch
#   - same camelCase argument convention as the Tauri / Rust web port,
#     so web-src/main.js works unchanged
#
# Stdlib only — no pip install required.

from __future__ import annotations

import datetime as _dt
import json
import mimetypes
import os
import re
import secrets
import shutil
import sqlite3
import subprocess
import sys
import threading
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Iterable

# ============================================================================
# paths
# ============================================================================

def _user_data_dir() -> Path:
    """OS-default per-user data dir. Matches the Rust `dirs::data_dir()`
    semantics so the Python and Rust web editions land in the same place."""
    env_override = os.environ.get("THOUGHTGRAPH_DATA_DIR")
    if env_override:
        return Path(env_override)
    if sys.platform.startswith("win"):
        base = os.environ.get("APPDATA") or str(Path.home() / "AppData" / "Roaming")
        return Path(base) / "thoughtgraph"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "thoughtgraph"
    # Linux / others — follow XDG
    xdg = os.environ.get("XDG_DATA_HOME")
    if xdg:
        return Path(xdg) / "thoughtgraph"
    return Path.home() / ".local" / "share" / "thoughtgraph"


DATA_DIR = _user_data_dir()
DB_PATH = Path(os.environ.get("THOUGHTGRAPH_DB", str(DATA_DIR / "thoughtgraph.sqlite3")))
EXPORTS_DIR = DATA_DIR / "exports"

# web-src lives next to web-py in the repo. Env var override for packaged runs.
_DEFAULT_WEB_SRC = Path(__file__).resolve().parent.parent / "web-src"
WEB_SRC_DIR = Path(os.environ.get("THOUGHTGRAPH_WEB_SRC", str(_DEFAULT_WEB_SRC)))

# ============================================================================
# db helpers
# ============================================================================

# One connection per thread — sqlite3 in Python is by default not safe to share
# between threads. We use thread-local connections + WAL so concurrent reads /
# writes from worker threads serialise cleanly.
_local = threading.local()


def _now_iso() -> str:
    return _dt.datetime.now(_dt.timezone.utc).isoformat()


def db() -> sqlite3.Connection:
    conn = getattr(_local, "conn", None)
    if conn is None:
        DB_PATH.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(str(DB_PATH), isolation_level=None, check_same_thread=True)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys = ON;")
        conn.execute("PRAGMA journal_mode = WAL;")
        conn.execute("PRAGMA synchronous = NORMAL;")
        _local.conn = conn
    return conn


def init_db() -> None:
    """Schema mirrors src-tauri/src/db.rs::init(). Idempotent."""
    DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(DB_PATH), isolation_level=None)
    try:
        conn.execute("PRAGMA foreign_keys = ON;")
        conn.execute("PRAGMA journal_mode = WAL;")
        conn.execute("PRAGMA synchronous = NORMAL;")
        conn.executescript("""
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
        """)
        # Backfill FTS for pre-existing rows (first-run upgrade safety).
        fts_count = conn.execute("SELECT COUNT(*) FROM nodes_fts").fetchone()[0]
        node_count = conn.execute("SELECT COUNT(*) FROM nodes").fetchone()[0]
        if fts_count < node_count:
            conn.execute("DELETE FROM nodes_fts;")
            conn.execute(
                "INSERT INTO nodes_fts(rowid, app_id, content) SELECT id, app_id, content FROM nodes;"
            )
    finally:
        conn.close()


def _row_to_dict(row: sqlite3.Row) -> dict:
    return {k: row[k] for k in row.keys()}


def _touch_graph(conn: sqlite3.Connection, graph_id: int) -> None:
    conn.execute("UPDATE graphs SET updated_at = ? WHERE id = ?", (_now_iso(), graph_id))


# ============================================================================
# db operations — one function per Rust db::* function
# ============================================================================

def db_create_graph(name: str, description: str) -> dict:
    ts = _now_iso()
    conn = db()
    cur = conn.execute(
        "INSERT INTO graphs (name, description, created_at, updated_at) VALUES (?, ?, ?, ?)",
        (name, description, ts, ts),
    )
    return {
        "id": cur.lastrowid,
        "name": name,
        "description": description,
        "created_at": ts,
        "updated_at": ts,
    }


def db_list_graphs() -> list[dict]:
    rows = db().execute(
        "SELECT id, name, description, created_at, updated_at FROM graphs ORDER BY updated_at DESC"
    ).fetchall()
    return [_row_to_dict(r) for r in rows]


def db_rename_graph(graph_id: int, name: str, description: str) -> None:
    db().execute(
        "UPDATE graphs SET name = ?, description = ?, updated_at = ? WHERE id = ?",
        (name, description, _now_iso(), graph_id),
    )


def db_delete_graph(graph_id: int) -> None:
    db().execute("DELETE FROM graphs WHERE id = ?", (graph_id,))


def db_graph_by_id(graph_id: int) -> dict | None:
    row = db().execute(
        "SELECT id, name, description, created_at, updated_at FROM graphs WHERE id = ?",
        (graph_id,),
    ).fetchone()
    return _row_to_dict(row) if row else None


def db_node_by_app_id(graph_id: int, app_id: str) -> dict | None:
    row = db().execute(
        "SELECT id, graph_id, app_id, content, created_at FROM nodes WHERE graph_id = ? AND app_id = ?",
        (graph_id, app_id),
    ).fetchone()
    return _row_to_dict(row) if row else None


def db_create_node(graph_id: int, app_id: str, content: str, parent_node_id: int | None) -> dict:
    if not app_id.strip():
        raise ValueError("app_id cannot be empty")
    if db_node_by_app_id(graph_id, app_id):
        raise ValueError(f"app_id '{app_id}' already exists in this graph")
    ts = _now_iso()
    conn = db()
    cur = conn.execute(
        "INSERT INTO nodes (graph_id, app_id, content, created_at) VALUES (?, ?, ?, ?)",
        (graph_id, app_id, content, ts),
    )
    node_id = cur.lastrowid
    if parent_node_id is not None:
        conn.execute(
            "INSERT INTO edges (graph_id, from_node_id, to_node_id, kind, label) VALUES (?, ?, ?, 'reply', '')",
            (graph_id, parent_node_id, node_id),
        )
    _touch_graph(conn, graph_id)
    return {
        "id": node_id,
        "graph_id": graph_id,
        "app_id": app_id,
        "content": content,
        "created_at": ts,
    }


def db_update_node(node_id: int, content: str) -> None:
    conn = db()
    row = conn.execute("SELECT graph_id FROM nodes WHERE id = ?", (node_id,)).fetchone()
    if not row:
        raise ValueError(f"node {node_id} not found")
    conn.execute("UPDATE nodes SET content = ? WHERE id = ?", (content, node_id))
    _touch_graph(conn, row["graph_id"])


def db_delete_node(node_id: int) -> None:
    conn = db()
    row = conn.execute("SELECT graph_id FROM nodes WHERE id = ?", (node_id,)).fetchone()
    if not row:
        raise ValueError(f"node {node_id} not found")
    conn.execute("DELETE FROM nodes WHERE id = ?", (node_id,))
    _touch_graph(conn, row["graph_id"])


def db_list_nodes(graph_id: int) -> list[dict]:
    rows = db().execute(
        "SELECT id, graph_id, app_id, content, created_at FROM nodes WHERE graph_id = ? ORDER BY id ASC",
        (graph_id,),
    ).fetchall()
    return [_row_to_dict(r) for r in rows]


def db_list_edges(graph_id: int) -> list[dict]:
    rows = db().execute(
        "SELECT id, graph_id, from_node_id, to_node_id, kind, label FROM edges WHERE graph_id = ?",
        (graph_id,),
    ).fetchall()
    return [_row_to_dict(r) for r in rows]


def db_add_ref_edge(graph_id: int, from_node_id: int, to_app_id: str, label: str) -> dict:
    target = db_node_by_app_id(graph_id, to_app_id)
    if not target:
        raise ValueError(f"No node with app_id '{to_app_id}' in this graph")
    conn = db()
    cur = conn.execute(
        "INSERT INTO edges (graph_id, from_node_id, to_node_id, kind, label) VALUES (?, ?, ?, 'ref', ?)",
        (graph_id, from_node_id, target["id"], label),
    )
    _touch_graph(conn, graph_id)
    return {
        "id": cur.lastrowid,
        "graph_id": graph_id,
        "from_node_id": from_node_id,
        "to_node_id": target["id"],
        "kind": "ref",
        "label": label,
    }


def db_delete_edge(edge_id: int) -> None:
    conn = db()
    row = conn.execute("SELECT graph_id FROM edges WHERE id = ?", (edge_id,)).fetchone()
    if not row:
        raise ValueError(f"edge {edge_id} not found")
    conn.execute("DELETE FROM edges WHERE id = ?", (edge_id,))
    _touch_graph(conn, row["graph_id"])


def db_search_nodes(query: str, graph_id: int | None, limit: int) -> list[dict]:
    if not query.strip():
        return []
    base = """
        SELECT n.id AS n_id, n.graph_id AS n_graph_id, n.app_id AS n_app_id,
               n.content AS n_content, n.created_at AS n_created_at,
               g.name AS graph_name,
               snippet(nodes_fts, 1, '[', ']', '…', 12) AS snippet,
               bm25(nodes_fts) AS rank
        FROM nodes_fts
        JOIN nodes n ON n.id = nodes_fts.rowid
        JOIN graphs g ON g.id = n.graph_id
        WHERE nodes_fts MATCH ?
    """
    if graph_id is not None:
        sql = base + " AND n.graph_id = ? ORDER BY bm25(nodes_fts) ASC LIMIT ?"
        params: tuple = (query, graph_id, limit)
    else:
        sql = base + " ORDER BY bm25(nodes_fts) ASC LIMIT ?"
        params = (query, limit)
    rows = db().execute(sql, params).fetchall()
    return [
        {
            "node": {
                "id": r["n_id"],
                "graph_id": r["n_graph_id"],
                "app_id": r["n_app_id"],
                "content": r["n_content"],
                "created_at": r["n_created_at"],
            },
            "graph_name": r["graph_name"],
            "snippet": r["snippet"],
            "rank": r["rank"],
        }
        for r in rows
    ]


# ============================================================================
# DOT rendering — character-for-character port of graph.rs
# ============================================================================

# macOS-bundled CJK font; older Graphviz fontconfig is picky. STHeiti renders
# Chinese correctly where Helvetica / PingFang fall back to empty boxes.
FONT = "STHeiti"
WRAP_WIDTH = 16


def _escape_label(s: str) -> str:
    out = []
    for ch in s:
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\r":
            continue
        else:
            out.append(ch)
    return "".join(out)


def _wrap_label(s: str, width: int = WRAP_WIDTH) -> str:
    """DOT-side line wrapping for box labels. Mirrors graph::wrap_label.
    CJK glyphs count as one character; English breaks at the last space in the
    line, falling back to a hard break for long tokens / Chinese."""
    out: list[str] = []
    current: list[str] = []

    def flush(lst: list[str]):
        for ch in lst:
            if ch == '"':
                out.append('\\"')
            elif ch == "\\":
                out.append("\\\\")
            else:
                out.append(ch)
        lst.clear()

    for ch in s:
        if ch == "\r":
            continue
        if ch == "\n":
            flush(current)
            out.append("\\n")
            continue
        current.append(ch)
        if len(current) >= width:
            # Try to break at the last space in the line.
            try:
                idx = max(i for i, c in enumerate(current) if c == " ")
            except ValueError:
                idx = -1
            if idx >= width // 2:
                head = current[:idx]
                tail = current[idx + 1:]
                flush(head)
                out.append("\\n")
                current = tail
            else:
                flush(current)
                out.append("\\n")
                current = []
    flush(current)
    return "".join(out)


def render_dot(graph_name: str, nodes: list[dict], edges: list[dict]) -> str:
    out = []
    out.append(f'digraph "{_escape_label(graph_name)}" {{\n')
    out.append("  rankdir=LR;\n")
    out.append('  graph [splines=true, overlap=false, bgcolor="#fafafa"];\n')
    out.append(
        f'  node  [shape=box, style="rounded,filled", fillcolor="#ffffff", color="#888888", fontname="{FONT}", fontsize=11];\n'
    )
    out.append(f'  edge  [color="#888888", fontname="{FONT}", fontsize=10];\n\n')

    out.append(
        f'  graph_root [label="{_wrap_label(graph_name)}", shape=ellipse, fillcolor="#e8eef9", color="#3a73e8", fontsize=13];\n'
    )

    for n in nodes:
        out.append(
            f'  n{n["id"]} [label="{_wrap_label(n["content"])}", tooltip="{_escape_label(n["content"])}"];\n'
        )
    out.append("\n")

    has_reply_parent = {e["to_node_id"] for e in edges if e["kind"] == "reply"}
    for n in nodes:
        if n["id"] not in has_reply_parent:
            out.append(
                f'  graph_root -> n{n["id"]} [arrowhead=vee, color="#3a73e8", penwidth=1.4];\n'
            )

    for e in edges:
        style = ', style=dashed, color="#cc5555", constraint=false' if e["kind"] == "ref" else ""
        label_attr = f', label="{_escape_label(e["label"])}"' if e["label"] else ""
        out.append(
            f'  n{e["from_node_id"]} -> n{e["to_node_id"]} [arrowhead=vee{style}{label_attr}];\n'
        )
    out.append("}\n")
    return "".join(out)


def render_paths_dot(graph_name: str, paths: list[dict]) -> str:
    """Mirrors graph::render_paths_dot — one DOT subgraph showing only the
    nodes/edges that participate in the search hits, with each path coloured
    distinctly."""
    colors = ["#3a73e8", "#cc5555", "#449944", "#cc8822", "#9966cc", "#1b9aaa", "#d36b6b"]

    all_nodes: dict[int, dict] = {}
    for p in paths:
        for n in p["nodes"]:
            all_nodes.setdefault(n["id"], n)
    if paths and paths[0]["nodes"]:
        start_id = paths[0]["nodes"][0]["id"]
        end_id = paths[0]["nodes"][-1]["id"]
    else:
        start_id = end_id = -1

    out = [f'digraph "paths · {_escape_label(graph_name)}" {{\n']
    out.append("  rankdir=LR;\n")
    out.append('  graph [splines=true, overlap=false, bgcolor="#fafafa"];\n')
    out.append(
        f'  node  [shape=box, style="rounded,filled", fillcolor="#ffffff", color="#888888", fontname="{FONT}", fontsize=11];\n'
    )
    out.append(f'  edge  [fontname="{FONT}", fontsize=10, penwidth=2];\n\n')

    for nid, n in all_nodes.items():
        extra = ""
        if nid == start_id or nid == end_id:
            extra = ', color="#1f2330", penwidth=2.5, fillcolor="#fff8e0"'
        out.append(
            f'  n{nid} [label="{_wrap_label(n["content"])}", tooltip="{_escape_label(n["content"])}"{extra}];\n'
        )
    out.append("\n")

    for pi, p in enumerate(paths):
        color = colors[pi % len(colors)]
        for i, step in enumerate(p["steps"]):
            a = p["nodes"][i + 1]["id"] if step["reversed"] else p["nodes"][i]["id"]
            b = p["nodes"][i]["id"] if step["reversed"] else p["nodes"][i + 1]["id"]
            style = ", style=dashed" if step["kind"] == "ref" else ""
            label = f"p{pi + 1}"
            if step["label"]:
                label += f" · {step['label']}"
            out.append(
                f'  n{a} -> n{b} [arrowhead=vee, color="{color}"{style}, label="{_escape_label(label)}"];\n'
            )
    out.append("}\n")
    return "".join(out)


# ============================================================================
# path search — mirrors graph::find_paths and graph::find_paths_by_keyword
# ============================================================================

def find_paths(graph_id: int, from_app_id: str, to_app_id: str, max_paths: int) -> list[dict]:
    """BFS over the **undirected** graph, recording per-step direction so the
    UI can render arrows correctly. Without the undirected traversal, a leaf
    reply could never reach its ancestor (reply edges only point downward)."""
    start = db_node_by_app_id(graph_id, from_app_id)
    if not start:
        raise ValueError(f"from app_id not found: {from_app_id}")
    end = db_node_by_app_id(graph_id, to_app_id)
    if not end:
        raise ValueError(f"to app_id not found: {to_app_id}")

    nodes = {n["id"]: n for n in db_list_nodes(graph_id)}
    edges = db_list_edges(graph_id)

    adj: dict[int, list[tuple[int, str, bool, str]]] = {}
    for e in edges:
        adj.setdefault(e["from_node_id"], []).append(
            (e["to_node_id"], e["kind"], False, e["label"])
        )
        adj.setdefault(e["to_node_id"], []).append(
            (e["from_node_id"], e["kind"], True, e["label"])
        )

    from collections import deque
    queue: deque = deque()
    queue.append([(start["id"], None)])
    hits: list[list] = []
    shortest_len: int | None = None
    max_depth = 64

    while queue:
        path = queue.popleft()
        cur = path[-1][0]
        if cur == end["id"]:
            l = len(path)
            if shortest_len is None:
                shortest_len = l
            elif l > shortest_len:
                break
            hits.append(path)
            if len(hits) >= max_paths:
                break
            continue
        if len(path) >= max_depth:
            continue
        for (nb, kind, rev, label) in adj.get(cur, []):
            if any(nid == nb for nid, _ in path):
                continue
            np = list(path) + [(nb, (kind, rev, label))]
            queue.append(np)

    result = []
    for p in hits:
        ns = []
        steps = []
        for nid, edge in p:
            if nid in nodes:
                ns.append(nodes[nid])
            if edge is not None:
                kind, rev, label = edge
                steps.append({"kind": kind, "reversed": rev, "label": label})
        result.append({"nodes": ns, "steps": steps})
    return result


def find_paths_by_keyword(graph_id: int, from_kw: str, to_kw: str, max_paths: int) -> list[dict]:
    """Directed DFS enumerating every simple path from any node matching the
    From keyword to any node matching the To keyword. Matching is
    case-insensitive substring on `content` or `app_id`, same as graph.rs."""
    from_key = from_kw.strip().lower()
    to_key = to_kw.strip().lower()
    if not from_key:
        raise ValueError("from keyword cannot be empty")
    if not to_key:
        raise ValueError("to keyword cannot be empty")

    nodes_list = db_list_nodes(graph_id)
    edges = db_list_edges(graph_id)

    def matches(n: dict, kw: str) -> bool:
        return kw in n["content"].lower() or kw in n["app_id"].lower()

    from_nodes = [n for n in nodes_list if matches(n, from_key)]
    to_ids = {n["id"] for n in nodes_list if matches(n, to_key)}
    if not from_nodes:
        raise ValueError(f"no nodes match from keyword: {from_kw}")
    if not to_ids:
        raise ValueError(f"no nodes match to keyword: {to_kw}")

    adj: dict[int, list[tuple[int, str, str]]] = {}
    for e in edges:
        adj.setdefault(e["from_node_id"], []).append((e["to_node_id"], e["kind"], e["label"]))

    nodes = {n["id"]: n for n in nodes_list}
    max_depth = 32
    all_hits: list[dict] = []

    def dfs(current: int, visited: set, cur_nodes: list, cur_steps: list):
        if len(all_hits) >= max_paths:
            return
        if len(cur_nodes) > 1 and current in to_ids:
            all_hits.append({
                "nodes": [nodes[nid] for nid in cur_nodes if nid in nodes],
                "steps": list(cur_steps),
            })
            return
        if len(cur_nodes) >= max_depth:
            return
        visited.add(current)
        for (nb, kind, label) in adj.get(current, []):
            if nb in visited:
                continue
            cur_nodes.append(nb)
            cur_steps.append({"kind": kind, "reversed": False, "label": label})
            dfs(nb, visited, cur_nodes, cur_steps)
            cur_nodes.pop()
            cur_steps.pop()
            if len(all_hits) >= max_paths:
                break
        visited.discard(current)

    for fn in from_nodes:
        if len(all_hits) >= max_paths:
            break
        dfs(fn["id"], set(), [fn["id"]], [])
    return all_hits


# ============================================================================
# dot binary discovery
# ============================================================================

def _which_dot() -> str:
    env = os.environ.get("DOT_BIN")
    if env and Path(env).exists():
        return env
    found = shutil.which("dot") or shutil.which("dot.exe")
    if found:
        return found
    candidates: Iterable[str] = []
    if sys.platform.startswith("win"):
        candidates = [
            r"C:\Program Files\Graphviz\bin\dot.exe",
            r"C:\Program Files (x86)\Graphviz\bin\dot.exe",
            r"C:\Graphviz\bin\dot.exe",
        ]
    else:
        candidates = [
            "/usr/local/bin/dot",
            "/opt/homebrew/bin/dot",
            "/usr/bin/dot",
            "/data/data/com.termux/files/usr/bin/dot",
        ]
    for c in candidates:
        if Path(c).exists():
            return c
    raise RuntimeError(
        "graphviz `dot` not found. Install from https://graphviz.org/download/ "
        "and ensure it is on PATH, or set the DOT_BIN env var."
    )


def _safe_name(s: str) -> str:
    return "".join(c if (c.isalnum() or c in "-_") else "_" for c in s)


def _run_dot(gv_path: Path, img_path: Path, fmt: str) -> None:
    dot_bin = _which_dot()
    res = subprocess.run(
        [dot_bin, f"-T{fmt}", str(gv_path), "-o", str(img_path)],
        capture_output=True,
    )
    if res.returncode != 0:
        raise RuntimeError(
            f"dot exited with status {res.returncode}: {res.stderr.decode('utf-8', 'replace')}"
        )


# ============================================================================
# command dispatcher — keys match web-server/src/main.rs::dispatch
# ============================================================================

_db_lock = threading.Lock()


def _graph_name(graph_id: int) -> str:
    g = db_graph_by_id(graph_id)
    if not g:
        raise ValueError(f"graph {graph_id} not found")
    return g["name"]


def _export_url(p: Path) -> str:
    return "/exports/" + p.name


def dispatch(cmd: str, args: dict) -> Any:
    """Single entry point that maps to the same set of commands the Rust
    web-server exposes. Argument keys are camelCase to match the Tauri /
    desktop convention, so the existing web-src/main.js works unchanged."""
    with _db_lock:
        if cmd == "create_graph":
            return db_create_graph(args["name"], args["description"])
        if cmd == "list_graphs":
            return db_list_graphs()
        if cmd == "rename_graph":
            db_rename_graph(args["id"], args["name"], args["description"])
            return None
        if cmd == "delete_graph":
            db_delete_graph(args["id"])
            return None
        if cmd == "create_node":
            return db_create_node(
                args["graphId"], args["appId"], args["content"], args.get("parentNodeId"),
            )
        if cmd == "update_node":
            db_update_node(args["nodeId"], args["content"])
            return None
        if cmd == "delete_node":
            db_delete_node(args["nodeId"])
            return None
        if cmd == "list_nodes":
            return db_list_nodes(args["graphId"])
        if cmd == "list_edges":
            return db_list_edges(args["graphId"])
        if cmd == "add_ref_edge":
            return db_add_ref_edge(
                args["graphId"], args["fromNodeId"], args["toAppId"], args["label"],
            )
        if cmd == "delete_edge":
            db_delete_edge(args["edgeId"])
            return None
        if cmd == "preview_dot":
            gid = args["graphId"]
            return render_dot(_graph_name(gid), db_list_nodes(gid), db_list_edges(gid))
        if cmd == "export_gv":
            gid = args["graphId"]
            name = _graph_name(gid)
            safe = _safe_name(name)
            EXPORTS_DIR.mkdir(parents=True, exist_ok=True)
            p = EXPORTS_DIR / f"{safe}.gv"
            p.write_text(
                render_dot(name, db_list_nodes(gid), db_list_edges(gid)),
                encoding="utf-8",
            )
            return {"path": str(p), "url": _export_url(p), "filename": p.name}
        if cmd in ("render_and_open",):
            gid = args["graphId"]
            fmt = args.get("format") or "pdf"
            name = _graph_name(gid)
            safe = _safe_name(name)
            EXPORTS_DIR.mkdir(parents=True, exist_ok=True)
            gv = EXPORTS_DIR / f"{safe}.gv"
            img = EXPORTS_DIR / f"{safe}.{fmt}"
            gv.write_text(
                render_dot(name, db_list_nodes(gid), db_list_edges(gid)),
                encoding="utf-8",
            )
            _run_dot(gv, img, fmt)
            return {
                "gv_path": str(gv), "image_path": str(img),
                "gv_url": _export_url(gv), "image_url": _export_url(img),
            }
        if cmd == "open_in_graphviz_app":
            # No Graphviz.app on Windows — serve the .gv for download.
            gid = args["graphId"]
            name = _graph_name(gid)
            safe = _safe_name(name)
            EXPORTS_DIR.mkdir(parents=True, exist_ok=True)
            p = EXPORTS_DIR / f"{safe}.gv"
            p.write_text(
                render_dot(name, db_list_nodes(gid), db_list_edges(gid)),
                encoding="utf-8",
            )
            return {"path": str(p), "url": _export_url(p), "filename": p.name}
        if cmd == "find_paths":
            return find_paths(
                args["graphId"], args["fromAppId"], args["toAppId"],
                int(args.get("maxPaths") or 10),
            )
        if cmd == "find_paths_by_keyword":
            return find_paths_by_keyword(
                args["graphId"], args["fromKeyword"], args["toKeyword"],
                int(args.get("maxPaths") or 50),
            )
        if cmd == "render_paths_and_open":
            gid = args["graphId"]
            name = _graph_name(gid)
            paths = find_paths(
                gid, args["fromAppId"], args["toAppId"], int(args.get("maxPaths") or 10),
            )
            if not paths:
                raise ValueError("No paths to render — the two nodes are not connected.")
            return _render_paths_to_files(paths, name, args.get("format") or "pdf")
        if cmd == "render_paths_by_keyword_and_open":
            gid = args["graphId"]
            name = _graph_name(gid)
            paths = find_paths_by_keyword(
                gid, args["fromKeyword"], args["toKeyword"], int(args.get("maxPaths") or 50),
            )
            if not paths:
                raise ValueError("No paths found for those keywords.")
            return _render_paths_to_files(paths, name, args.get("format") or "pdf")
        if cmd == "search_nodes":
            return db_search_nodes(
                args["query"], args.get("graphId"), int(args.get("limit") or 30),
            )
        raise ValueError(f"unknown command: {cmd}")


def _render_paths_to_files(paths: list[dict], graph_name: str, fmt: str) -> dict:
    EXPORTS_DIR.mkdir(parents=True, exist_ok=True)
    safe = _safe_name(graph_name)
    gv = EXPORTS_DIR / f"{safe}_paths.gv"
    img = EXPORTS_DIR / f"{safe}_paths.{fmt}"
    gv.write_text(render_paths_dot(graph_name, paths), encoding="utf-8")
    _run_dot(gv, img, fmt)
    return {
        "gv_path": str(gv), "image_path": str(img),
        "gv_url": _export_url(gv), "image_url": _export_url(img),
    }


# ============================================================================
# HTTP server
# ============================================================================

# Add a couple of MIME types Python's stdlib doesn't know about by default.
mimetypes.add_type("application/javascript", ".js")
mimetypes.add_type("text/vnd.graphviz", ".gv")
mimetypes.add_type("text/vnd.graphviz", ".dot")
mimetypes.add_type("image/svg+xml", ".svg")


def _safe_join(base: Path, rel: str) -> Path | None:
    rel = rel.lstrip("/")
    if not rel:
        return None
    if ".." in rel.split("/") or rel.startswith("/") or re.match(r"^[A-Za-z]:", rel):
        return None
    cand = (base / rel).resolve()
    try:
        cand.relative_to(base.resolve())
    except ValueError:
        return None
    return cand


class Handler(BaseHTTPRequestHandler):
    # Silence the default per-request stderr line; we log once in the dispatch.
    def log_message(self, fmt, *args):  # noqa: A003
        sys.stderr.write("[http] " + (fmt % args) + "\n")

    def _send_json(self, status: int, payload: Any) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _send_text(self, status: int, body: str, ctype: str = "text/plain; charset=utf-8") -> None:
        data = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _serve_file(self, p: Path) -> None:
        if not p.is_file():
            self._send_text(404, "Not Found")
            return
        try:
            data = p.read_bytes()
        except OSError as e:
            self._send_text(500, f"read error: {e}")
            return
        ctype, _ = mimetypes.guess_type(p.name)
        ctype = ctype or "application/octet-stream"
        if ctype.startswith("text/") and "charset" not in ctype:
            ctype = f"{ctype}; charset=utf-8"
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):  # noqa: N802
        path = self.path.split("?", 1)[0]
        if path == "/" or path == "/index.html":
            self._serve_file(WEB_SRC_DIR / "index.html")
            return
        if path == "/healthz":
            self._send_text(200, "ok")
            return
        if path.startswith("/exports/"):
            cand = _safe_join(EXPORTS_DIR, path[len("/exports/"):])
            if cand is None:
                self._send_text(400, "Bad path")
                return
            self._serve_file(cand)
            return
        cand = _safe_join(WEB_SRC_DIR, path)
        if cand is None or not cand.is_file():
            self._send_text(404, "Not Found")
            return
        self._serve_file(cand)

    def do_POST(self):  # noqa: N802
        path = self.path.split("?", 1)[0]
        if path != "/api/invoke":
            self._send_text(404, "Not Found")
            return
        length = int(self.headers.get("Content-Length", "0") or "0")
        raw = self.rfile.read(length) if length else b""
        try:
            payload = json.loads(raw.decode("utf-8") or "{}")
        except Exception as e:
            self._send_json(400, {"error": f"bad json: {e}"})
            return
        cmd = payload.get("cmd")
        args = payload.get("args") or {}
        if not isinstance(cmd, str):
            self._send_json(400, {"error": "missing cmd"})
            return
        try:
            result = dispatch(cmd, args)
            self._send_json(200, result)
        except ValueError as e:
            # Domain errors (not found, validation) — 400.
            sys.stderr.write(f"[invoke {cmd}] {e}\n")
            self._send_json(400, {"error": str(e)})
        except Exception as e:
            sys.stderr.write(f"[invoke {cmd}] {e}\n")
            traceback.print_exc()
            self._send_json(500, {"error": str(e)})


# ============================================================================
# main
# ============================================================================

def main() -> int:
    host = os.environ.get("HOST", "127.0.0.1")
    port = int(os.environ.get("PORT", "8888"))

    DATA_DIR.mkdir(parents=True, exist_ok=True)
    EXPORTS_DIR.mkdir(parents=True, exist_ok=True)
    init_db()

    sys.stderr.write("ThoughtGraph Web server (Python edition)\n")
    sys.stderr.write(f"  python   : {sys.version.split()[0]}\n")
    sys.stderr.write(f"  database : {DB_PATH}\n")
    sys.stderr.write(f"  exports  : {EXPORTS_DIR}\n")
    sys.stderr.write(f"  web-src  : {WEB_SRC_DIR}\n")
    sys.stderr.write(f"  listen   : http://{host}:{port}\n\n")
    sys.stderr.write(f"Open http://{host}:{port} in your browser.\n")

    if not (WEB_SRC_DIR / "index.html").is_file():
        sys.stderr.write(
            f"\nWARNING: {WEB_SRC_DIR / 'index.html'} not found. "
            "Set THOUGHTGRAPH_WEB_SRC to the directory containing index.html.\n"
        )

    server = ThreadingHTTPServer((host, port), Handler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        sys.stderr.write("\nshutting down\n")
        server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
