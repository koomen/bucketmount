//! Synced folders: a plain local folder kept in two-way sync with the bucket.
//!
//! Files live on the local disk, so editors and tools see an ordinary
//! folder. The supervisor thread runs `rclone bisync` a few seconds after
//! local changes settle (reported by FSEvents) and on a timer to pick up
//! changes made elsewhere.

use crate::applog::log;
use crate::config::{self, MountConfig};
use crate::mac;
use crate::rclone::{self, Invocation};
use crate::sso::{self, SsoProfile};
use crate::supervisor::{self, lock, Ctx, State, LOGIN_GENERATION};
use notify::{EventKind, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Longest wait between retries after a failed run.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// bisync's state (listings, lock file) for one synced folder.
fn workdir(m: &MountConfig) -> PathBuf {
    config::support_dir().join("bisync").join(rclone::sanitize(&m.name))
}

/// bisync has completed a run for this folder/bucket pair before. The
/// listing file names encode both paths, so pointing a synced folder at a
/// different bucket or folder starts over with a resync.
fn has_state(dir: &Path, folder: &Path, remote: &str) -> bool {
    // Same encoding as bisync's session name: "/Users/me/x" -> "Users_me_x".
    let enc = |p: &str| p.replace(['/', ':'], "_").trim_start_matches('_').to_string();
    let prefix = format!("{}..{}", enc(&folder.to_string_lossy()), enc(remote));
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok).any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with(&prefix) && n.ends_with(".path1.lst")
            })
        })
        .unwrap_or(false)
}

/// Throw away bisync's state so the next run is a full resync.
pub fn forget_state(m: &MountConfig) {
    let dir = workdir(m);
    log(format!("[{}] clearing sync state in {}", m.name, dir.display()));
    let _ = std::fs::remove_dir_all(dir);
}

/// Local changes that should trigger a sync. Excluded files (Finder
/// metadata, swap files) and pure reads do not count.
fn relevant(event: &notify::Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.iter().any(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        !(name == ".DS_Store"
            || name.starts_with("._")
            || (name.starts_with('.') && (name.ends_with(".swp") || name.ends_with(".swx"))))
    })
}

fn prepare_folder(folder: &Path) -> Result<(), String> {
    // Switching from mount mode: the old volume may still be there.
    if mac::is_mounted(folder) {
        mac::kill_stale_rclone(folder);
        mac::unmount(folder)?;
    }
    std::fs::create_dir_all(folder).map_err(|e| format!("Cannot create folder {}: {e}", folder.display()))?;
    if !folder.is_dir() {
        return Err(format!("{} is not a folder", config::collapse_tilde(folder)));
    }
    Ok(())
}

enum Failure {
    SignIn,
    /// bisync lost track of what is in sync; needs a user-approved resync.
    NeedsResync(String),
    Other(String),
}

/// One bisync run. `Ok(())` when both sides are in sync.
fn run_bisync(
    cfg: &MountConfig,
    inv: &Invocation,
    folder: &Path,
    resync: bool,
    ctx: &mut Ctx,
) -> Result<(), Failure> {
    let dir = workdir(cfg);
    std::fs::create_dir_all(&dir).map_err(|e| Failure::Other(format!("Cannot create {}: {e}", dir.display())))?;
    let args = rclone::bisync_args(cfg, inv, folder, &dir, resync);
    log(format!("[{}] rclone {}", cfg.name, args.join(" ")));

    let mut cmd = inv.command();
    cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Failure::Other(format!("Cannot start rclone: {e}")))?;
    ctx.update(|s| s.pid = Some(child.id()));

    let output = Arc::new(Mutex::new(Vec::new()));
    let reader = child
        .stderr
        .take()
        .and_then(|e| supervisor::stream_log(e, &cfg.name, ctx.status.clone(), Some(output.clone())));

    let status = loop {
        if ctx.stopping() {
            // bisync handles SIGTERM gracefully; --recover picks up next time.
            supervisor::terminate(&mut child);
            break None;
        }
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(Failure::Other(format!("rclone wait failed: {e}"))),
        }
    };
    if let Some(r) = reader {
        let _ = r.join();
    }
    ctx.update(|s| s.pid = None);

    let Some(status) = status else { return Err(Failure::Other("Stopped".into())) };
    if status.success() {
        return Ok(());
    }
    let text = output.lock().unwrap_or_else(|e| e.into_inner()).join("\n");
    if sso::is_session_error(&text) {
        return Err(Failure::SignIn);
    }
    let summary = rclone::summarize_error(&text);
    // A failed resync says the same, but the next attempt is a resync anyway.
    if !resync && (text.contains("Must run --resync") || text.contains("must run --resync")) {
        Err(Failure::NeedsResync(summary))
    } else if text.contains("too many deletes") {
        Err(Failure::Other(format!(
            "Stopped: more than half the files would be deleted. Check the folder, then pass --force under Extra rclone flags for one run. ({summary})"
        )))
    } else {
        Err(Failure::Other(summary))
    }
}

