//! Shared application state and the commands the web UI invokes.

use crate::config::{self, Config, MountConfig};
use crate::mac;
use crate::rclone;
use crate::supervisor::{self, Manager};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager as _, State};

pub struct AppState {
    pub cfg: Mutex<Config>,
    pub config_error: Mutex<Option<String>>,
    pub manager: Mutex<Manager>,
    pub background: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Serialize, Clone)]
pub struct MountView {
    pub config: MountConfig,
    pub state: supervisor::State,
    pub state_label: &'static str,
    pub color: String,
    pub detail: String,
    pub mounted: bool,
    pub uploads_pending: u64,
    pub restarts: u32,
    pub pid: Option<u32>,
    pub last_ok_secs: Option<u64>,
    pub mount_path: String,
    pub mount_path_short: String,
}

#[derive(Serialize, Clone)]
pub struct Snapshot {
    pub mounts: Vec<MountView>,
    pub start_at_login: Option<bool>,
    pub config_error: Option<String>,
    pub config_path: String,
    pub logs_dir: String,
    pub rclone_path: Option<String>,
    pub rclone_version: Option<String>,
    pub show_login_prompt: bool,
    pub version: &'static str,
}

impl AppState {
    pub fn snapshot(&self) -> Snapshot {
        let cfg = lock(&self.cfg);
        let manager = lock(&self.manager);
        let config_error = lock(&self.config_error).clone();
        let mounts = manager
            .statuses(&cfg)
            .into_iter()
            .map(|(m, s)| MountView {
                state_label: s.state.label(),
                color: s.state.hex(),
                state: s.state,
                detail: s.detail,
                mounted: s.mounted,
                uploads_pending: s.uploads_pending,
                restarts: s.restarts,
                pid: s.pid,
                last_ok_secs: s.last_ok.map(|t| t.elapsed().as_secs()),
                mount_path: m.mount_path().display().to_string(),
                mount_path_short: config::collapse_tilde(&m.mount_path()),
                config: m,
            })
            .collect();
        let rclone_path = manager.rclone().map(PathBuf::from);
        Snapshot {
            mounts,
            start_at_login: cfg.start_at_login,
            show_login_prompt: cfg.start_at_login.is_none() && config_error.is_none() && !self.background,
            config_error,
            config_path: config::collapse_tilde(&config::config_path()),
            logs_dir: config::collapse_tilde(&config::logs_dir()),
            rclone_version: rclone_path.as_deref().and_then(rclone::version),
            rclone_path: rclone_path.map(|p| config::collapse_tilde(&p)),
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// Persist the config and reconcile supervisors with it.
    fn commit(&self, app: &AppHandle) -> Result<(), String> {
        if let Some(e) = lock(&self.config_error).as_ref() {
            return Err(format!("The config file could not be read ({e}). Fix or delete it before saving."));
        }
        let cfg = lock(&self.cfg).clone();
        config::save(&cfg)?;
        lock(&self.manager).apply(&cfg);
        let _ = app.emit("state-changed", ());
        Ok(())
    }
}

/// Convenience for code that only has an `AppHandle`.
pub fn lock_cfg(app: &AppHandle) -> Config {
    lock(&app.state::<AppState>().cfg).clone()
}

pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

// ------------------------------------------------------------------ commands

#[tauri::command]
pub fn snapshot(state: State<'_, AppState>) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
pub fn default_mount_point(name: String) -> String {
    config::default_mount_point(&name)
}

#[tauri::command]
pub fn save_mount(
    app: AppHandle,
    state: State<'_, AppState>,
    mount: MountConfig,
    original_name: Option<String>,
) -> Result<(), String> {
    let mut m = mount;
    m.name = m.name.trim().to_string();
    m.bucket = m.bucket.trim().to_string();
    m.prefix = m.prefix.trim().trim_matches('/').to_string();
    m.mount_point = m.mount_point.trim().to_string();
    m.region = m.region.trim().to_string();
    m.endpoint = m.endpoint.trim().to_string();
    m.rclone_remote = m.rclone_remote.trim().to_string();
    m.access_key_id = m.access_key_id.trim().to_string();
    m.secret_access_key = m.secret_access_key.trim().to_string();
    if m.cache_max_size.trim().is_empty() {
        m.cache_max_size = "10G".into();
    }
    {
        let mut cfg = lock(&state.cfg);
        let idx = original_name.as_deref().and_then(|n| cfg.mounts.iter().position(|x| x.name == n));
        let others: Vec<MountConfig> = cfg
            .mounts
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != idx)
            .map(|(_, x)| x.clone())
            .collect();
        m.validate(&others)?;
        match idx {
            Some(i) => cfg.mounts[i] = m,
            None => cfg.mounts.push(m),
        }
    }
    state.commit(&app)
}

#[tauri::command]
pub fn delete_mount(app: AppHandle, state: State<'_, AppState>, name: String) -> Result<(), String> {
    {
        let mut cfg = lock(&state.cfg);
        let before = cfg.mounts.len();
        cfg.mounts.retain(|m| m.name != name);
        if cfg.mounts.len() == before {
            return Err(format!("No mount named '{name}'."));
        }
    }
    state.commit(&app)
}

#[tauri::command]
pub fn set_start_at_login(app: AppHandle, state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    if enabled {
        mac::install_login_item()?;
    } else {
        mac::remove_login_item()?;
    }
    lock(&state.cfg).start_at_login = Some(enabled);
    state.commit(&app)
}

/// Runs a real listing request against the bucket; returns the number of
/// top-level entries.
#[tauri::command]
pub async fn test_connection(state: State<'_, AppState>, mount: MountConfig) -> Result<usize, String> {
    let rclone = lock(&state.manager)
        .rclone()
        .map(PathBuf::from)
        .ok_or_else(|| "rclone not found".to_string())?;
    let inv = rclone::invocation(&rclone, &mount);
    tauri::async_runtime::spawn_blocking(move || rclone::check_connectivity(&inv, Duration::from_secs(40)))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn restart_mount(app: AppHandle, state: State<'_, AppState>, name: String) {
    let cfg = lock(&state.cfg).clone();
    lock(&state.manager).restart(&name, &cfg);
    let _ = app.emit("state-changed", ());
}

#[tauri::command]
pub fn log_tail(state: State<'_, AppState>, name: String) -> Vec<String> {
    lock(&state.manager)
        .status(&name)
        .map(|s| s.log_tail.iter().rev().take(80).cloned().collect::<Vec<_>>().into_iter().rev().collect())
        .unwrap_or_default()
}

#[tauri::command]
pub fn open_mount(state: State<'_, AppState>, name: String) {
    if let Some(m) = lock(&state.cfg).mounts.iter().find(|m| m.name == name) {
        mac::open_in_finder(&m.mount_path());
    }
}

#[tauri::command]
pub fn show_log(name: String) {
    mac::reveal_in_finder(&supervisor::log_path(&name));
}

#[tauri::command]
pub fn reveal_config() {
    let p = config::config_path();
    if p.exists() {
        mac::reveal_in_finder(&p);
    } else {
        let _ = std::fs::create_dir_all(config::config_dir());
        mac::open_in_finder(&config::config_dir());
    }
}

#[tauri::command]
pub fn open_logs() {
    let _ = std::fs::create_dir_all(config::logs_dir());
    mac::open_in_finder(&config::logs_dir());
}

#[tauri::command]
pub fn debug_log(msg: String) {
    crate::applog::log(format!("[ui] {msg}"));
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    app.exit(0);
}
