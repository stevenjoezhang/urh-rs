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
use urh_app::{
    AnalysisParams, AnalysisResult, CaptureConfig, CaptureHandle, CaptureStats, DeviceInfo,
    FieldLabel, GeneratorParams, ProjectState, SignalMeta, SignalRepository, SimRuleInfo,
    SimTranscriptEntry, WaveformMode,
};

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

// ---- v2 commands (docs/ipc-contract.md "v2 additions") ----

/// IPC-side range validation: both endpoints present and ordered, or neither.
/// The repository re-validates against the signal's actual length.
fn opt_range(
    range_start: Option<u64>,
    range_end: Option<u64>,
) -> Result<Option<(u64, u64)>, CmdError> {
    match (range_start, range_end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) if start < end => Ok(Some((start, end))),
        (Some(start), Some(end)) => Err(CmdError::invalid_argument(format!(
            "range must satisfy start < end, got [{start}, {end})"
        ))),
        _ => Err(CmdError::invalid_argument(
            "range_start and range_end must be given together".to_string(),
        )),
    }
}

#[tauri::command(rename_all = "snake_case")]
async fn get_spectrogram_tile(
    state: State<'_, AppState>,
    signal_id: String,
    start: u64,
    end: u64,
    freq_bins: u32,
    window_size: u32,
    request_id: u32,
) -> Result<tauri::ipc::Response, CmdError> {
    let frame = blocking(&state.repo, move |repo| {
        repo.spectrogram_tile(&signal_id, start, end, freq_bins, window_size, request_id)
    })
    .await?;
    Ok(tauri::ipc::Response::new(frame))
}

#[tauri::command(rename_all = "snake_case")]
async fn analyze_signal(
    state: State<'_, AppState>,
    signal_id: String,
    range_start: Option<u64>,
    range_end: Option<u64>,
    params: AnalysisParams,
    job_id: u32,
) -> Result<AnalysisResult, CmdError> {
    let range = opt_range(range_start, range_end)?;
    blocking(&state.repo, move |repo| {
        repo.analyze(&signal_id, range, &params, job_id)
    })
    .await
}

// Sync on purpose: sets a cancellation flag; no blocking work (and Tauri
// requires async + State to return Result, which a plain bool avoids).
#[tauri::command(rename_all = "snake_case")]
fn cancel_analysis(state: State<AppState>, job_id: u32) -> bool {
    state.repo.cancel_analysis(job_id)
}

#[tauri::command(rename_all = "snake_case")]
async fn run_awre(
    state: State<'_, AppState>,
    signal_id: String,
    range_start: Option<u64>,
    range_end: Option<u64>,
    params: AnalysisParams,
) -> Result<Vec<FieldLabel>, CmdError> {
    let range = opt_range(range_start, range_end)?;
    blocking(&state.repo, move |repo| {
        repo.run_awre(&signal_id, range, &params)
    })
    .await
}

// ---- v2 device capture + generator commands ----

/// Flattened `capture://stats` payload: `{capture_id, …CaptureStats}`.
/// Owned (`emit` requires `Clone + Serialize`).
#[derive(Serialize, Clone)]
struct StatsEvent {
    capture_id: String,
    #[serde(flatten)]
    stats: CaptureStats,
}

