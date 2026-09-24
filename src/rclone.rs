//! Everything about invoking rclone: locating the binary, translating a
//! `MountConfig` into a remote definition + `nfsmount` arguments, health
//! checks and remote-control (rc) queries.

use crate::config::{self, CredMode, MountConfig};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Name of the on-the-fly remote we define through environment variables.
const ENV_REMOTE: &str = "bucket";

/// Find rclone: explicit config path, the copy bundled next to our own
/// executable (Contents/MacOS/rclone), then common install locations.
pub fn locate(configured: Option<&str>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(c) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        candidates.push(config::expand_tilde(c));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("rclone"));
            candidates.push(dir.join("../Resources/rclone"));
        }
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/rclone"));
    candidates.push(PathBuf::from("/usr/local/bin/rclone"));
    candidates.push(config::home_dir().join(".local/bin/rclone"));
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            candidates.push(Path::new(dir).join("rclone"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn version(rclone: &Path) -> Option<String> {
    let out = Command::new(rclone).arg("version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|l| l.trim().to_string())
}

/// The pieces needed to talk to one bucket.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub rclone: PathBuf,
    /// `remote:bucket/prefix`
    pub remote: String,
    pub env: Vec<(String, String)>,
}

impl Invocation {
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.rclone);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        // launchd starts us with a minimal PATH; mount_nfs lives in /sbin.
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{path}:/sbin:/usr/sbin:/usr/bin:/bin"));
        cmd
    }
}

pub fn invocation(rclone: &Path, m: &MountConfig) -> Invocation {
    let mut env = Vec::new();
    let remote_name = match m.cred_mode() {
        CredMode::RcloneRemote => m.rclone_remote.trim().to_string(),
        mode => {
            let key = |opt: &str| format!("RCLONE_CONFIG_{}_{}", ENV_REMOTE.to_uppercase(), opt);
            env.push((key("TYPE"), "s3".to_string()));
            if !m.provider.trim().is_empty() {
                env.push((key("PROVIDER"), m.provider.trim().to_string()));
            }
            if !m.region.trim().is_empty() {
                env.push((key("REGION"), m.region.trim().to_string()));
            }
            if !m.endpoint.trim().is_empty() {
                env.push((key("ENDPOINT"), m.endpoint.trim().to_string()));
            }
            match mode {
                CredMode::Keys => {
                    env.push((key("ACCESS_KEY_ID"), m.access_key_id.trim().to_string()));
                    env.push((key("SECRET_ACCESS_KEY"), m.secret_access_key.trim().to_string()));
                }
                _ => env.push((key("ENV_AUTH"), "true".to_string())),
            }
            ENV_REMOTE.to_string()
        }
    };
    Invocation {
        rclone: rclone.to_path_buf(),
        remote: format!("{remote_name}:{}", m.bucket_path()),
        env,
    }
}

/// Arguments for `rclone nfsmount`.
pub fn mount_args(m: &MountConfig, inv: &Invocation, mount_point: &Path, rc_port: u16) -> Vec<String> {
    let cache = config::cache_dir().join(sanitize(&m.name));
    let mut args: Vec<String> = vec![
        "nfsmount".into(),
        inv.remote.clone(),
        mount_point.to_string_lossy().to_string(),
        "--vfs-cache-mode".into(),
        "full".into(),
        "--vfs-write-back".into(),
        format!("{}s", m.write_back_secs),
        "--vfs-cache-max-size".into(),
        m.cache_max_size.trim().to_string(),
        "--dir-cache-time".into(),
        format!("{}s", m.dir_cache_secs.max(1)),
        "--cache-dir".into(),
        cache.join("vfs").to_string_lossy().to_string(),
        "--nfs-cache-type".into(),
        "disk".into(),
        "--nfs-cache-dir".into(),
        cache.join("nfs-handles").to_string_lossy().to_string(),
        "--volname".into(),
        m.name.trim().to_string(),
        // Local file locking so apps that flock() (editors, cargo, sqlite)
        // work on the volume.
        "-o".into(),
        "locallocks".into(),
        "--rc".into(),
        "--rc-addr".into(),
        format!("127.0.0.1:{rc_port}"),
        "--rc-no-auth".into(),
        "--log-level".into(),
        "INFO".into(),
        "--use-json-log=false".into(),
    ];
    if m.read_only {
        args.push("--read-only".into());
    }
    args.extend(m.extra_args.iter().cloned());
    args
}

pub fn sanitize(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

pub struct Finished {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run a command to completion, killing it when the deadline passes.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Finished, String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn rclone: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    match status {
        Some(st) => Ok(Finished { success: st.success(), stdout, stderr }),
        None => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

/// One cheap listing request against the bucket. Returns the number of
/// top-level entries on success.
pub fn check_connectivity(inv: &Invocation, timeout: Duration) -> Result<usize, String> {
    let mut cmd = inv.command();
    cmd.args([
        "lsjson",
        "--max-depth",
        "1",
        "--contimeout",
        "10s",
        "--timeout",
        "20s",
        "--retries",
        "1",
        "--low-level-retries",
        "2",
        &inv.remote,
    ]);
    let fin = run_with_timeout(cmd, timeout)?;
    if fin.success {
        let n = serde_json::from_str::<Vec<serde_json::Value>>(&fin.stdout)
            .map(|v| v.len())
            .unwrap_or(0);
        Ok(n)
    } else {
        Err(summarize_error(&fin.stderr))
    }
}

/// Pull the most useful single line out of rclone's stderr.
pub fn summarize_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let pick = lines
        .iter()
        .rev()
        .find(|l| l.contains("ERROR") || l.contains("Failed") || l.contains("error"))
        .or(lines.last())
        .copied()
        .unwrap_or("unknown error");
    // Strip the "2026/09/23 09:30:27 ERROR : " prefix.
    let stripped = pick
        .splitn(3, ' ')
        .nth(2)
        .map(|s| s.trim_start_matches(|c: char| c.is_alphabetic() || c == ' ' || c == ':'))
        .filter(|s| !s.is_empty())
        .unwrap_or(pick);
    let mut s = stripped.to_string();
    if s.len() > 200 {
        s.truncate(200);
        s.push('…');
    }
    s
}

#[derive(Debug, Clone, Default)]
pub struct VfsStats {
    pub uploads_queued: u64,
    pub uploads_in_progress: u64,
    pub errored_files: u64,
}

/// Query the running mount's remote-control API for cache/upload state.
pub fn rc_vfs_stats(rclone: &Path, port: u16) -> Result<VfsStats, String> {
    let mut cmd = Command::new(rclone);
    cmd.args(["rc", "--rc-addr", &format!("127.0.0.1:{port}"), "vfs/stats"]);
    let fin = run_with_timeout(cmd, Duration::from_secs(5))?;
    if !fin.success {
        return Err(summarize_error(&fin.stderr));
    }
    let v: serde_json::Value = serde_json::from_str(&fin.stdout).map_err(|e| e.to_string())?;
    let dc = &v["diskCache"];
    let num = |k: &str| dc[k].as_u64().unwrap_or(0);
    Ok(VfsStats {
        uploads_queued: num("uploadsQueued"),
        uploads_in_progress: num("uploadsInProgress"),
        errored_files: num("erroredFiles"),
    })
}

/// Pick an unused localhost port for the rc server.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(5572)
}
