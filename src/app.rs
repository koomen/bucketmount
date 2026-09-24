//! Shared application state and the commands the web UI invokes.

use crate::config::{self, Config, MountConfig};
use crate::mac;
use crate::rclone;
use crate::sso;
use crate::supervisor::{self, Manager};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager as _, State};

pub struct AppState {
    pub cfg: Mutex<Config>,
    pub config_error: Mutex<Option<String>>,
    pub manager: Mutex<Manager>,
    pub background: bool,
    /// The SSO sign-in in progress, if any (one at a time).
    pub login: Mutex<Option<Login>>,
}

pub struct Login {
    pub view: LoginView,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Serialize, Clone)]
pub struct LoginView {
    pub profile: String,
    /// Empty until AWS has handed out the code.
    pub user_code: String,
    pub url: String,
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
    pub sso_profile: Option<String>,
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
    pub sso_login: Option<LoginView>,
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
                sso_profile: s.sso_profile,
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
            sso_login: lock(&self.login).as_ref().map(|l| l.view.clone()),
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

/// Start an AWS SSO sign-in for the profile behind `mount` on a background
/// thread: the browser opens on the AWS approval page, and once approved the
/// token is cached and every supervisor re-checks its bucket.
pub fn start_sso_login(app: &AppHandle, mount: &MountConfig) -> Result<(), String> {
    let state = app.state::<AppState>();
    let rclone = lock(&state.manager).rclone().map(PathBuf::from);
    let sp = sso::for_mount(mount, rclone.as_deref()).ok_or_else(|| {
        "This mount does not use an AWS SSO profile. Set an AWS profile that has sso_session or sso_start_url in ~/.aws/config.".to_string()
    })?;

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut login = lock(&state.login);
        if let Some(l) = login.as_ref() {
            if l.view.profile != sp.profile {
                return Err(format!("A sign-in for profile '{}' is already in progress.", l.view.profile));
            }
            // Same profile: just bring the approval page back.
            if !l.view.url.is_empty() {
                mac::open_url(&l.view.url);
            }
            return Ok(());
        }
        *login = Some(Login {
            view: LoginView { profile: sp.profile.clone(), user_code: String::new(), url: String::new() },
            cancel: cancel.clone(),
        });
    }
    crate::applog::log(format!("SSO sign-in started for profile '{}'", sp.profile));
    let _ = app.emit("state-changed", ());

    let app = app.clone();
    std::thread::spawn(move || {
        let result = sso::login(&sp, &cancel, |code, url| {
            if let Some(l) = lock(&app.state::<AppState>().login).as_mut() {
                l.view.user_code = code.to_string();
                l.view.url = url.to_string();
            }
            mac::open_url(url);
            let _ = app.emit("state-changed", ());
        });
        *lock(&app.state::<AppState>().login) = None;
        match &result {
            Ok(()) => {
                crate::applog::log(format!("SSO sign-in for '{}' succeeded", sp.profile));
                supervisor::LOGIN_GENERATION.fetch_add(1, Ordering::SeqCst);
                mac::notify(config::APP_NAME, &format!("Signed in to AWS (profile {})", sp.profile));
                let _ = app.emit("toast", "Signed in to AWS");
            }
            Err(e) => {
                crate::applog::log(format!("SSO sign-in for '{}' failed: {e}", sp.profile));
                let _ = app.emit("toast-error", format!("AWS sign-in failed: {e}"));
            }
        }
        let _ = app.emit("state-changed", ());
        crate::tray::refresh(&app);
    });
    Ok(())
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

/// Sign in for a mount; takes the (possibly unsaved) mount from the editor.
#[tauri::command]
pub fn sso_login(app: AppHandle, mount: MountConfig) -> Result<(), String> {
    start_sso_login(&app, &mount)
}

#[tauri::command]
pub fn cancel_sso_login(state: State<'_, AppState>) {
    if let Some(l) = lock(&state.login).as_ref() {
        l.cancel.store(true, Ordering::SeqCst);
    }
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
