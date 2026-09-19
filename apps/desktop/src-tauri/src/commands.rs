//! The thin command surface the UI talks to.
//!
//! Commands never block on the engine: they queue a request and return, and the
//! UI updates from the snapshot stream. That keeps the UI responsive even while
//! a handshake or a file transfer is in flight.

use std::path::PathBuf;

use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use ud_core::config::{EdgeLink, Settings};
use ud_core::identity::DeviceId;
use ud_core::protocol::TransferId;
use ud_engine::{Command, Snapshot};

use crate::state::AppState;

type Result<T> = std::result::Result<T, String>;

fn queue(state: &AppState, command: Command) -> Result<()> {
    state
        .engine
        .send(command)
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub fn get_snapshot(state: State<'_, AppState>) -> Option<Snapshot> {
    state.snapshot.read().clone()
}

#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<()> {
    queue(&state, Command::UpdateSettings(Box::new(settings)))
}

#[tauri::command]
pub fn set_sharing(state: State<'_, AppState>, enabled: bool) -> Result<()> {
    queue(&state, Command::SetSharing(enabled))
}

#[tauri::command]
pub fn release_control(state: State<'_, AppState>) -> Result<()> {
    queue(&state, Command::ReleaseControl)
}

#[tauri::command]
pub fn connect(state: State<'_, AppState>, device_id: String) -> Result<()> {
    queue(&state, Command::Connect(DeviceId(device_id)))
}

#[tauri::command]
pub fn connect_address(
    state: State<'_, AppState>,
    address: String,
    name: Option<String>,
) -> Result<()> {
    let address: std::net::SocketAddr = normalize_address(&address)
        .parse()
        .map_err(|_| format!("'{address}' is not a valid address. Use host:port."))?;
    queue(
        &state,
        Command::ConnectAddress {
            address,
            name,
        },
    )
}

#[tauri::command]
pub fn disconnect(state: State<'_, AppState>, device_id: String) -> Result<()> {
    queue(&state, Command::Disconnect(DeviceId(device_id)))
}

#[tauri::command]
pub fn forget(state: State<'_, AppState>, device_id: String) -> Result<()> {
    queue(&state, Command::Forget(DeviceId(device_id)))
}

#[tauri::command]
pub fn answer_pairing(
    state: State<'_, AppState>,
    device_id: String,
    code: Option<String>,
    accept: bool,
) -> Result<()> {
    queue(
        &state,
        Command::AnswerPairing {
            peer: DeviceId(device_id),
            code,
            accept,
        },
    )
}

#[tauri::command]
pub fn set_link(
    state: State<'_, AppState>,
    device_id: String,
    name: String,
    side: String,
) -> Result<()> {
    let local_side = parse_side(&side)?;
    let mut link = EdgeLink::new(DeviceId(device_id), name, local_side);
    link.remote_side = local_side.opposite();
    queue(&state, Command::SetLink(Box::new(link)))
}

#[tauri::command]
pub fn remove_link(state: State<'_, AppState>, device_id: String) -> Result<()> {
    queue(&state, Command::RemoveLink(DeviceId(device_id)))
}

#[tauri::command]
pub fn send_files(state: State<'_, AppState>, device_id: String, paths: Vec<String>) -> Result<()> {
    if paths.is_empty() {
        return Err("no files were selected".into());
    }
    queue(
        &state,
        Command::SendFiles {
            peer: DeviceId(device_id),
            paths: paths.into_iter().map(PathBuf::from).collect(),
        },
    )
}

#[tauri::command]
pub fn accept_transfer(state: State<'_, AppState>, id: u64) -> Result<()> {
    queue(&state, Command::AcceptTransfer(TransferId(id)))
}

#[tauri::command]
pub fn cancel_transfer(state: State<'_, AppState>, id: u64) -> Result<()> {
    queue(&state, Command::CancelTransfer(TransferId(id)))
}

#[tauri::command]
pub fn clear_finished_transfers(state: State<'_, AppState>) -> Result<()> {
    queue(&state, Command::ClearFinishedTransfers)
}

