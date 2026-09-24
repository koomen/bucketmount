//! macOS specific helpers: mount table, unmounting, Finder, notifications,
//! login item (LaunchAgent) and the single-instance lock.

use crate::applog::log;
use crate::config::{self, BUNDLE_ID};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Resolve symlinks in the *parent* of `p` only. Never stat `p` itself: if it
/// is a mount whose server has died, any access to it blocks indefinitely.
fn canonical(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .map(|c| c.join(name))
            .unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
}

fn run_timeout(prog: &str, args: &[&str], secs: u64) -> Result<(), String> {
    let mut cmd = Command::new(prog);
    cmd.args(args);
    match crate::rclone::run_with_timeout(cmd, Duration::from_secs(secs)) {
        Ok(f) if f.success => Ok(()),
        Ok(f) => Err(f.stderr.trim().to_string()),
        Err(e) => Err(e),
    }
}

/// All currently mounted paths, from `mount(8)`.
pub fn mounted_paths() -> Vec<PathBuf> {
    let out = match Command::new("/sbin/mount").output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // "<dev> on <path> (<type>, <flags>)"
            let rest = &line[line.find(" on ")? + 4..];
            let end = rest.rfind(" (")?;
            Some(PathBuf::from(&rest[..end]))
        })
        .collect()
}

pub fn is_mounted(path: &Path) -> bool {
    let want = canonical(path);
    mounted_paths().iter().any(|m| *m == want || *m == path)
}

/// Unmount, escalating from polite to forceful. Every step has a timeout
/// because a plain `umount` of a volume whose server is gone can hang.
pub fn unmount(path: &Path) -> Result<(), String> {
    let p = path.to_string_lossy().to_string();
    let attempts: [(&str, Vec<&str>, u64); 3] = [
        ("/sbin/umount", vec![&p], 5),
        ("/sbin/umount", vec!["-f", &p], 10),
        ("/usr/sbin/diskutil", vec!["unmount", "force", &p], 20),
    ];
    let mut last_err = String::new();
    for (prog, args, secs) in attempts.iter() {
        if !is_mounted(path) {
            return Ok(());
        }
        if let Err(e) = run_timeout(prog, args, *secs) {
            last_err = e;
        }
        std::thread::sleep(Duration::from_millis(700));
        if !is_mounted(path) {
            return Ok(());
        }
    }
    if is_mounted(path) {
        Err(format!("could not unmount {}: {last_err}", path.display()))
    } else {
        Ok(())
    }
}

/// Kill any `rclone nfsmount` left over from a previous run that serves this
/// mount point (e.g. after the app crashed).
pub fn kill_stale_rclone(mount_point: &Path) -> usize {
    let out = match Command::new("/bin/ps").args(["-axo", "pid=,command="]).output() {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let me = std::process::id();
    let mp = mount_point.to_string_lossy();
    let mp_canon = canonical(mount_point).to_string_lossy().to_string();
    let mut killed = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let (pid, cmd) = match line.split_once(char::is_whitespace) {
            Some(x) => x,
            None => continue,
        };
        let pid: i32 = match pid.trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid as u32 == me {
            continue;
        }
        let is_ours = cmd.contains("rclone") && cmd.contains("nfsmount") && (cmd.contains(&*mp) || cmd.contains(&mp_canon));
        if is_ours {
            log(format!("killing stale rclone pid {pid} for {}", mount_point.display()));
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            killed += 1;
        }
    }
    if killed > 0 {
        std::thread::sleep(Duration::from_secs(2));
    }
    killed
}

pub fn open_in_finder(path: &Path) {
    let _ = Command::new("/usr/bin/open").arg(path).spawn();
}

pub fn reveal_in_finder(path: &Path) {
    let _ = Command::new("/usr/bin/open").arg("-R").arg(path).spawn();
}

/// User-visible macOS notification.
pub fn notify(title: &str, body: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        esc(body),
        esc(title)
    );
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// Login item via LaunchAgent
// ---------------------------------------------------------------------------

fn launch_agent_path() -> PathBuf {
    config::home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{BUNDLE_ID}.plist"))
}

fn current_exe() -> Option<PathBuf> {
    std::env::current_exe().ok().map(|p| canonical(&p))
}

fn plist_contents(exe: &Path) -> String {
    let logs = config::logs_dir();
    let esc = |s: String| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{BUNDLE_ID}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--background</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
</dict>
</plist>
"#,
        exe = esc(exe.display().to_string()),
        out = esc(logs.join("launchd.out.log").display().to_string()),
        err = esc(logs.join("launchd.err.log").display().to_string()),
    )
}

/// True when the LaunchAgent exists and points at the running executable.
pub fn login_item_matches_exe() -> bool {
    let (Ok(text), Some(exe)) = (fs::read_to_string(launch_agent_path()), current_exe()) else {
        return false;
    };
    text.contains(&format!("<string>{}</string>", exe.display()))
}

fn launchctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("/bin/launchctl")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Write the LaunchAgent and load it. Loading starts a second copy of the
/// app, which exits immediately thanks to the single-instance lock, so this
/// is safe to call while running.
pub fn install_login_item() -> Result<(), String> {
    let exe = current_exe().ok_or("cannot determine executable path")?;
    let path = launch_agent_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let _ = fs::create_dir_all(config::logs_dir());
    fs::write(&path, plist_contents(&exe)).map_err(|e| format!("write {}: {e}", path.display()))?;
    let uid = unsafe { libc::getuid() };
    let _ = launchctl(&["bootout", &format!("gui/{uid}/{BUNDLE_ID}")]);
    // `bootstrap` fails harmlessly if launchd already knows the label.
    let _ = launchctl(&["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()]);
    log(format!("login item installed -> {}", exe.display()));
    Ok(())
}

pub fn remove_login_item() -> Result<(), String> {
    let uid = unsafe { libc::getuid() };
    let _ = launchctl(&["bootout", &format!("gui/{uid}/{BUNDLE_ID}")]);
    let path = launch_agent_path();
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    log("login item removed");
    Ok(())
}

/// Make sure the login item points at wherever the app lives now (it may have
/// been moved to /Applications since it was registered).
pub fn ensure_login_item() {
    if !login_item_matches_exe() {
        if let Err(e) = install_login_item() {
            log(format!("could not refresh login item: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Single instance
// ---------------------------------------------------------------------------

/// Returns a guard while this process holds the instance lock; `None` if
/// another BucketMount is already running. The lock lives on the local disk
/// (not on any mount) so flock works.
pub fn acquire_instance_lock() -> Option<fs::File> {
    use std::os::unix::io::AsRawFd;
    let dir = config::support_dir();
    fs::create_dir_all(&dir).ok()?;
    let f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("instance.lock"))
        .ok()?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Some(f)
    } else {
        None
    }
}
