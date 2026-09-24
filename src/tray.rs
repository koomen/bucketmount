//! Menu bar status item: a bucket with a coloured badge for the worst mount
//! state, plus a menu listing every mount.

use crate::app::{show_window, AppState, Snapshot};
use crate::supervisor::State;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const TRAY_ID: &str = "main";

/// System appearance the icon was last drawn for. The bucket is drawn in the
/// menu bar's text colour, which a template image would give us for free, but
/// a template image cannot keep the badge coloured.
static DARK: AtomicBool = AtomicBool::new(false);

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(status_icon(DARK.load(Ordering::Relaxed), State::Disabled.rgb()))
        .tooltip("BucketMount")
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| {
            let id = event.id().as_ref();
            match id {
                "open" => show_window(app),
                "quit" => app.exit(0),
                _ => {
                    if let Some(name) = id.strip_prefix("open:") {
                        if let Some(m) = crate::app::lock_cfg(app).mounts.iter().find(|m| m.name == name) {
                            crate::mac::open_in_finder(&m.mount_path());
                        }
                    } else if let Some(name) = id.strip_prefix("login:") {
                        show_window(app);
                        if let Some(m) = crate::app::lock_cfg(app).mounts.iter().find(|m| m.name == name) {
                            if let Err(e) = crate::app::start_sso_login(app, m) {
                                crate::mac::notify("BucketMount", &e);
                            }
                        }
                    } else if let Some(name) = id.strip_prefix("edit:") {
                        show_window(app);
                        let _ = app.emit("edit-mount", name.to_string());
                    }
                }
            }
        })
        .build(app)?;
    DARK.store(crate::mac::dark_mode(), Ordering::Relaxed);
    refresh(app);

    // Redraw when the user switches between Light and Dark.
    let h = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(3));
        let dark = crate::mac::dark_mode();
        if DARK.swap(dark, Ordering::Relaxed) != dark {
            refresh(&h);
        }
    });
    Ok(())
}

/// Rebuild menu, icon and tooltip from the current state. Safe to call from
/// any thread; the work is dispatched to the main thread.
pub fn refresh(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let snap = handle.state::<AppState>().snapshot();
        if let Err(e) = apply(&handle, &snap) {
            crate::applog::log(format!("tray update failed: {e}"));
        }
    });
}

