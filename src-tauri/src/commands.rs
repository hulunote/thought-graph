use crate::db::{self, DbState, Edge, Graph, Node};
use crate::graph::{self, PathHit, RenderResult};
use std::path::PathBuf;
use tauri::{Manager, State};

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
pub fn create_graph(state: State<'_, DbState>, name: String, description: String) -> Result<Graph, String> {
    let conn = state.conn.lock().map_err(err)?;
    db::create_graph(&conn, &name, &description).map_err(err)
}

#[tauri::command]
pub fn list_graphs(state: State<'_, DbState>) -> Result<Vec<Graph>, String> {
    let conn = state.conn.lock().map_err(err)?;
    db::list_graphs(&conn).map_err(err)
}

#[tauri::command]
pub fn rename_graph(
    state: State<'_, DbState>,
    id: i64,
    name: String,
    description: String,
) -> Result<(), String> {
    let conn = state.conn.lock().map_err(err)?;
    db::rename_graph(&conn, id, &name, &description).map_err(err)
}

#[tauri::command]
pub fn delete_graph(state: State<'_, DbState>, id: i64) -> Result<(), String> {
    let conn = state.conn.lock().map_err(err)?;
    db::delete_graph(&conn, id).map_err(err)
}

#[tauri::command]
pub fn create_node(
    state: State<'_, DbState>,
    graph_id: i64,
    app_id: String,
    content: String,
    parent_node_id: Option<i64>,
) -> Result<Node, String> {
    let conn = state.conn.lock().map_err(err)?;
    db::create_node(&conn, graph_id, &app_id, &content, parent_node_id).map_err(err)
}

#[tauri::command]
pub fn update_node(state: State<'_, DbState>, node_id: i64, content: String) -> Result<(), String> {
    let conn = state.conn.lock().map_err(err)?;
    db::update_node(&conn, node_id, &content).map_err(err)
}

#[tauri::command]
pub fn delete_node(state: State<'_, DbState>, node_id: i64) -> Result<(), String> {
    let conn = state.conn.lock().map_err(err)?;
    db::delete_node(&conn, node_id).map_err(err)
}

#[tauri::command]
pub fn list_nodes(state: State<'_, DbState>, graph_id: i64) -> Result<Vec<Node>, String> {
    let conn = state.conn.lock().map_err(err)?;
    db::list_nodes(&conn, graph_id).map_err(err)
}

#[tauri::command]
pub fn list_edges(state: State<'_, DbState>, graph_id: i64) -> Result<Vec<Edge>, String> {
    let conn = state.conn.lock().map_err(err)?;
    db::list_edges(&conn, graph_id).map_err(err)
}

#[tauri::command]
pub fn add_ref_edge(
    state: State<'_, DbState>,
    graph_id: i64,
    from_node_id: i64,
    to_app_id: String,
    label: String,
) -> Result<Edge, String> {
    let conn = state.conn.lock().map_err(err)?;
    db::add_ref_edge(&conn, graph_id, from_node_id, &to_app_id, &label).map_err(err)
}

#[tauri::command]
pub fn delete_edge(state: State<'_, DbState>, edge_id: i64) -> Result<(), String> {
    let conn = state.conn.lock().map_err(err)?;
    db::delete_edge(&conn, edge_id).map_err(err)
}

#[tauri::command]
pub fn preview_dot(state: State<'_, DbState>, graph_id: i64) -> Result<String, String> {
    let conn = state.conn.lock().map_err(err)?;
    let g_name = conn
        .query_row(
            "SELECT name FROM graphs WHERE id = ?1",
            [graph_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(err)?;
    let nodes = db::list_nodes(&conn, graph_id).map_err(err)?;
    let edges = db::list_edges(&conn, graph_id).map_err(err)?;
    Ok(graph::render_dot(&g_name, &nodes, &edges))
}

fn export_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    Ok(base.join("exports"))
}

#[tauri::command]
pub fn export_gv(
    app: tauri::AppHandle,
    state: State<'_, DbState>,
    graph_id: i64,
) -> Result<String, String> {
    let conn = state.conn.lock().map_err(err)?;
    let g_name = conn
        .query_row(
            "SELECT name FROM graphs WHERE id = ?1",
            [graph_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(err)?;
    let safe: String = g_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let dir = export_dir(&app)?;
    std::fs::create_dir_all(&dir).map_err(err)?;
    let path = dir.join(format!("{}.gv", safe));
    graph::export_dot_to_path(&conn, graph_id, &g_name, &path).map_err(err)?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn render_and_open(
    app: tauri::AppHandle,
    state: State<'_, DbState>,
    graph_id: i64,
    format: Option<String>,
) -> Result<RenderResult, String> {
    let fmt = format.unwrap_or_else(|| "pdf".to_string());
    let conn = state.conn.lock().map_err(err)?;
    let g_name = conn
        .query_row(
            "SELECT name FROM graphs WHERE id = ?1",
            [graph_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(err)?;
    let dir = export_dir(&app)?;
    let res = graph::render_and_save(&conn, graph_id, &g_name, &dir, &fmt).map_err(err)?;
    // open with default app
    std::process::Command::new("open")
        .arg(&res.image_path)
        .status()
        .map_err(err)?;
    Ok(res)
}

#[tauri::command]
pub fn open_in_graphviz_app(
    app: tauri::AppHandle,
    state: State<'_, DbState>,
    graph_id: i64,
) -> Result<String, String> {
    // Save .gv then open with Graphviz.app explicitly
    let path = export_gv(app, state, graph_id)?;
    let app_path = std::path::Path::new("/Applications/Graphviz.app");
    if app_path.exists() {
        std::process::Command::new("open")
            .arg("-a")
            .arg("Graphviz")
            .arg(&path)
            .status()
            .map_err(err)?;
    } else {
        std::process::Command::new("open")
            .arg(&path)
            .status()
            .map_err(err)?;
    }
    Ok(path)
}

#[tauri::command]
pub fn find_paths(
    state: State<'_, DbState>,
    graph_id: i64,
    from_app_id: String,
    to_app_id: String,
    max_paths: Option<usize>,
) -> Result<Vec<PathHit>, String> {
    let conn = state.conn.lock().map_err(err)?;
    graph::find_paths(
        &conn,
        graph_id,
        &from_app_id,
        &to_app_id,
        max_paths.unwrap_or(10),
    )
    .map_err(err)
}
