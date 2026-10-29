//! urh desktop GUI — Tauri 2 shell.
//!
//! Thin adapter per `docs/ipc-contract.md`: commands translate IPC arguments
//! into `urh_app::SignalRepository` calls and perform the IPC-side validation
//! (ids, ranges, numeric bounds); the heavy lifting stays in `crates/urh-app`.
//! Waveform data goes back as raw `tauri::ipc::Response` binary frames —
//! never JSON/base64 arrays (contract: "Rust 重任务在后台线程池执行").
//!
//! Security notes: capabilities are minimal (`core:default` + the file dialog
//! only); file access happens inside these Rust commands, so the frontend
//! holds no filesystem permissions. CSP is left null for the P0 slice because
//! it cannot be exercised on a headless machine; tightening it is on the P0
//! completion checklist (see docs/adr).

use std::sync::Arc;

use serde::Serialize;
use tauri::State;
use urh_app::{SignalMeta, SignalRepository, WaveformMode};

/// Process-wide state: one signal repository shared by all commands.
struct AppState {
    repo: Arc<SignalRepository>,
}

/// IPC error shape. The frontend branches on `code` and shows `message`;
/// neither is parsed the other way around (contract).
#[derive(Debug, Serialize)]
pub struct CmdError {
    pub code: String,
    pub message: String,
}

impl CmdError {
    fn invalid_argument(message: String) -> Self {
        CmdError {
            code: "invalid-argument".to_string(),
            message,
        }
    }

    fn internal(message: String) -> Self {
        CmdError {
            code: "internal".to_string(),
            message,
        }
    }
}

impl From<urh_app::AppError> for CmdError {
    fn from(e: urh_app::AppError) -> Self {
        CmdError {
            code: e.code.to_string(),
            message: e.message,
        }
    }
}

/// IPC-side validation of the mode code (contract: 1 = Iq, 2 = Amplitude).
fn wave_mode(mode: u32) -> Result<WaveformMode, CmdError> {
    match mode {
        1 => Ok(WaveformMode::Iq),
        2 => Ok(WaveformMode::Amplitude),
        _ => Err(CmdError::invalid_argument(format!(
            "unknown waveform mode {mode} (use 1=Iq, 2=Amplitude)"
        ))),
    }
}

/// Run a repository call on the blocking pool so neither the Tauri main
/// thread nor the async runtime is stalled by file I/O or envelope work.
async fn blocking<T, F>(repo: &Arc<SignalRepository>, f: F) -> Result<T, CmdError>
where
    T: Send + 'static,
    F: FnOnce(&SignalRepository) -> Result<T, urh_app::AppError> + Send + 'static,
{
    let repo = Arc::clone(repo);
    tauri::async_runtime::spawn_blocking(move || f(&repo))
        .await
        .map_err(|e| CmdError::internal(format!("background task failed: {e}")))?
        .map_err(CmdError::from)
}

#[tauri::command(rename_all = "snake_case")]
async fn open_signal(
    state: State<'_, AppState>,
    path: String,
    sample_rate: Option<f64>,
) -> Result<SignalMeta, CmdError> {
    blocking(&state.repo, move |repo| repo.open(&path, sample_rate)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn close_signal(state: State<'_, AppState>, signal_id: String) -> Result<(), CmdError> {
    blocking(&state.repo, move |repo| repo.close(&signal_id)).await
}

// Sync on purpose: a cheap map snapshot, and Tauri requires async commands
// that borrow State to return Result.
#[tauri::command(rename_all = "snake_case")]
fn list_signals(state: State<AppState>) -> Vec<SignalMeta> {
    state.repo.list()
}

#[tauri::command(rename_all = "snake_case")]
async fn set_sample_rate(
    state: State<'_, AppState>,
    signal_id: String,
    sample_rate: f64,
) -> Result<SignalMeta, CmdError> {
    blocking(&state.repo, move |repo| {
        repo.set_sample_rate(&signal_id, sample_rate)
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
async fn get_waveform_envelope(
    state: State<'_, AppState>,
    signal_id: String,
    start: u64,
    end: u64,
    bins: u32,
    mode: u32,
    request_id: u32,
) -> Result<tauri::ipc::Response, CmdError> {
    let mode = wave_mode(mode)?;
    let frame = blocking(&state.repo, move |repo| {
        repo.waveform_envelope(&signal_id, start, end, bins, mode, request_id)
    })
    .await?;
    Ok(tauri::ipc::Response::new(frame))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            repo: Arc::new(SignalRepository::new()),
        })
        .invoke_handler(tauri::generate_handler![
            open_signal,
            close_signal,
            list_signals,
            set_sample_rate,
            get_waveform_envelope
        ])
        .run(tauri::generate_context!())
        .expect("error while running urh desktop");
}
