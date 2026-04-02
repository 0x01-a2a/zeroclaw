// zeroclaw/src/ffi.rs
//
// iOS in-process FFI — called from NodeService.swift via the C bridging header:
//
//   int32_t zeroclaw_start(const char *config_path,
//                           const char *node_api_url,
//                           const char *llm_api_key,    // nullable
//                           const char *data_dir);       // nullable
//   int32_t zeroclaw_stop(void);
//   int32_t zeroclaw_set_busy(const char *dir);  // nullable
//   int32_t zeroclaw_set_idle(const char *dir);  // nullable
//
// iOS kernel sandbox blocks exec*()/posix_spawn(); zeroclaw must run
// in-process. The daemon is started on 127.0.0.1:9093 to avoid clashing
// with zerox1-node (9090) and the phone bridge server (9092).
//
// Compile with `--features ios-ffi`.

// This module intentionally uses raw pointers passed across the C boundary.
// All unsafe blocks are carefully reviewed; no unsafe code leaks to callers.
#![allow(unsafe_code)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::CStr;
use std::io::Write as _;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

/// Tracks whether a zeroclaw daemon is currently running in-process.
static IS_RUNNING: AtomicBool = AtomicBool::new(false);

/// Holds the tokio Runtime that drives the daemon.  Dropping (or explicitly
/// shutting down) the runtime cancels all tasks, stopping the daemon.
static RUNTIME: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);

/// Directory used for the `zeroclaw.busy` sentinel file.
/// Set once during `zeroclaw_start`; cleared on `zeroclaw_stop`.
static DATA_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Guard so tracing is only initialised once per process lifetime.
static TRACING_INIT: OnceLock<()> = OnceLock::new();

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a (possibly null) C string pointer to an `Option<String>`.
///
/// # Safety
/// `ptr` must be either null or a valid NUL-terminated C string that remains
/// valid for the duration of this call.
unsafe fn cstr_to_opt(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: caller guarantees ptr is a valid, NUL-terminated C string.
        unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .ok()
            .map(str::to_string)
    }
}

/// Write `$data_dir/zeroclaw.busy` using the currently stored DATA_DIR.
/// Silently ignores errors (best-effort IPC).
fn write_busy_file() {
    if let Some(ref dir) = *DATA_DIR.lock().unwrap() {
        let path = dir.join("zeroclaw.busy");
        // An empty file is sufficient — presence is the signal.
        let _ = std::fs::write(&path, b"");
    }
}

/// Remove `$data_dir/zeroclaw.busy` using the currently stored DATA_DIR.
/// Silently ignores errors (best-effort IPC).
fn delete_busy_file() {
    if let Some(ref dir) = *DATA_DIR.lock().unwrap() {
        let path = dir.join("zeroclaw.busy");
        let _ = std::fs::remove_file(&path);
    }
}

/// Write `$data_dir/zeroclaw.busy` using an explicit directory path.
fn write_busy_file_at(dir: &std::path::Path) {
    let _ = std::fs::write(dir.join("zeroclaw.busy"), b"");
}

/// Remove `$data_dir/zeroclaw.busy` using an explicit directory path.
fn delete_busy_file_at(dir: &std::path::Path) {
    let _ = std::fs::remove_file(dir.join("zeroclaw.busy"));
}

/// Append a timestamped line to `$data_dir/zeroclaw_ffi.log`.
/// Used for diagnosing iOS-specific startup failures where stderr is not
/// easily accessible.  Silently ignores I/O errors (best-effort).
fn ffi_log(msg: &str) {
    eprintln!("[zeroclaw-ffi] {msg}");
    if let Some(ref dir) = *DATA_DIR.lock().unwrap() {
        let path = dir.join("zeroclaw_ffi.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = writeln!(f, "[{ts}] {msg}");
        }
    }
}

/// Initialise a `tracing_subscriber` that writes to stderr (device log on iOS)
/// and to `$data_dir/zeroclaw_ffi.log`.  Called once via `TRACING_INIT`.
fn init_tracing(data_dir: &std::path::Path) {
    let log_path = data_dir.join("zeroclaw_ffi.log");
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            // Write to file; also mirror to stderr which appears in Xcode device log.
            let _ = tracing_subscriber::fmt()
                .with_writer(Mutex::new(file))
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .try_init();
        }
        Err(_) => {
            // Fall back to stderr only.
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .try_init();
        }
    }
}

