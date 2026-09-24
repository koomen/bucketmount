//! Menu bar status item: a coloured dot for the worst mount state plus a
//! menu listing every mount.

use crate::app::{show_window, AppState, Snapshot};
use crate::supervisor::State;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};

pub const TRAY_ID: &str = "main";

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(dot(State::Disabled.rgb()))
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
                    } else if let Some(name) = id.strip_prefix("edit:") {
                        show_window(app);
                        let _ = app.emit("edit-mount", name.to_string());
                    }
                }
            }
        })
        .build(app)?;
    refresh(app);
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
        menu.append(&MenuItem::with_id(app, format!("open:{}", m.config.name), "      Open in Finder", m.mounted, None::<&str>)?)?;
        menu.append(&MenuItem::with_id(app, format!("edit:{}", m.config.name), "      Settings…", true, None::<&str>)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    menu.append(&MenuItem::with_id(app, "open", "Open BucketMount", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit BucketMount", true, None::<&str>)?)?;
    tray.set_menu(Some(menu))?;

    let active: Vec<_> = snap.mounts.iter().filter(|m| m.state != State::Disabled).collect();
    let worst = active.iter().map(|m| m.state).max_by_key(|s| s.severity()).unwrap_or(State::Disabled);
    tray.set_icon(Some(dot(worst.rgb())))?;

    let problems = active.iter().filter(|m| m.state.is_problem()).count();
    let tip = if active.is_empty() {
        "BucketMount — no active mounts".to_string()
    } else if problems > 0 {
        format!("BucketMount — {problems} of {} mount(s) need attention", active.len())
    } else {
        match worst {
            State::Syncing => "BucketMount — syncing".to_string(),
            State::Starting => "BucketMount — mounting".to_string(),
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

/// A filled, anti-aliased dot rendered at 2x for retina menu bars.
fn dot((r, g, b): (u8, u8, u8)) -> Image<'static> {
    const SIZE: u32 = 36;
    let center = SIZE as f32 / 2.0;
    let radius = 12.0f32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let d = (dx * dx + dy * dy).sqrt();
            let a = (radius - d + 0.5).clamp(0.0, 1.0);
            rgba.extend_from_slice(&[r, g, b, (a * 255.0) as u8]);
        }
    }
    Image::new_owned(rgba, SIZE, SIZE)
}
