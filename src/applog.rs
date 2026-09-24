//! Tiny append-only application log at `~/Library/Logs/BucketMount/app.log`.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: Mutex<()> = Mutex::new(());

pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}Z")
}

pub fn log(msg: impl AsRef<str>) {
    let line = format!("{} {}\n", timestamp(), msg.as_ref());
    eprint!("{line}");
    let _guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = crate::config::logs_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(dir.join("app.log")) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}
