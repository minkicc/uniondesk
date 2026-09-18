use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use parking_lot::{Mutex, RwLock};
use ud_engine::{EngineHandle, Snapshot};

/// Everything the Tauri commands need to reach the engine.
pub struct AppState {
    pub engine: EngineHandle,
    /// Latest snapshot, kept so a freshly opened window can render immediately
    /// instead of waiting for the next engine update.
    pub snapshot: RwLock<Option<Snapshot>>,
    /// Files dropped onto the window, waiting for the user to pick a target.
    pub pending_drop: Mutex<Vec<PathBuf>>,
    /// True while the application is really exiting rather than hiding.
    pub quitting: AtomicBool,
}

impl AppState {
    pub fn new(engine: EngineHandle) -> Self {
        AppState {
            engine,
            snapshot: RwLock::new(None),
            pending_drop: Mutex::new(Vec::new()),
            quitting: AtomicBool::new(false),
        }
    }
}