/// Forward a capture's preview batches and 1 Hz stats snapshots to the
/// frontend as `capture://batch` / `capture://stats` events (contract).
///
/// Two plain threads per capture: the batch poller owns the single
/// [`urh_app::devices::PreviewReceiver`] and exits when the capture ends
/// (`PreviewClosed`); the stats poller exits when the capture id disappears
/// from the repository. Batches are base64-encoded because Tauri 2 events
/// are always JSON-serialized — raw `ArrayBuffer`s cannot ride `emit`.
fn spawn_capture_forwarders(
    app: tauri::AppHandle,
    repo: Arc<SignalRepository>,
    capture_id: String,
) {
    use std::time::Duration;
    use tauri::Emitter;

    if let Some(receiver) = repo.take_preview_receiver(&capture_id) {
        let app = app.clone();
        let id = capture_id.clone();
        std::thread::spawn(move || {
            use base64::Engine;
            let engine = base64::engine::general_purpose::STANDARD;
            loop {
                match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(Some(batch)) => {
                        let payload = serde_json::json!({
                            "capture_id": id,
                            "seq": batch.seq,
                            "body": engine.encode(&batch.body),
                        });
                        // A failed emit (window gone) must not kill the
                        // poller: batches keep draining until capture stop.
                        let _ = app.emit("capture://batch", payload);
                    }
                    Ok(None) => continue,
                    Err(_) => break, // capture ended, queue drained
                }
            }
        });
    }

    let app = app.clone();
    let id = capture_id.clone();
    std::thread::spawn(move || {
        use tauri::Emitter;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            match repo.capture_stats(&id) {
                Ok(stats) => {
                    let _ = app.emit(
                        "capture://stats",
                        StatsEvent {
                            capture_id: id.clone(),
                            stats,
                        },
                    );
                }
                // Unknown capture id → stopped (and possibly unregistered);
                // a fresh start_capture spawns fresh forwarders.
                Err(_) => break,
            }
        }
    });
}

#[tauri::command(rename_all = "snake_case")]
async fn list_devices(state: State<'_, AppState>) -> Result<Vec<DeviceInfo>, CmdError> {
    blocking(&state.repo, |repo| Ok(repo.list_devices())).await
}

#[tauri::command(rename_all = "snake_case")]
async fn start_capture(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    config: CaptureConfig,
) -> Result<CaptureHandle, CmdError> {
    let handle = blocking(&state.repo, move |repo| repo.start_capture(&config)).await?;
    spawn_capture_forwarders(app, Arc::clone(&state.repo), handle.capture_id.clone());
    Ok(handle)
}

#[tauri::command(rename_all = "snake_case")]
async fn stop_capture(
    state: State<'_, AppState>,
    capture_id: String,
) -> Result<CaptureStats, CmdError> {
    blocking(&state.repo, move |repo| repo.stop_capture(&capture_id)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn capture_stats(
    state: State<'_, AppState>,
    capture_id: String,
) -> Result<CaptureStats, CmdError> {
    blocking(&state.repo, move |repo| repo.capture_stats(&capture_id)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn generate(
    state: State<'_, AppState>,
    params: GeneratorParams,
) -> Result<SignalMeta, CmdError> {
    blocking(&state.repo, move |repo| repo.generate(&params)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn save_signal_as(
    state: State<'_, AppState>,
    signal_id: String,
    path: String,
) -> Result<(), CmdError> {
    blocking(&state.repo, move |repo| {
        repo.save_signal_as(&signal_id, &path)
    })
    .await
}

// ---- v2 project + simulator commands ----

// The payload key for the project body is `state` (contract), so the injected
// Tauri `State` is bound under a different name in these commands. Tauri
// injects `State` by type; the payload parameter is matched by its name.

#[tauri::command(rename_all = "snake_case")]
async fn save_project(
    ctx: State<'_, AppState>,
    path: String,
    state: ProjectState,
) -> Result<(), CmdError> {
    blocking(&ctx.repo, move |repo| repo.save_project(&path, &state)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn load_project(ctx: State<'_, AppState>, path: String) -> Result<ProjectState, CmdError> {
    blocking(&ctx.repo, move |repo| repo.load_project(&path)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn load_simulator(
    ctx: State<'_, AppState>,
    path: String,
) -> Result<Vec<SimRuleInfo>, CmdError> {
    blocking(&ctx.repo, move |repo| repo.load_simulator(&path)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn run_simulation(
    ctx: State<'_, AppState>,
    path: String,
    timeout_secs: u64,
) -> Result<Vec<SimTranscriptEntry>, CmdError> {
    blocking(&ctx.repo, move |repo| {
        repo.run_simulation(&path, timeout_secs)
    })
    .await
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
            get_waveform_envelope,
            get_spectrogram_tile,
            analyze_signal,
            cancel_analysis,
            run_awre,
            list_devices,
            start_capture,
            stop_capture,
            capture_stats,
            generate,
            save_signal_as,
            save_project,
            load_project,
            load_simulator,
            run_simulation
        ])
        .run(tauri::generate_context!())
        .expect("error while running urh desktop");
}