fn apply(app: &AppHandle, snap: &Snapshot) -> tauri::Result<()> {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return Ok(()) };

    let menu = Menu::new(app)?;
    if snap.mounts.is_empty() {
        menu.append(&MenuItem::with_id(app, "none", "No mounts configured", false, None::<&str>)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    for m in &snap.mounts {
        let header = format!("{}  —  {}", m.config.name, m.state_label);
        menu.append(&MenuItem::with_id(app, format!("hdr:{}", m.config.name), header, false, None::<&str>)?)?;
        if !m.detail.is_empty() && m.detail != m.state_label {
            let d = truncate(&m.detail, 70);
            menu.append(&MenuItem::with_id(app, format!("det:{}", m.config.name), format!("      {d}"), false, None::<&str>)?)?;
        }
        if m.state == State::SignInRequired {
            menu.append(&MenuItem::with_id(app, format!("login:{}", m.config.name), "      Sign in to AWS…", true, None::<&str>)?)?;
        }
        menu.append(&MenuItem::with_id(app, format!("open:{}", m.config.name), "      Open in Finder", m.mounted, None::<&str>)?)?;
        menu.append(&MenuItem::with_id(app, format!("edit:{}", m.config.name), "      Settings…", true, None::<&str>)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    menu.append(&MenuItem::with_id(app, "open", "Open BucketMount", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit BucketMount", true, None::<&str>)?)?;
    tray.set_menu(Some(menu))?;

    let active: Vec<_> = snap.mounts.iter().filter(|m| m.state != State::Disabled).collect();
    let worst = active.iter().map(|m| m.state).max_by_key(|s| s.severity()).unwrap_or(State::Disabled);
    tray.set_icon(Some(status_icon(DARK.load(Ordering::Relaxed), worst.rgb())))?;

    let problems = active.iter().filter(|m| m.state.is_problem()).count();
    let tip = if active.is_empty() {
        "BucketMount — no active mounts".to_string()
    } else if problems > 0 {
        format!("BucketMount — {problems} of {} mount(s) need attention", active.len())
    } else {
        match worst {
            State::Syncing => "BucketMount — syncing".to_string(),
            State::Starting => "BucketMount — starting".to_string(),
            _ => format!("BucketMount — {} mount(s) connected", active.len()),
        }
    };
    tray.set_tooltip(Some(tip))?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// The menu bar icon: a simplified version of the app icon's bucket (handle,
/// open rim, one hoop) with a status dot at the bottom right, cut out of the
/// bucket by a thin gap. As in the app icon, the handle's left end is in front
/// of the bucket and its right end goes behind it. Rendered at 2x for retina menu bars (20x18 pt).
fn status_icon(dark: bool, (r, g, b): (u8, u8, u8)) -> Image<'static> {
    const W: u32 = 40;
    const H: u32 = 36;
    const SS: u32 = 4;
    let (fg, fg_alpha) = if dark { (255.0, 1.0) } else { (0.0, 0.85) };

    let ellipse = |x: f32, y: f32, cx: f32, cy: f32, rx: f32, ry: f32| {
        ((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2) <= 1.0
    };
    let (cx, top, bot) = (18.0f32, 13.5f32, 30.0f32);
    let halfw = |y: f32| 12.2 + (8.8 - 12.2) * (y - top) / (bot - top);

    // Handle: a squared-off arc standing on the bucket, its plane turned
    // towards the viewer, projected like the rim (same shape as the app icon). Points are (x, y, depth), depth > 0
    // in front.
    const HANDLE_W: f32 = 1.1; // half the wire's thickness
    // Narrower than the rim so the front leg still crosses the opening with
    // only a small turn, and squarer than the app icon so it reads as upright.
    let (hr, turn) = (10.0f32, 20f32.to_radians());
    let handle: Vec<(f32, f32, f32)> = (0..=48)
        .map(|i| {
            let t = std::f32::consts::PI * i as f32 / 48.0;
            let z = hr * t.cos() * turn.sin();
            (cx - hr * t.cos() * turn.cos(), 15.5 - 12.5 * t.sin().powf(0.5) + z * 5.0 / 13.0, z)
        })
        .collect();
    // distance to the handle's centre line and the depth there
    let handle_hit = |x: f32, y: f32| {
        let mut best = (f32::MAX, 0.0f32);
        for w in handle.windows(2) {
            let ((x0, y0, z0), (x1, y1, z1)) = (w[0], w[1]);
            let (ddx, ddy) = (x1 - x0, y1 - y0);
            let k = (((x - x0) * ddx + (y - y0) * ddy) / (ddx * ddx + ddy * ddy)).clamp(0.0, 1.0);
            let d = ((x - x0 - k * ddx).powi(2) + (y - y0 - k * ddy).powi(2)).sqrt();
            if d < best.0 {
                best = (d, z0 + (z1 - z0) * k);
            }
        }
        best
    };

    let bucket = |x: f32, y: f32| {
        // rim with the opening cut out
        let outer = ellipse(x, y, cx, top, 13.0, 5.0);
        let opening = ellipse(x, y, cx, top - 0.6, 10.8, 3.4);
        let rim = outer && !opening;
        // tapered body with a rounded bottom and a gap for the hoop
        let body = y >= top
            && ((y <= bot && (x - cx).abs() <= halfw(y)) || ellipse(x, y, cx, bot, 8.8, 2.2));
        let hoop = {
            let yc = 20.5;
            let u = ((x - cx) / halfw(yc)).clamp(-1.0, 1.0);
            let ry = 5.0 + (2.2 - 5.0) * (yc - top) / (bot - top);
            (y - (yc + ry * (1.0 - u * u).sqrt())).abs() < 0.7
        };
        // The front half crosses the opening; the back half is hidden by
        // the rim and body.
        let (hd, hz) = handle_hit(x, y);
        let wire = hd < HANDLE_W && (hz >= 0.0 || !(outer || body));
        wire || rim || (body && !hoop && !opening)
    };
    let (dx, dy, dr, gap) = (32.5f32, 28.5f32, 6.5f32, 2.0f32);

    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for py in 0..H {
        for px in 0..W {
            let (mut n_dot, mut n_fg) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    let d = ((x - dx).powi(2) + (y - dy).powi(2)).sqrt();
                    if d <= dr {
                        n_dot += 1;
                    } else if d > dr + gap && bucket(x, y) {
                        n_fg += 1;
                    }
                }
            }
            let total = (SS * SS) as f32;
            let (a_dot, a_fg) = (n_dot as f32 / total, n_fg as f32 / total * fg_alpha);
            let a = a_dot + a_fg;
            if a <= 0.0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let mix = |c: u8| ((c as f32 * a_dot + fg * a_fg) / a).round() as u8;
            rgba.extend_from_slice(&[mix(r), mix(g), mix(b), (a * 255.0).round() as u8]);
        }
    }
    Image::new_owned(rgba, W, H)
}