// ---------------------------------------------------------------------------
// Public C API
// ---------------------------------------------------------------------------

/// Start the zeroclaw daemon in a background tokio runtime.
///
/// # Parameters
/// - `config_path`  — path to the TOML config file written by `NodeService.swift`.
/// - `node_api_url` — base URL of the zerox1-node REST API
///                    (e.g. `"http://127.0.0.1:9090"`).
/// - `llm_api_key`  — LLM API key (nullable).  When non-null it is exported as
///                    `ZEROCLAW_API_KEY` before config is loaded; the config
///                    layer always lets this env var win over any file value.
/// - `data_dir`     — (nullable) directory where `zeroclaw.busy` sentinel file
///                    is written while the daemon is running.  Used by the iOS
///                    `KeepAliveService` to decide whether to hold an audio
///                    session.  When null no sentinel file is managed.
///
/// Returns `0` on success (or if already running), `-1` on failure.
///
/// # Safety
/// All non-null pointer arguments must point to valid NUL-terminated C strings
/// that remain valid for the duration of this call.
#[no_mangle]
pub extern "C" fn zeroclaw_start(
    config_path: *const c_char,
    node_api_url: *const c_char,
    llm_api_key: *const c_char,
    data_dir: *const c_char,
) -> i32 {
    // Idempotent — return success if already running.
    if IS_RUNNING.load(Ordering::SeqCst) {
        return 0;
    }

    // ── Parse C strings while still on the Swift/ObjC thread ────────────────
    // SAFETY: Swift guarantees these are valid NUL-terminated strings (or null)
    // for the duration of this call.
    let config_path_str = match unsafe { cstr_to_opt(config_path) } {
        Some(s) => s,
        None => {
            eprintln!("[zeroclaw-ffi] config_path must not be null");
            return -1;
        }
    };
    let node_api_url_str = unsafe { cstr_to_opt(node_api_url) }
        .unwrap_or_else(|| "http://127.0.0.1:9090".to_string());
    let llm_api_key_opt = unsafe { cstr_to_opt(llm_api_key) };
    let data_dir_opt = unsafe { cstr_to_opt(data_dir) }.map(PathBuf::from);

    // Store the data_dir globally for use by write_busy_file / delete_busy_file.
    *DATA_DIR.lock().unwrap() = data_dir_opt.clone();

    // ── Install tracing subscriber (once per process) ────────────────────────
    if let Some(ref dir) = data_dir_opt {
        TRACING_INIT.get_or_init(|| init_tracing(dir));
    } else {
        TRACING_INIT.get_or_init(|| {
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .try_init();
        });
    }

    // ── Export API key as env var before Config::load_or_init ───────────────
    // `apply_env_overrides()` (called inside load_or_init) treats
    // ZEROCLAW_API_KEY as the highest-priority source, overriding any value
    // that may be present in the TOML file.
    if let Some(ref key) = llm_api_key_opt {
        // SAFETY: std::env::set_var is safe to call from a single-threaded
        // context.  This executes before we spawn the runtime thread.
        std::env::set_var("ZEROCLAW_API_KEY", key);
    }

    // ── Build tokio runtime ─────────────────────────────────────────────────
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[zeroclaw-ffi] failed to build tokio runtime: {e}");
            return -1;
        }
    };

    // ── Spawn the daemon task ───────────────────────────────────────────────
    // Write the busy sentinel so KeepAliveService starts the audio session
    // before the first async tick — no gap between start() and daemon ready.
    write_busy_file();

    rt.spawn(async move {
        ffi_log("async task started — reading config");

        // Load config from the TOML file written by NodeService.swift.
        let contents = match tokio::fs::read_to_string(&config_path_str).await {
            Ok(c) => c,
            Err(e) => {
                ffi_log(&format!("FATAL: failed to read config file {config_path_str}: {e}"));
                IS_RUNNING.store(false, Ordering::SeqCst);
                delete_busy_file();
                return;
            }
        };

        ffi_log(&format!("config file read ({} bytes)", contents.len()));

        let mut config: crate::config::Config = match toml::from_str(&contents) {
            Ok(c) => c,
            Err(e) => {
                ffi_log(&format!("FATAL: failed to parse config file: {e}"));
                IS_RUNNING.store(false, Ordering::SeqCst);
                delete_busy_file();
                return;
            }
        };

        // Set computed path fields that are skipped during TOML serialization.
        config.config_path = std::path::PathBuf::from(&config_path_str);

        // Derive workspace_dir next to the config file when not set.
        if config.workspace_dir == std::path::PathBuf::default() {
            if let Some(parent) = std::path::Path::new(&config_path_str).parent() {
                config.workspace_dir = parent.join("workspace");
            }
        }

        ffi_log(&format!("workspace_dir={}", config.workspace_dir.display()));

        // Apply env overrides (ZEROCLAW_API_KEY etc.) now that config is loaded.
        config.apply_env_overrides();

        ffi_log(&format!(
            "config ready: provider={:?} api_key={} workspace={}",
            config.default_provider,
            if config.api_key.is_some() { "SET" } else { "NONE" },
            config.workspace_dir.display(),
        ));

        // Override the node_api_url used by the zerox1 channel so it points to
        // the running zerox1-node instance on this device.
        if let Some(ref mut zerox1_cfg) = config.channels_config.zerox1 {
            zerox1_cfg.node_api_url = node_api_url_str.clone();
        }

        ffi_log("calling daemon::run on 127.0.0.1:9093");

        // Run the daemon on port 9093 (avoids clash with node:9090 / bridge:9092).
        // Use std::future::pending() as the shutdown signal: the in-process model
        // never needs Ctrl+C — the daemon runs until zeroclaw_stop() drops the runtime.
        if let Err(e) = crate::daemon::run(config, "127.0.0.1".to_string(), 9093,
                                            std::future::pending::<()>()).await {
            ffi_log(&format!("daemon::run error: {e}"));
        } else {
            ffi_log("daemon::run exited cleanly (runtime shutdown)");
        }

        IS_RUNNING.store(false, Ordering::SeqCst);
        ffi_log("IS_RUNNING=false, busy file removed");
        // Daemon exited — remove the sentinel so KeepAliveService stops audio.
        delete_busy_file();
    });

    // Store the runtime so it stays alive — dropping it would cancel all tasks.
    *RUNTIME.lock().unwrap() = Some(rt);
    IS_RUNNING.store(true, Ordering::SeqCst);
    0
}

