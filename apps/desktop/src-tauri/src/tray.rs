//! Menu bar / notification area integration.
//!
//! A keyboard and mouse sharing tool lives in the background, so the tray is
//! the primary way to reach it: show the window, stop sharing in a hurry, or
//! quit properly.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use ud_engine::Command;

use crate::state::AppState;

const TRAY_ID: &str = "uniondesk-tray";

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show UnionDesk", true, None::<&str>)?;
    let release = MenuItem::with_id(app, "release", "Release control", true, None::<&str>)?;
    let sharing = MenuItem::with_id(app, "sharing", "Sharing: on/off", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit UnionDesk", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &release, &sharing, &separator, &quit])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("UnionDesk")
        .on_menu_event(handle_menu_event)
        .on_tray_icon_event(handle_tray_event);
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        "show" => show_window(app),
        "release" => with_state(app, |state| {
            let _ = state.engine.send(Command::ReleaseControl);
        }),
        "sharing" => with_state(app, |state| {
            let enabled = state
                .snapshot
                .read()
                .as_ref()
                .map(|snapshot| snapshot.control.sharing_enabled)
                .unwrap_or(false);
            let _ = state.engine.send(Command::SetSharing(!enabled));
        }),
        "quit" => quit(app),
        _ => {}
    }
}

fn handle_tray_event(tray: &tauri::tray::TrayIcon, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        show_window(tray.app_handle());
    }
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn quit(app: &AppHandle) {
    with_state(app, |state| {
        state
            .quitting
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = state.engine.shutdown();
    });
    app.exit(0);
}

fn with_state(app: &AppHandle, action: impl FnOnce(&AppState)) {
    if let Some(state) = app.try_state::<AppState>() {
        action(&state);
    }
    let _ = Emitter::emit(app, "uniondesk://tray", ());
}
