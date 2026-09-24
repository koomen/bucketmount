//! Configuration: `~/.config/bucketmount/config.toml`.
//!
//! The whole app is driven by this one file. Copy it to a new machine, launch
//! the app, and every enabled mount comes up.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "BucketMount";
pub const BUNDLE_ID: &str = "com.bucketmount.desktop";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// `None` = never asked. Set on first launch when the user answers the prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_at_login: Option<bool>,
    /// Override the rclone binary. Normally the bundled copy is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rclone_path: Option<String>,
    /// How often to actively verify the bucket is reachable.
    pub health_check_interval_secs: u64,
    #[serde(rename = "mount")]
    pub mounts: Vec<MountConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            start_at_login: None,
            rclone_path: None,
            health_check_interval_secs: 30,
            mounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MountConfig {
    /// Display name; also the default volume / folder name. Must be unique.
    pub name: String,
    pub bucket: String,
    /// Optional directory inside the bucket to mount instead of the root.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// Where the volume appears. `~` is expanded.
    pub mount_point: String,
    pub enabled: bool,
    pub read_only: bool,

    /// rclone S3 provider name: AWS, Cloudflare, Minio, Wasabi, DigitalOcean, Other, ...
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub region: String,
    /// Custom endpoint for non-AWS providers.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub endpoint: String,

    // Credentials: exactly one of the three styles is used, see `cred_mode()`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub access_key_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub secret_access_key: String,
    /// Name of a remote in the user's own rclone.conf to reuse instead of inline keys.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub rclone_remote: String,
    /// Let the AWS SDK find credentials (env vars, ~/.aws/credentials, IAM).
    pub env_auth: bool,
    /// AWS profile from ~/.aws/config for the default chain (or to override
    /// the rclone remote's). SSO profiles are signed in to by the app.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub aws_profile: String,

    /// Seconds a written file sits in the local cache before it is uploaded.
    pub write_back_secs: u64,
    /// How long directory listings are cached (how quickly changes made
    /// elsewhere show up).
    pub dir_cache_secs: u64,
    /// Upper bound on the local VFS cache, e.g. "10G".
    pub cache_max_size: String,
    /// Any additional rclone flags.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            bucket: String::new(),
            prefix: String::new(),
            mount_point: String::new(),
            enabled: true,
            read_only: false,
            provider: "AWS".to_string(),
            region: String::new(),
            endpoint: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            rclone_remote: String::new(),
            env_auth: false,
            aws_profile: String::new(),
            write_back_secs: 5,
            dir_cache_secs: 60,
            cache_max_size: "10G".to_string(),
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredMode {
    /// Inline access key + secret stored in config.toml.
    Keys,
    /// Reuse a remote from ~/.config/rclone/rclone.conf.
    RcloneRemote,
    /// AWS SDK default chain.
    EnvAuth,
}

impl MountConfig {
    pub fn cred_mode(&self) -> CredMode {
        if !self.rclone_remote.trim().is_empty() {
            CredMode::RcloneRemote
        } else if !self.access_key_id.trim().is_empty() {
            CredMode::Keys
        } else {
            CredMode::EnvAuth
        }
    }

    /// Absolute mount point with `~` expanded.
    pub fn mount_path(&self) -> PathBuf {
        expand_tilde(&self.mount_point)
    }

    /// `bucket/prefix` part of the rclone remote spec.
    pub fn bucket_path(&self) -> String {
        let prefix = self.prefix.trim().trim_matches('/');
        if prefix.is_empty() {
            self.bucket.trim().to_string()
        } else {
            format!("{}/{}", self.bucket.trim(), prefix)
        }
    }

    pub fn validate(&self, others: &[MountConfig]) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Name is required.".into());
        }
        if name.contains('/') || name.contains(':') || name.starts_with('.') {
            return Err("Name may not contain '/' or ':' or start with '.'".into());
        }
        if others.iter().any(|o| o.name.trim() == name) {
            return Err(format!("A mount named '{name}' already exists."));
        }
        if self.bucket.trim().is_empty() {
            return Err("Bucket is required.".into());
        }
        if self.bucket.contains('/') {
            return Err("Bucket must be just the bucket name; put sub-folders in 'Path in bucket'.".into());
        }
        if self.mount_point.trim().is_empty() {
            return Err("Mount point is required.".into());
        }
        let mp = self.mount_path();
        if !mp.is_absolute() {
            return Err("Mount point must be an absolute path (or start with ~).".into());
        }
        if mp.parent().is_none() || mp == Path::new("/") {
            return Err("Mount point cannot be the root directory.".into());
        }
        if others.iter().any(|o| o.mount_path() == mp) {
            return Err("Another mount already uses that mount point.".into());
        }
        if self.cred_mode() == CredMode::Keys && self.secret_access_key.trim().is_empty() {
            return Err("Secret access key is required when an access key ID is given.".into());
        }
        if self.write_back_secs == 0 {
            return Err("Write-back delay must be at least 1 second.".into());
        }
        Ok(())
    }
}

pub fn default_mount_point(name: &str) -> String {
    let n = name.trim();
    if n.is_empty() {
        "~/BucketMount/".to_string()
    } else {
        format!("~/BucketMount/{n}")
    }
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand_tilde(p: &str) -> PathBuf {
    let p = p.trim();
    if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Display a path with the home directory collapsed to `~`.
pub fn collapse_tilde(p: &Path) -> String {
    let home = home_dir();
    match p.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

pub fn config_dir() -> PathBuf {
    // BUCKETMOUNT_CONFIG_DIR lets tests and power users relocate the config.
    if let Some(dir) = std::env::var_os("BUCKETMOUNT_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    home_dir().join(".config").join("bucketmount")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn logs_dir() -> PathBuf {
    home_dir().join("Library").join("Logs").join(APP_NAME)
}

pub fn cache_dir() -> PathBuf {
    home_dir().join("Library").join("Caches").join(APP_NAME)
}

pub fn support_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Application Support")
        .join(APP_NAME)
}

/// Load the config. `Ok(None)` when the file does not exist yet.
pub fn load() -> Result<Option<Config>, String> {
    let path = config_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(cfg))
}

pub fn save(cfg: &Config) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));

    let mut text = String::from(
        "# BucketMount configuration. Each [[mount]] is an S3 bucket mounted as a volume.\n\
         # Docs: see README.md in the BucketMount repository.\n\n",
    );
    text.push_str(&toml::to_string_pretty(cfg).map_err(|e| format!("serialize config: {e}"))?);

    let tmp = dir.join("config.toml.tmp");
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(text.as_bytes()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all().ok();
    }
    let path = config_path();
    fs::rename(&tmp, &path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(())
}
