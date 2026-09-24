//! AWS IAM Identity Center (SSO) sign-in, so SSO profiles work without the
//! AWS CLI.
//!
//! rclone's AWS SDK already turns a cached SSO token into credentials and
//! refreshes it while it can. What it cannot do is the interactive sign-in
//! once the session has fully expired. We do that here with the OIDC device
//! authorization flow (the same one `aws sso login` uses) and write the token
//! to `~/.aws/sso/cache/<sha1>.json`, where the SDK looks for it.

use crate::config::{self, CredMode, MountConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Error text used when a bucket check fails because the SSO session is gone.
pub const SESSION_EXPIRED: &str = "AWS SSO session expired or invalid";

/// Everything needed to sign in for one SSO profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsoProfile {
    pub profile: String,
    /// `sso_session` name for the modern config format; `None` for legacy
    /// profiles that carry `sso_start_url` themselves (no refresh tokens).
    pub session: Option<String>,
    pub start_url: String,
    pub region: String,
    pub scopes: Vec<String>,
}

impl SsoProfile {
    /// Where the AWS SDKs look for this profile's token.
    pub fn cache_path(&self) -> PathBuf {
        let key = self.session.as_deref().unwrap_or(&self.start_url);
        config::home_dir()
            .join(".aws/sso/cache")
            .join(format!("{}.json", sha1_smol::Sha1::from(key).digest()))
    }

    /// True when rclone cannot get credentials without a new sign-in: no
    /// token, or an expired one that cannot be refreshed.
    pub fn needs_login(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(self.cache_path()) else { return true };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { return true };
        let now = OffsetDateTime::now_utc();
        let str_of = |k: &str| v[k].as_str().unwrap_or("");
        let later_than = |k: &str, t: OffsetDateTime| parse_time(str_of(k)).map_or(false, |x| x > t);
        if str_of("accessToken").is_empty() {
            return true;
        }
        if later_than("expiresAt", now + Duration::from_secs(300)) {
            return false;
        }
        let refreshable = !str_of("refreshToken").is_empty()
            && !str_of("clientId").is_empty()
            && later_than("registrationExpiresAt", now);
        !refreshable
    }
}

/// Does rclone's stderr say the SSO token is missing, expired or rejected?
pub fn is_session_error(stderr: &str) -> bool {
    ["SSO token", "SSO OIDC", "InvalidGrantException", "UnauthorizedException"]
        .iter()
        .any(|m| stderr.contains(m))
}

// ---------------------------------------------------------------------------
// Which profile a mount uses
// ---------------------------------------------------------------------------

/// The SSO profile behind a mount, if its credentials come from one.
pub fn for_mount(m: &MountConfig, rclone: Option<&Path>) -> Option<SsoProfile> {
    resolve(&profile_name(m, rclone)?)
}

/// The AWS profile rclone will use for this mount; `None` for inline keys.
fn profile_name(m: &MountConfig, rclone: Option<&Path>) -> Option<String> {
    let explicit = m.aws_profile.trim();
    if !explicit.is_empty() {
        return Some(explicit.to_string());
    }
    match m.cred_mode() {
        CredMode::Keys => return None,
        CredMode::RcloneRemote => {
            let remote = remote_config(rclone?, m.rclone_remote.trim())?;
            if remote.get("env_auth").map(String::as_str) != Some("true") {
                return None;
            }
            if let Some(p) = remote.get("profile").filter(|p| !p.is_empty()) {
                return Some(p.clone());
            }
        }
        CredMode::EnvAuth => {}
    }
    Some(std::env::var("AWS_PROFILE").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "default".into()))
}

