pub mod backup;
mod commands;
pub mod db;
pub mod graph;

use db::DbState;
use std::sync::Mutex;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("could not resolve app data dir");
            std::fs::create_dir_all(&data_dir).ok();
            let db_path = data_dir.join("thoughtgraph.sqlite3");
            let conn = db::init(&db_path).expect("failed to initialise database");
            app.manage(DbState {
                conn: Mutex::new(conn),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_graph,
            commands::list_graphs,
            commands::rename_graph,
            commands::delete_graph,
            commands::create_node,
            commands::update_node,
            commands::delete_node,
            commands::list_nodes,
            commands::list_edges,
            commands::add_ref_edge,
            commands::delete_edge,
            commands::preview_dot,
            commands::export_gv,
            commands::render_and_open,
            commands::open_in_graphviz_app,
            commands::find_paths,
            commands::find_paths_by_keyword,
            commands::search_nodes,
            commands::render_paths_and_open,
            commands::render_paths_by_keyword_and_open,
            commands::backup_now,
            commands::backup_history,
            commands::backup_commit_detail,
            commands::open_backup_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
