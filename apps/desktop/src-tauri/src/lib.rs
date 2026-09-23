//! UnionDesk desktop application.

mod commands;
mod logging;
mod state;
mod tray;

use std::sync::atomic::Ordering;

use tauri::{Emitter, Manager, RunEvent, WindowEvent};

use ud_engine::{start, EngineEvent, EngineOptions, Notice};

use crate::state::AppState;

/// Event names the frontend listens to.
const EVENT_SNAPSHOT: &str = "uniondesk://snapshot";
const EVENT_NOTICE: &str = "uniondesk://notice";
const EVENT_DROP: &str = "uniondesk://dropped";

pub fn run() {
    let config_dir = ud_core::paths::config_dir();
    let log_path = logging::init(&config_dir);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_dir.display(),
        log = ?log_path,
        "UnionDesk starting"
    );

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // macOS only shows the Accessibility dialog when the application
            // asks for it, and this hook runs on the main thread, which is where
            // a system prompt belongs.
            ud_input::request_permissions();
            let options = EngineOptions::discover();
            // The engine is a Tokio actor, so it has to be created from inside
            // a runtime; Tauri's setup hook is not one.
            let (engine, events) = tauri::async_runtime::block_on(async move { start(options) })
                .map_err(|err| err.to_string())?;
            app.manage(AppState::new(engine));
            spawn_event_pump(app.handle().clone(), events);
            tray::install(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                // Closing the window keeps the engine running in the tray.
                let quitting = window
                    .app_handle()
                    .try_state::<AppState>()
                    .map(|state| state.quitting.load(Ordering::SeqCst))
                    .unwrap_or(false);
                if !quitting {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) => {
                if let Some(state) = window.app_handle().try_state::<AppState>() {
                    state.pending_drop.lock().extend(paths.iter().cloned());
                }
                let _ = window
                    .app_handle()
                    .emit(EVENT_DROP, paths.len());
            }
            WindowEvent::DragDrop(tauri::DragDropEvent::Enter { paths, .. }) => {
                let _ = window.app_handle().emit(EVENT_DROP, paths.len());
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::save_settings,
            commands::set_sharing,
            commands::release_control,
            commands::connect,
            commands::connect_address,
            commands::disconnect,
            commands::forget,
            commands::answer_pairing,
            commands::set_link,
            commands::remove_link,
            commands::send_files,
            commands::accept_transfer,
            commands::cancel_transfer,
            commands::clear_finished_transfers,
            commands::open_download_dir,
            commands::pick_files,
            commands::pick_download_dir,
            commands::take_pending_drop,
            commands::refresh,
            commands::platform_notes,
            commands::open_permission_settings,
        ]);

    let app = builder
        .build(tauri::generate_context!())
        .expect("UnionDesk failed to start");

    app.run(|app_handle, event| {
        if let RunEvent::ExitRequested { api, code, .. } = &event {
            // Only the tray's Quit entry should end the process; a closed window
            // must not, because the engine keeps sharing in the background.
            let quitting = app_handle
                .try_state::<AppState>()
                .map(|state| state.quitting.load(Ordering::SeqCst))
                .unwrap_or(false);
            if !quitting && code.is_none() {
                api.prevent_exit();
            }
        }
    });
}

/// Copies engine output into the UI: the newest snapshot is cached and both
/// snapshots and notices are pushed to the webview.
fn spawn_event_pump(app: tauri::AppHandle, mut events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>) {
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                EngineEvent::Snapshot(snapshot) => {
                    if let Some(state) = app.try_state::<AppState>() {
                        *state.snapshot.write() = Some((*snapshot).clone());
                    }
                    let _ = app.emit(EVENT_SNAPSHOT, &*snapshot);
                }
                EngineEvent::Notice(notice) => {
                    if let Notice::Error { message } = &notice {
                        tracing::warn!(%message, "engine notice");
                    }
                    let _ = app.emit(EVENT_NOTICE, &notice);
                }
            }
        }
    });
}