/// One remote's options from the user's rclone.conf, via `rclone config dump`.
fn remote_config(rclone: &Path, name: &str) -> Option<HashMap<String, String>> {
    let out = Command::new(rclone).args(["config", "dump"]).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let obj = v.get(name)?.as_object()?;
    Some(obj.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
}

/// Look `profile` up in the AWS config file and return its SSO settings.
pub fn resolve(profile: &str) -> Option<SsoProfile> {
    let path = std::env::var("AWS_CONFIG_FILE")
        .map(|p| config::expand_tilde(&p))
        .unwrap_or_else(|_| config::home_dir().join(".aws/config"));
    let sections = parse_ini(&std::fs::read_to_string(path).ok()?);
    let section = if profile == "default" { "default".to_string() } else { format!("profile {profile}") };
    let p = sections.get(&section)?;

    if let Some(session) = p.get("sso_session") {
        let s = sections.get(&format!("sso-session {session}"))?;
        let scopes = s
            .get("sso_registration_scopes")
            .map(|v| v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_else(|| vec!["sso:account:access".to_string()]);
        return Some(SsoProfile {
            profile: profile.to_string(),
            session: Some(session.clone()),
            start_url: s.get("sso_start_url")?.clone(),
            region: s.get("sso_region")?.clone(),
            scopes,
        });
    }
    Some(SsoProfile {
        profile: profile.to_string(),
        session: None,
        start_url: p.get("sso_start_url")?.clone(),
        region: p.get("sso_region")?.clone(),
        scopes: Vec::new(),
    })
}

/// Minimal INI reader for ~/.aws/config. Indented lines (nested settings
/// such as `s3 =` blocks) are skipped.
fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        if raw.starts_with(char::is_whitespace) {
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            out.entry(name.clone()).or_default();
            current = Some(name);
        } else if let (Some(sec), Some((k, v))) = (&current, line.split_once('=')) {
            out.get_mut(sec).unwrap().insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Device authorization flow
// ---------------------------------------------------------------------------

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Run the sign-in. `on_code` is called once with the user code and the
/// verification URL (already carrying the code) as soon as they are known;
/// the caller opens the browser. Blocks until the user approves, the code
/// expires, or `cancel` is set.
pub fn login(sp: &SsoProfile, cancel: &AtomicBool, on_code: impl FnOnce(&str, &str)) -> Result<(), String> {
    let base = format!("https://oidc.{}.amazonaws.com", sp.region);

    let mut reg = json!({ "clientName": format!("{}-{}", config::APP_NAME, sp.profile), "clientType": "public" });
    if sp.session.is_some() {
        // Refresh tokens are only issued to clients registered for them.
        reg["scopes"] = json!(sp.scopes);
        reg["grantTypes"] = json!([DEVICE_GRANT, "refresh_token"]);
    }
    let client = post(&format!("{base}/client/register"), &reg).map_err(|e| format!("register client: {}", e.message))?;
    let client_id = field(&client, "clientId")?;
    let client_secret = field(&client, "clientSecret")?;
    let registration_expires = client["clientSecretExpiresAt"].as_i64().and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok());

    let auth = post(
        &format!("{base}/device_authorization"),
        &json!({ "clientId": client_id, "clientSecret": client_secret, "startUrl": sp.start_url }),
    )
    .map_err(|e| format!("start sign-in: {}", e.message))?;
    let device_code = field(&auth, "deviceCode")?;
    let user_code = field(&auth, "userCode")?;
    let url = auth["verificationUriComplete"]
        .as_str()
        .or(auth["verificationUri"].as_str())
        .ok_or("start sign-in: no verification URL in response")?
        .to_string();
    let mut interval = Duration::from_secs(auth["interval"].as_u64().unwrap_or(5).max(1));
    let deadline = Instant::now() + Duration::from_secs(auth["expiresIn"].as_u64().unwrap_or(600));
    on_code(&user_code, &url);

    let token_req = json!({
        "clientId": client_id, "clientSecret": client_secret,
        "grantType": DEVICE_GRANT, "deviceCode": device_code,
    });
    let token = loop {
        let wake = Instant::now() + interval;
        while Instant::now() < wake {
            if cancel.load(Ordering::SeqCst) {
                return Err("Sign-in cancelled".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if Instant::now() > deadline {
            return Err("The sign-in code expired before it was approved. Try again.".into());
        }
        match post(&format!("{base}/token"), &token_req) {
            Ok(t) => break t,
            Err(e) if e.code == "AuthorizationPendingException" || e.code == "authorization_pending" => {}
            Err(e) if e.code == "SlowDownException" || e.code == "slow_down" => interval += Duration::from_secs(5),
            Err(e) if e.code == "AccessDeniedException" || e.code == "access_denied" => {
                return Err("Sign-in was denied in the browser.".into())
            }
            Err(e) if e.code == "ExpiredTokenException" || e.code == "expired_token" => {
                return Err("The sign-in code expired before it was approved. Try again.".into())
            }
            Err(e) => return Err(format!("finish sign-in: {}", e.message)),
        }
    };

    let now = OffsetDateTime::now_utc();
    let mut cache = json!({
        "startUrl": sp.start_url,
        "region": sp.region,
        "accessToken": field(&token, "accessToken")?,
        "expiresAt": format_time(now + Duration::from_secs(token["expiresIn"].as_u64().unwrap_or(3600))),
        "clientId": client_id,
        "clientSecret": client_secret,
    });
    if let Some(t) = registration_expires {
        cache["registrationExpiresAt"] = json!(format_time(t));
    }
    if let Some(r) = token["refreshToken"].as_str() {
        cache["refreshToken"] = json!(r);
    }
    write_cache(&sp.cache_path(), &cache)
}

struct ApiError {
    code: String,
    message: String,
}

/// POST JSON to the OIDC API. Errors carry the service's error code
/// (e.g. `AuthorizationPendingException`) so the poll loop can branch on it.
fn post(url: &str, body: &Value) -> Result<Value, ApiError> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build();
    match agent.post(url).send_json(body) {
        Ok(resp) => resp.into_json::<Value>().map_err(|e| ApiError { code: String::new(), message: e.to_string() }),
        Err(ureq::Error::Status(status, resp)) => {
            let header = resp.header("x-amzn-ErrorType").unwrap_or("").split(':').next().unwrap_or("").to_string();
            let v: Value = resp.into_json().unwrap_or(Value::Null);
            let code = v["error"].as_str().map(str::to_string).filter(|c| !c.is_empty()).unwrap_or(header);
            let detail = v["error_description"].as_str().or(v["message"].as_str()).unwrap_or("");
            Err(ApiError { message: format!("HTTP {status} {code} {detail}").trim().to_string(), code })
        }
        Err(e) => Err(ApiError { code: String::new(), message: e.to_string() }),
    }
}

fn field(v: &Value, k: &str) -> Result<String, String> {
    v[k].as_str().map(str::to_string).ok_or_else(|| format!("unexpected response from AWS: missing {k}"))
}

fn write_cache(path: &Path, v: &Value) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.write_all(serde_json::to_string_pretty(v).unwrap().as_bytes())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("write {}: {e}", path.display()))
}

fn format_time(t: OffsetDateTime) -> String {
    t.replace_nanosecond(0).unwrap_or(t).format(&Rfc3339).unwrap_or_default()
}

/// RFC 3339, plus the `...UTC` suffix older AWS CLI versions wrote.
fn parse_time(s: &str) -> Option<OffsetDateTime> {
    let s = s.trim();
    let fixed = s.strip_suffix("UTC").map(|b| format!("{b}Z"));
    OffsetDateTime::parse(fixed.as_deref().unwrap_or(s), &Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aws_config() {
        let ini = "[default]\nregion = us-east-1\n\n[profile work]\nsso_session = corp\nsso_account_id = 1\ns3 =\n  max_concurrent_requests = 5\n\n[sso-session corp]\nsso_start_url = https://x.awsapps.com/start\nsso_region = us-west-2\nsso_registration_scopes = sso:account:access\n";
        let s = parse_ini(ini);
        assert_eq!(s["profile work"]["sso_session"], "corp");
        assert_eq!(s["sso-session corp"]["sso_region"], "us-west-2");
        assert!(!s["profile work"].contains_key("max_concurrent_requests"));
    }

    #[test]
    fn cache_key_matches_aws_cli() {
        // `echo -n my-sso | shasum` — the file name the AWS CLI uses.
        let sp = SsoProfile {
            profile: "work".into(),
            session: Some("my-sso".into()),
            start_url: String::new(),
            region: String::new(),
            scopes: vec![],
        };
        assert!(sp.cache_path().ends_with("0ad374308c5a4e22f723adf10145eafad7c4031c.json"));
    }

    #[test]
    fn times_round_trip() {
        assert!(parse_time("2026-09-24T06:27:48Z").is_some());
        assert!(parse_time("2026-09-24T06:27:48UTC").is_some());
        let t = parse_time("2026-09-24T06:27:48Z").unwrap();
        assert_eq!(format_time(t), "2026-09-24T06:27:48Z");
    }

    #[test]
    fn recognises_session_errors() {
        assert!(is_session_error("failed to refresh cached credentials, refresh cached SSO token failed"));
        assert!(!is_session_error("dial tcp: lookup s3.amazonaws.com: no such host"));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Talks to AWS: `SSO_TEST_PROFILE=my-profile cargo test live -- --ignored --nocapture`.
    /// Gets a device code, then cancels without approving; writes nothing.
    #[test]
    #[ignore]
    fn device_code_then_cancel() {
        let profile = std::env::var("SSO_TEST_PROFILE").unwrap_or_else(|_| "default".into());
        let sp = resolve(&profile).expect("not an SSO profile");
        let cancel = AtomicBool::new(false);
        let mut got = None;
        let res = login(&sp, &cancel, |code, url| {
            got = Some((code.to_string(), url.to_string()));
            cancel.store(true, Ordering::SeqCst);
        });
        let (code, url) = got.expect("no device code");
        println!("user code {code}, url {url}");
        assert_eq!(res, Err("Sign-in cancelled".to_string()));
    }

    /// Full sign-in: opens the browser and waits for approval, then writes the
    /// token cache under $HOME. Run with a scratch HOME to leave yours alone.
    #[test]
    #[ignore]
    fn full_login() {
        let profile = std::env::var("SSO_TEST_PROFILE").unwrap_or_else(|_| "default".into());
        let sp = resolve(&profile).expect("not an SSO profile");
        let cancel = AtomicBool::new(false);
        let res = login(&sp, &cancel, |code, url| {
            println!("APPROVE: code {code} at {url}");
            crate::mac::open_url(url);
        });
        assert_eq!(res, Ok(()));
        assert!(!sp.needs_login());
        println!("token written to {}", sp.cache_path().display());
    }
}