/// Stop the daemon by shutting down the tokio runtime.
///
/// Blocks up to 3 seconds for graceful shutdown, then forcibly terminates.
/// Returns `0`.
#[no_mangle]
pub extern "C" fn zeroclaw_stop() -> i32 {
    if let Some(rt) = RUNTIME.lock().unwrap().take() {
        rt.shutdown_timeout(Duration::from_secs(3));
    }
    IS_RUNNING.store(false, Ordering::SeqCst);
    delete_busy_file();
    *DATA_DIR.lock().unwrap() = None;
    0
}

/// Write the `zeroclaw.busy` sentinel file in `dir` (non-null C string).
///
/// Called from NodeModule.swift's `notifyAgentBusy` to mark the start of a
/// task from the JS layer (e.g. on receipt of a PROPOSE/ACCEPT message).
/// Pass the same `data_dir` supplied to `zeroclaw_start`.
///
/// # Safety
/// `dir` must be a valid NUL-terminated C string for the duration of this call.
#[no_mangle]
pub extern "C" fn zeroclaw_set_busy(dir: *const c_char) -> i32 {
    if let Some(path_str) = unsafe { cstr_to_opt(dir) } {
        write_busy_file_at(std::path::Path::new(&path_str));
        0
    } else {
        // Fall back to globally stored DATA_DIR.
        write_busy_file();
        0
    }
}

/// Remove the `zeroclaw.busy` sentinel file in `dir` (non-null C string).
///
/// Called from NodeModule.swift's `notifyAgentIdle` when a task completes.
///
/// # Safety
/// `dir` must be a valid NUL-terminated C string for the duration of this call.
#[no_mangle]
pub extern "C" fn zeroclaw_set_idle(dir: *const c_char) -> i32 {
    if let Some(path_str) = unsafe { cstr_to_opt(dir) } {
        delete_busy_file_at(std::path::Path::new(&path_str));
        0
    } else {
        delete_busy_file();
        0
    }
}
