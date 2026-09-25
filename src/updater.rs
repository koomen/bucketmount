//! Self-update from GitHub releases.
//!
//! Shortly after launch and then every few hours the app fetches
//! `latest.json` from the newest release. A newer, correctly signed build is
//! downloaded in the background, installed over the running app once no
//! mount is busy, and the app relaunches into it. No user action needed;
//! "Check for updates" in the window triggers a check right away.

use crate::app::AppState;
use crate::applog::log;
use crate::config;
use crate::mac;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(4 * 3600);
/// Install even if a mount still looks busy after this long.
const MAX_IDLE_WAIT: Duration = Duration::from_secs(15 * 60);

/// Set by the menu item; the updater thread picks it up within a second.
static CHECK_NOW: AtomicBool = AtomicBool::new(false);

/// Set in the environment of the relaunched app so it waits for the old
/// process to release the single-instance lock.
pub const RELAUNCH_ENV: &str = "BUCKETMOUNT_RELAUNCH";

pub fn check_now() {
    CHECK_NOW.store(true, Ordering::SeqCst);
}

pub fn start(app: &AppHandle) {
    let app = app.clone();
    std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            let mut next = Instant::now() + FIRST_CHECK_AFTER;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let manual = CHECK_NOW.swap(false, Ordering::SeqCst);
                if !manual && Instant::now() < next {
                    continue;
                }
                next = Instant::now() + CHECK_EVERY;
                if let Err(e) = check(&app, manual) {
                    log(format!("update check failed: {e}"));
                    if manual {
                        mac::notify(config::APP_NAME, &format!("Could not check for updates: {e}"));
                    }
                }
            }
        })
        .ok();
}

fn check(app: &AppHandle, manual: bool) -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");
    let found = tauri::async_runtime::block_on(async {
        let Some(update) = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())? else {
            return Ok::<_, String>(None);
        };
        log(format!("update {} available (running {current}), downloading", update.version));
        let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
        Ok(Some((update, bytes)))
    })?;
    let Some((update, bytes)) = found else {
        if manual {
            mac::notify(config::APP_NAME, &format!("BucketMount {current} is up to date."));
        }
        return Ok(());
    };

    wait_until_idle(app);
    update.install(bytes).map_err(|e| format!("install {}: {e}", update.version))?;
    log(format!("installed {}, relaunching", update.version));
    mac::notify(config::APP_NAME, &format!("Updated to {}", update.version));
    std::env::set_var(RELAUNCH_ENV, "1");
    // Runs the normal exit path (volumes unmounted, syncs stopped) first.
    app.restart();
}

/// Wait until no mount is uploading, mounting or in the middle of a sync, so
/// the relaunch interrupts nothing.
fn wait_until_idle(app: &AppHandle) {
    let start = Instant::now();
    while start.elapsed() < MAX_IDLE_WAIT {
        if !app.state::<AppState>().busy() {
            return;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    log("mounts still busy; installing the update anyway");
}