#[tauri::command]
pub fn open_download_dir(state: State<'_, AppState>) -> Result<()> {
    queue(&state, Command::OpenDownloadDir)
}

/// Opens the native picker. Returns the chosen paths, or an empty list.
#[tauri::command]
pub async fn pick_files(app: AppHandle, multiple: bool) -> Result<Vec<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog().file().add_filter("All files", &["*"]);
    if multiple {
        builder = builder.set_title("Choose files to send");
    } else {
        builder = builder.set_title("Choose a file to send");
    }
    builder.pick_files(move |paths| {
        let result = paths
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| path.into_path().ok())
            .map(|path| path.to_string_lossy().to_string())
            .collect();
        let _ = tx.send(result);
    });
    rx.await.map_err(|_| "the file picker was dismissed".to_string())
}

/// Opens the native folder picker and stores the choice as the download folder.
#[tauri::command]
pub async fn pick_download_dir(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose where received files are saved")
        .pick_folder(move |path| {
            let _ = tx.send(
                path.and_then(|path| path.into_path().ok())
                    .map(|path| path.to_string_lossy().to_string()),
            );
        });
    let chosen = rx
        .await
        .map_err(|_| "the folder picker was dismissed".to_string())?;
    let Some(chosen) = chosen else {
        return Ok(String::new());
    };
    let Some(mut settings) = state.snapshot.read().as_ref().map(|s| s.settings.clone()) else {
        return Err("settings are not loaded yet".into());
    };
    settings.transfer.download_dir = PathBuf::from(&chosen);
    queue(&state, Command::UpdateSettings(Box::new(settings)))?;
    Ok(chosen)
}

/// Files that were dropped onto the window and are waiting for a target.
#[tauri::command]
pub fn take_pending_drop(state: State<'_, AppState>) -> Vec<String> {
    state
        .pending_drop
        .lock()
        .drain(..)
        .map(|path| path.to_string_lossy().to_string())
        .collect()
}

#[tauri::command]
pub fn refresh(state: State<'_, AppState>) -> Result<()> {
    state.engine.refresh().map_err(|err| err.to_string())
}

/// Platform specific notes the UI shows in the settings panel.
#[tauri::command]
pub fn platform_notes() -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(hint) = ud_input::permission_hint() {
        notes.push(hint);
    }
    if cfg!(target_os = "windows") {
        notes.push(
            "Windows may show a firewall prompt the first time UnionDesk listens for peers; \
             allow it on private networks."
                .into(),
        );
    }
    if !ud_input::input_relay_supported() {
        notes.push("Keyboard and mouse relaying is not implemented on this platform yet.".into());
    }
    notes
}

fn parse_side(value: &str) -> Result<ud_core::geom::Side> {
    match value {
        "left" => Ok(ud_core::geom::Side::Left),
        "right" => Ok(ud_core::geom::Side::Right),
        "top" => Ok(ud_core::geom::Side::Top),
        "bottom" => Ok(ud_core::geom::Side::Bottom),
        other => Err(format!("'{other}' is not a screen edge")),
    }
}

/// Accepts "192.168.1.4", "192.168.1.4:47823" and "[::1]:47823".
fn normalize_address(input: &str) -> String {
    let trimmed = input.trim();
    let has_port = if trimmed.starts_with('[') {
        trimmed.contains("]:")
    } else {
        trimmed.matches(':').count() == 1
    };
    if has_port {
        trimmed.to_string()
    } else {
        format!("{trimmed}:{}", ud_core::DEFAULT_PORT)
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_address;

    #[test]
    fn addresses_get_the_default_port() {
        assert_eq!(normalize_address("192.168.1.4"), "192.168.1.4:47823");
        assert_eq!(normalize_address("192.168.1.4:1000"), "192.168.1.4:1000");
        assert_eq!(normalize_address("[::1]:1000"), "[::1]:1000");
        assert_eq!(normalize_address(" ::1 "), "::1:47823");
    }
}
