//! One supervisor thread per mount. It starts `rclone nfsmount`, watches the
//! process, the mount table, rclone's remote-control API and the bucket
//! itself, and restarts everything with backoff when anything goes wrong.

use crate::applog::log;
use crate::config::{self, Config, MountConfig};
use crate::mac;
use crate::rclone::{self, Invocation, VfsStats};
use crate::sso::{self, SsoProfile};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Set from the SIGTERM/SIGINT handler; every supervisor tears down when it sees it.
pub static TERMINATE: AtomicBool = AtomicBool::new(false);

/// Bumped after every successful SSO sign-in; supervisors re-check the bucket
/// straight away instead of waiting for the next health check.
pub static LOGIN_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Called whenever any mount's status changes, so the UI and tray can refresh.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

const LOG_TAIL_LINES: usize = 200;
const MOUNT_APPEAR_TIMEOUT: Duration = Duration::from_secs(45);
const UNMOUNTED_GRACE: Duration = Duration::from_secs(8);
const RC_HUNG_AFTER: Duration = Duration::from_secs(90);
const RC_POLL: Duration = Duration::from_secs(5);
const HEALTHY_RESET: Duration = Duration::from_secs(300);
const NOTIFY_THROTTLE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Disabled,
    Starting,
    Connected,
    Syncing,
    /// Mounted, but the bucket cannot be reached right now.
    Disconnected,
    /// The AWS SSO session has expired; the user has to sign in again.
    SignInRequired,
    /// The rclone process is not running; a restart is pending.
    Down,
    /// A configuration / environment problem that a restart will not fix.
    Error,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Disabled => "Disabled",
            State::Starting => "Mounting",
            State::Connected => "Connected",
            State::Syncing => "Syncing",
            State::Disconnected => "Connection lost",
            State::SignInRequired => "Sign-in required",
            State::Down => "Mount down",
            State::Error => "Error",
        }
    }

    pub fn is_problem(self) -> bool {
        matches!(self, State::Disconnected | State::SignInRequired | State::Down | State::Error)
    }

    pub fn is_healthy(self) -> bool {
        matches!(self, State::Connected | State::Syncing)
    }

    /// Higher = worse; used to pick the menu bar icon colour.
    pub fn severity(self) -> u8 {
        match self {
            State::Disabled => 0,
            State::Connected => 1,
            State::Syncing => 2,
            State::Starting => 3,
            State::Disconnected | State::SignInRequired | State::Down | State::Error => 4,
        }
    }

    pub fn hex(self) -> String {
        let (r, g, b) = self.rgb();
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            State::Disabled => (142, 142, 147),
            State::Connected => (52, 199, 89),
            State::Syncing => (10, 132, 255),
            State::Starting => (255, 159, 10),
            State::Disconnected | State::SignInRequired | State::Down | State::Error => (255, 69, 58),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MountStatus {
    pub state: State,
    pub detail: String,
    pub mounted: bool,
    pub uploads_pending: u64,
    pub errored_files: u64,
    pub restarts: u32,
    pub pid: Option<u32>,
    pub since: Instant,
    pub last_ok: Option<Instant>,
    pub last_error: Option<String>,
    pub log_tail: VecDeque<String>,
    /// The AWS SSO profile behind this mount, if its credentials come from one.
    pub sso_profile: Option<String>,
}

impl MountStatus {
    fn new(state: State, detail: &str) -> Self {
        Self {
            state,
            detail: detail.to_string(),
            mounted: false,
            uploads_pending: 0,
            errored_files: 0,
            restarts: 0,
            pid: None,
            since: Instant::now(),
            last_ok: None,
            last_error: None,
            log_tail: VecDeque::new(),
            sso_profile: None,
        }
    }
}

pub type SharedStatus = Arc<Mutex<MountStatus>>;

fn lock(s: &SharedStatus) -> std::sync::MutexGuard<'_, MountStatus> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Manager: owns one handle per configured mount
// ---------------------------------------------------------------------------

struct Handle {
    cfg: MountConfig,
    status: SharedStatus,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub struct Manager {
    handles: HashMap<String, Handle>,
    notify: Notify,
    rclone: Option<PathBuf>,
}

impl Manager {
    pub fn new(notify: Notify, rclone: Option<PathBuf>) -> Self {
        Self { handles: HashMap::new(), notify, rclone }
    }

    pub fn rclone(&self) -> Option<&Path> {
        self.rclone.as_deref()
    }

    /// Bring the running supervisors in line with `cfg`: start new/changed
    /// mounts, stop removed/disabled ones, leave unchanged ones alone.
    pub fn apply(&mut self, cfg: &Config) {
        let rclone = rclone::locate(cfg.rclone_path.as_deref());
        let rclone_changed = rclone != self.rclone;
        self.rclone = rclone;
        let interval = Duration::from_secs(cfg.health_check_interval_secs.max(5));

        let wanted: HashMap<String, &MountConfig> = cfg.mounts.iter().map(|m| (m.name.clone(), m)).collect();

        // Stop what is gone or changed.
        let to_stop: Vec<String> = self
            .handles
            .iter()
            .filter(|(name, h)| match wanted.get(*name) {
                None => true,
                Some(m) => **m != h.cfg || rclone_changed,
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in to_stop {
            self.stop_one(&name);
        }

        // Start what is missing.
        for m in &cfg.mounts {
            if self.handles.contains_key(&m.name) {
                continue;
            }
            let handle = if m.enabled {
                self.spawn(m.clone(), interval)
            } else {
                Handle {
                    cfg: m.clone(),
                    status: Arc::new(Mutex::new(MountStatus::new(State::Disabled, "Disabled in settings"))),
                    stop: Arc::new(AtomicBool::new(false)),
                    thread: None,
                }
            };
            self.handles.insert(m.name.clone(), handle);
        }
        (self.notify)();
    }

    fn spawn(&self, cfg: MountConfig, health_interval: Duration) -> Handle {
        let status = Arc::new(Mutex::new(MountStatus::new(State::Starting, "Starting…")));
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Ctx {
            name: cfg.name.clone(),
            status: status.clone(),
            stop: stop.clone(),
            notify: self.notify.clone(),
            last_notify: None,
            ever_healthy: false,
        };
        let rclone = self.rclone.clone();
        let cfg_for_thread = cfg.clone();
        let thread = std::thread::Builder::new()
            .name(format!("mount-{}", cfg.name))
            .spawn(move || run(cfg_for_thread, ctx, rclone, health_interval))
            .ok();
        Handle { cfg, status, stop, thread }
    }

    fn stop_one(&mut self, name: &str) {
        if let Some(mut h) = self.handles.remove(name) {
            h.stop.store(true, Ordering::SeqCst);
            if let Some(t) = h.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// Stop and immediately start again (user-requested).
    pub fn restart(&mut self, name: &str, cfg: &Config) {
        self.stop_one(name);
        self.apply(cfg);
    }

    pub fn status(&self, name: &str) -> Option<MountStatus> {
        self.handles.get(name).map(|h| lock(&h.status).clone())
    }

    /// Statuses in the order of the config's mount list.
    pub fn statuses(&self, cfg: &Config) -> Vec<(MountConfig, MountStatus)> {
        cfg.mounts
            .iter()
            .map(|m| {
                let st = self
                    .status(&m.name)
                    .unwrap_or_else(|| MountStatus::new(State::Disabled, "Not started"));
                (m.clone(), st)
            })
            .collect()
    }

    /// Tear everything down (unmounting volumes). Blocks until done.
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.handles.keys().cloned().collect();
        for h in self.handles.values() {
            h.stop.store(true, Ordering::SeqCst);
        }
        for name in names {
            self.stop_one(&name);
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor thread
// ---------------------------------------------------------------------------

struct Ctx {
    name: String,
    status: SharedStatus,
    stop: Arc<AtomicBool>,
    notify: Notify,
    last_notify: Option<Instant>,
    ever_healthy: bool,
}

impl Ctx {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || TERMINATE.load(Ordering::SeqCst)
    }

    fn set(&mut self, state: State, detail: impl Into<String>) {
        let detail = detail.into();
        let prev = {
            let mut s = lock(&self.status);
            if s.state == state && s.detail == detail {
                return;
            }
            let prev = s.state;
            if prev != state {
                s.since = Instant::now();
                log(format!("[{}] {} -> {}: {}", self.name, prev.label(), state.label(), detail));
            }
            s.state = state;
            s.detail = detail.clone();
            if state.is_healthy() {
                s.last_ok = Some(Instant::now());
            }
            prev
        };
        (self.notify)();

        // Notifications on meaningful transitions only.
        if state.is_healthy() {
            if prev.is_problem() && self.ever_healthy {
                mac::notify(config::APP_NAME, &format!("{}: connection restored", self.name));
            }
            self.ever_healthy = true;
        } else if state.is_problem() && !prev.is_problem() {
            let throttled = self.last_notify.map_or(false, |t| t.elapsed() < NOTIFY_THROTTLE);
            if !throttled {
                self.last_notify = Some(Instant::now());
                let what = match state {
                    State::Disconnected => "connection to the bucket lost",
                    State::SignInRequired => "AWS sign-in required. Open BucketMount and click Sign in",
                    State::Down => "mount stopped, restarting",
                    _ => "needs attention",
                };
                mac::notify(config::APP_NAME, &format!("{}: {what}. {}", self.name, detail));
            }
        }
    }

    fn update<F: FnOnce(&mut MountStatus)>(&self, f: F) {
        f(&mut lock(&self.status));
    }

    /// Sleep in small steps so stop requests are honoured quickly.
    fn wait(&self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end && !self.stopping() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn run(cfg: MountConfig, mut ctx: Ctx, rclone: Option<PathBuf>, health_interval: Duration) {
    let mount_point = cfg.mount_path();
    let mut attempts: u32 = 0;

    while !ctx.stopping() {
        let Some(rclone) = rclone.clone() else {
            ctx.set(
                State::Error,
                "rclone not found. Reinstall BucketMount or set rclone_path in config.toml.",
            );
            ctx.wait(Duration::from_secs(30));
            continue;
        };
        if let Err(e) = prepare_mount_point(&mount_point) {
            ctx.set(State::Error, e);
            ctx.wait(Duration::from_secs(15));
            continue;
        }
        if ctx.stopping() {
            break;
        }

        let inv = rclone::invocation(&rclone, &cfg);
        let sso = sso::for_mount(&cfg, Some(&rclone));
        ctx.update(|s| s.sso_profile = sso.as_ref().map(|p| p.profile.clone()));
        let started = Instant::now();
        let reason = run_session(&cfg, &inv, sso.as_ref(), &mount_point, &mut ctx, health_interval);
        ctx.update(|s| {
            s.pid = None;
            s.mounted = false;
            s.uploads_pending = 0;
        });
        if mac::is_mounted(&mount_point) {
            if let Err(e) = mac::unmount(&mount_point) {
                log(format!("[{}] {e}", cfg.name));
            }
        }
        if ctx.stopping() {
            break;
        }

        if started.elapsed() > HEALTHY_RESET {
            attempts = 0;
        }
        attempts += 1;
        ctx.update(|s| s.restarts += 1);
        let delay = (1u64 << attempts.min(6)).clamp(2, 60);
        let signin = sso.as_ref().is_some_and(|p| sso::is_session_error(&reason) || p.needs_login());
        let generation = LOGIN_GENERATION.load(Ordering::SeqCst);
        for remaining in (1..=delay).rev() {
            if ctx.stopping() || LOGIN_GENERATION.load(Ordering::SeqCst) != generation {
                break;
            }
            if signin {
                ctx.set(State::SignInRequired, signin_detail(sso.as_ref()));
            } else {
                ctx.set(State::Down, format!("{reason}. Restarting in {remaining}s"));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    if mac::is_mounted(&mount_point) {
        if let Err(e) = mac::unmount(&mount_point) {
            log(format!("[{}] {e}", cfg.name));
        }
    }
    ctx.update(|s| {
        s.state = State::Disabled;
        s.detail = "Stopped".into();
        s.mounted = false;
        s.pid = None;
    });
    log(format!("[{}] supervisor stopped", cfg.name));
}

fn signin_detail(sso: Option<&SsoProfile>) -> String {
    match sso {
        Some(p) => format!("AWS SSO session for profile '{}' has expired", p.profile),
        None => "AWS SSO session has expired".into(),
    }
}

/// Create the mount point, clean up leftovers from a previous run and make
/// sure nothing else lives there.
fn prepare_mount_point(mount_point: &Path) -> Result<(), String> {
    if mac::is_mounted(mount_point) {
        mac::kill_stale_rclone(mount_point);
        mac::unmount(mount_point)?;
    }
    std::fs::create_dir_all(mount_point)
        .map_err(|e| format!("Cannot create mount point {}: {e}", mount_point.display()))?;
    let mut entries = std::fs::read_dir(mount_point)
        .map_err(|e| format!("Cannot read mount point {}: {e}", mount_point.display()))?;
    let non_trivial = entries.any(|e| {
        e.map(|e| {
            let n = e.file_name();
            n != ".DS_Store" && n != ".localized"
        })
        .unwrap_or(true)
    });
    if non_trivial {
        return Err(format!(
            "Mount point {} is not empty. Choose an empty folder.",
            config::collapse_tilde(mount_point)
        ));
    }
    Ok(())
}

fn open_log_file(name: &str) -> Option<std::fs::File> {
    let dir = config::logs_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.log", rclone::sanitize(name)));
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > 10 * 1024 * 1024 {
            let _ = std::fs::rename(&path, dir.join(format!("{}.log.1", rclone::sanitize(name))));
        }
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
}

pub fn log_path(name: &str) -> PathBuf {
    config::logs_dir().join(format!("{}.log", rclone::sanitize(name)))
}

fn terminate(child: &mut Child) {
    let pid = child.id() as i32;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Run one rclone process until it dies or has to be killed. Returns a short
/// human-readable reason.
fn run_session(
    cfg: &MountConfig,
    inv: &Invocation,
    sso: Option<&SsoProfile>,
    mount_point: &Path,
    ctx: &mut Ctx,
    health_interval: Duration,
) -> String {
    let rc_port = rclone::free_port();
    let args = rclone::mount_args(cfg, inv, mount_point, rc_port);
    log(format!("[{}] rclone {}", cfg.name, args.join(" ")));
    ctx.set(State::Starting, "Mounting…");

    let mut cmd = inv.command();
    cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Cannot start rclone: {e}");
            ctx.set(State::Error, msg.clone());
            return msg;
        }
    };
    ctx.update(|s| {
        s.pid = Some(child.id());
        s.log_tail.clear();
    });

    // Stream rclone's log to a file and keep the tail for the UI.
    if let Some(stderr) = child.stderr.take() {
        let status = ctx.status.clone();
        let mut file = open_log_file(&cfg.name);
        let name = cfg.name.clone();
        std::thread::Builder::new()
            .name(format!("log-{name}"))
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Some(f) = file.as_mut() {
                        let _ = writeln!(f, "{line}");
                    }
                    let mut s = lock(&status);
                    if line.contains(" ERROR ") || line.contains("Failed to") || line.contains("CRITICAL") {
                        s.last_error = Some(rclone::summarize_error(&line));
                    }
                    if s.log_tail.len() >= LOG_TAIL_LINES {
                        s.log_tail.pop_front();
                    }
                    s.log_tail.push_back(line);
                }
            })
            .ok();
    }

    let started = Instant::now();
    let mut mounted_seen = false;
    let mut unmounted_since: Option<Instant> = None;
    let mut last_rc: Option<Instant> = None;
    let mut rc_fail_since: Option<Instant> = None;
    let mut stats = VfsStats::default();
    let mut health_rx: Option<mpsc::Receiver<Result<usize, String>>> = None;
    let mut last_health: Option<Instant> = None;
    // None until the first bucket check has completed.
    let mut reachable: Option<bool> = None;
    let mut health_detail = String::new();
    let mut signin_needed = false;
    let mut login_generation = LOGIN_GENERATION.load(Ordering::SeqCst);

    loop {
        if ctx.stopping() {
            terminate(&mut child);
            return "Stopped".into();
        }
        match child.try_wait() {
            Ok(Some(st)) => {
                std::thread::sleep(Duration::from_millis(300)); // let the log reader catch up
                let err = lock(&ctx.status).last_error.clone();
                return match err {
                    Some(e) => format!("rclone exited ({st}): {e}"),
                    None => format!("rclone exited ({st})"),
                };
            }
            Err(e) => return format!("rclone wait failed: {e}"),
            Ok(None) => {}
        }

        let mounted = mac::is_mounted(mount_point);
        ctx.update(|s| s.mounted = mounted);
        if mounted {
            mounted_seen = true;
            unmounted_since = None;
        } else if mounted_seen {
            let since = *unmounted_since.get_or_insert_with(Instant::now);
            if since.elapsed() > UNMOUNTED_GRACE {
                terminate(&mut child);
                return "Volume was unmounted".into();
            }
        } else if started.elapsed() > MOUNT_APPEAR_TIMEOUT {
            terminate(&mut child);
            let err = lock(&ctx.status).last_error.clone();
            return match err {
                Some(e) => format!("Mount did not come up: {e}"),
                None => "Mount did not come up in time".into(),
            };
        }

        if mounted {
            // rc: upload queue + liveness of the rclone process itself.
            if last_rc.map_or(true, |t| t.elapsed() >= RC_POLL) {
                last_rc = Some(Instant::now());
                match rclone::rc_vfs_stats(&inv.rclone, rc_port) {
                    Ok(s) => {
                        stats = s;
                        rc_fail_since = None;
                    }
                    Err(_) => {
                        let since = *rc_fail_since.get_or_insert_with(Instant::now);
                        if since.elapsed() > RC_HUNG_AFTER {
                            terminate(&mut child);
                            return "rclone stopped responding".into();
                        }
                    }
                }
            }

            // A fresh sign-in: check right away rather than at the next interval.
            let generation = LOGIN_GENERATION.load(Ordering::SeqCst);
            if generation != login_generation {
                login_generation = generation;
                last_health = None;
            }

            // Active reachability check against the bucket, off-thread.
            let due_every = if reachable == Some(false) {
                health_interval.min(Duration::from_secs(10))
            } else {
                health_interval
            };
            if health_rx.is_none() && last_health.map_or(true, |t| t.elapsed() >= due_every) {
                let (tx, rx) = mpsc::channel();
                let inv2 = inv.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(rclone::check_connectivity(&inv2, Duration::from_secs(30)));
                });
                health_rx = Some(rx);
            }
            if let Some(rx) = &health_rx {
                match rx.try_recv() {
                    Ok(res) => {
                        health_rx = None;
                        last_health = Some(Instant::now());
                        match res {
                            Ok(_) => {
                                reachable = Some(true);
                                health_detail.clear();
                                signin_needed = false;
                            }
                            Err(e) => {
                                reachable = Some(false);
                                signin_needed = sso.is_some_and(|p| e == sso::SESSION_EXPIRED || p.needs_login());
                                health_detail = e;
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        health_rx = None;
                        last_health = Some(Instant::now());
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }

            let pending = stats.uploads_queued + stats.uploads_in_progress;
            ctx.update(|s| {
                s.uploads_pending = pending;
                s.errored_files = stats.errored_files;
            });
            if reachable.is_none() {
                ctx.set(State::Starting, "Mounted, checking bucket…");
            } else if reachable == Some(false) && signin_needed {
                let extra = if pending > 0 { format!(" · {pending} file(s) waiting to upload") } else { String::new() };
                ctx.set(State::SignInRequired, format!("{}{extra}", signin_detail(sso)));
            } else if reachable == Some(false) {
                let extra = if pending > 0 { format!(" · {pending} file(s) waiting to upload") } else { String::new() };
                ctx.set(State::Disconnected, format!("Bucket unreachable: {health_detail}{extra}"));
            } else if pending > 0 {
                ctx.set(State::Syncing, format!("Uploading {pending} file(s)"));
            } else if stats.errored_files > 0 {
                ctx.set(
                    State::Connected,
                    format!("Connected · {} file(s) failed to upload, see log", stats.errored_files),
                );
            } else {
                ctx.set(State::Connected, "Connected");
            }
        } else if !mounted_seen {
            ctx.set(State::Starting, "Mounting…");
        }

        std::thread::sleep(Duration::from_secs(1));
    }
}