pub fn run(cfg: MountConfig, mut ctx: Ctx, rclone: Option<PathBuf>) {
    let folder = cfg.mount_path();
    let quiet = Duration::from_secs(cfg.write_back_secs.max(1));
    let poll = Duration::from_secs(cfg.dir_cache_secs.max(10));

    // Set up: rclone present and the folder exists.
    let rclone = loop {
        if ctx.stopping() {
            return stopped(&cfg, &mut ctx);
        }
        let Some(r) = rclone.clone() else {
            ctx.set(State::Error, "rclone not found. Reinstall BucketMount or set rclone_path in config.toml.");
            ctx.wait(Duration::from_secs(30));
            continue;
        };
        match prepare_folder(&folder) {
            Ok(()) => break r,
            Err(e) => {
                ctx.set(State::Error, e);
                ctx.wait(Duration::from_secs(15));
            }
        }
    };
    ctx.update(|s| s.mounted = true);

    let inv = rclone::invocation(&rclone, &cfg);
    let sso: Option<SsoProfile> = sso::for_mount(&cfg, Some(&rclone));
    ctx.update(|s| s.sso_profile = sso.as_ref().map(|p| p.profile.clone()));

    // Time of the most recent local change not yet synced.
    let dirty: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let d = dirty.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.as_ref().is_ok_and(relevant) {
            *d.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    })
    .and_then(|mut w| w.watch(&folder, RecursiveMode::Recursive).map(|_| w));
    let _watcher = match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            log(format!("[{}] cannot watch {}: {e}; syncing on the timer only", cfg.name, folder.display()));
            None
        }
    };

    let mut next_remote = Instant::now();
    let mut retry_at = Instant::now();
    let mut failures: u32 = 0;
    let mut needs_resync = false;
    let mut login_generation = LOGIN_GENERATION.load(Ordering::SeqCst);

    while !ctx.stopping() {
        let generation = LOGIN_GENERATION.load(Ordering::SeqCst);
        if generation != login_generation {
            login_generation = generation;
            retry_at = Instant::now();
            next_remote = Instant::now();
        }

        let changed = *dirty.lock().unwrap_or_else(|e| e.into_inner());
        let local_due = changed.is_some_and(|t| t.elapsed() >= quiet);
        let now = Instant::now();
        if needs_resync || now < retry_at || !(local_due || now >= next_remote) {
            if changed.is_some() && !needs_resync && lock(&ctx.status).state.is_healthy() {
                ctx.set(State::Syncing, "Local changes waiting to upload");
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }

        // Events that arrive while bisync runs mark the folder dirty again.
        *dirty.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let resync = !has_state(&workdir(&cfg), &folder, &inv.remote);
        if resync {
            log(format!("[{}] no sync state yet: running a full resync", cfg.name));
            ctx.set(State::Starting, "First sync with the bucket…");
        } else if local_due {
            ctx.set(State::Syncing, "Syncing local changes…");
        }

        match run_bisync(&cfg, &inv, &folder, resync, &mut ctx) {
            Ok(()) => {
                failures = 0;
                retry_at = Instant::now();
                next_remote = Instant::now() + poll;
                ctx.update(|s| {
                    s.last_error = None;
                    s.last_ok = Some(Instant::now());
                });
                ctx.set(State::Connected, "Up to date");
            }
            Err(f) => {
                if ctx.stopping() {
                    break;
                }
                // Whatever the run did not get to is still unsynced.
                if local_due {
                    dirty.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Instant::now);
                }
                failures += 1;
                let backoff = Duration::from_secs(1u64 << failures.min(6)).clamp(Duration::from_secs(2), MAX_BACKOFF);
                retry_at = Instant::now() + backoff;
                next_remote = retry_at;
                match f {
                    Failure::SignIn => {
                        ctx.set(State::SignInRequired, supervisor::signin_detail(sso.as_ref()));
                    }
                    Failure::NeedsResync(e) => {
                        needs_resync = true;
                        ctx.update(|s| s.needs_resync = true);
                        ctx.set(
                            State::Error,
                            format!("Sync state was lost ({e}). Click Resync to merge the folder and the bucket again."),
                        );
                    }
                    Failure::Other(e) => {
                        let signin = sso.as_ref().is_some_and(|p| p.needs_login());
                        if signin {
                            ctx.set(State::SignInRequired, supervisor::signin_detail(sso.as_ref()));
                        } else {
                            ctx.set(State::Disconnected, format!("Sync failed: {e}. Retrying in {}s", backoff.as_secs()));
                        }
                    }
                }
            }
        }
    }
    stopped(&cfg, &mut ctx);
}

fn stopped(cfg: &MountConfig, ctx: &mut Ctx) {
    ctx.update(|s| {
        s.state = State::Disabled;
        s.detail = "Stopped".into();
        s.mounted = false;
        s.pid = None;
    });
    log(format!("[{}] sync stopped", cfg.name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_bisync_state() {
        let dir = std::env::temp_dir().join(format!("bucketmount-sync-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let folder = Path::new("/Users/me/git/pete-bucket");
        assert!(!has_state(&dir, folder, "bucket:petes-bucket"));
        std::fs::write(dir.join("Users_me_git_pete-bucket..bucket_petes-bucket.path1.lst-new"), "").unwrap();
        assert!(!has_state(&dir, folder, "bucket:petes-bucket"));
        std::fs::write(dir.join("Users_me_git_pete-bucket..bucket_petes-bucket.path1.lst"), "").unwrap();
        assert!(has_state(&dir, folder, "bucket:petes-bucket"));
        assert!(!has_state(&dir, folder, "bucket:other"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ignores_metadata_and_swap_files() {
        let ev = |p: &str| notify::Event::new(EventKind::Any).add_path(PathBuf::from(p));
        assert!(relevant(&ev("/x/slides.md")));
        assert!(!relevant(&ev("/x/.DS_Store")));
        assert!(!relevant(&ev("/x/._slides.md")));
        assert!(!relevant(&ev("/x/.slides.md.swp")));
    }
}
