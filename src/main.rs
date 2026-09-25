//! BucketMount — mount S3 buckets as macOS volumes and keep them alive.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod applog;
mod config;
mod mac;
mod rclone;
mod sso;
mod supervisor;
mod sync;
mod tray;
mod updater;

use app::AppState;
use config::Config;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tauri::{Emitter, Manager, RunEvent, WindowEvent};

extern "C" fn on_signal(_sig: libc::c_int) {
    supervisor::TERMINATE.store(true, Ordering::SeqCst);
}

fn main() {
    let background = std::env::args().any(|a| a == "--background");

    // After a self-update the old process may still be shutting down.
    let relaunched = std::env::var_os(updater::RELAUNCH_ENV).is_some();
    std::env::remove_var(updater::RELAUNCH_ENV);
    let mut instance_lock = mac::acquire_instance_lock();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while instance_lock.is_none() && relaunched && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        instance_lock = mac::acquire_instance_lock();
    }
    let Some(_instance_lock) = instance_lock else {
        eprintln!("{} is already running; use the menu bar icon.", config::APP_NAME);
        return;
    };
    applog::log(format!(
        "{} {} starting{}",
        config::APP_NAME,
        env!("CARGO_PKG_VERSION"),
        if background { " (background)" } else { "" }
    ));
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, on_signal as *const () as usize);
    }

    let (cfg, config_error) = match config::load() {
        Ok(Some(c)) => (c, None),
        Ok(None) => (Config::default(), None),
        Err(e) => {
            applog::log(format!("config error: {e}"));
            (Config::default(), Some(e))
        }
    };
    if cfg.start_at_login == Some(true) {
        mac::ensure_login_item();
    }

    // Supervisors signal changes through this channel; a helper thread turns
    // them into UI events and tray refreshes (debounced).
    let (notify_tx, notify_rx) = mpsc::channel::<()>();
    let notify_tx = Mutex::new(notify_tx);
    let notify: supervisor::Notify = Arc::new(move || {
        let _ = notify_tx.lock().unwrap_or_else(|e| e.into_inner()).send(());
    });
    let rclone = rclone::locate(cfg.rclone_path.as_deref());
    applog::log(format!(
        "using rclone: {}",
        rclone.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND".into())
    ));
    let state = AppState {
        cfg: Mutex::new(cfg),
        config_error: Mutex::new(config_error),
        manager: Mutex::new(supervisor::Manager::new(notify, rclone)),
        background,
        login: Mutex::new(None),
    };

    let tauri_app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            app::snapshot,
            app::default_mount_point,
            app::save_mount,
            app::delete_mount,
            app::set_start_at_login,
            app::test_connection,
            app::sso_login,
            app::cancel_sso_login,
            app::restart_mount,
            app::log_tail,
            app::open_mount,
            app::show_log,
            app::reveal_config,
            app::open_logs,
            app::quit,
            app::debug_log,
            app::check_for_updates,
        ])
        .on_window_event(|window, event| {
            // Closing the window hides it; the app lives on in the menu bar.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                app::hide_window(window.app_handle());
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            // Menu bar only: no Dock icon, not in the app switcher. The
            // bundle's Info.plist (LSUIElement) does the same from launch.
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            tray::create(&handle)?;
            updater::start(&handle);

            // Start the supervisors now that the tray exists to show them.
            {
                let st = handle.state::<AppState>();
                let cfg = st.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone();
                st.manager.lock().unwrap_or_else(|e| e.into_inner()).apply(&cfg);
            }

            // Notification pump: coalesce bursts, then refresh tray + UI.
            let h = handle.clone();
            std::thread::spawn(move || {
                while notify_rx.recv().is_ok() {
                    std::thread::sleep(Duration::from_millis(150));
                    while notify_rx.try_recv().is_ok() {}
                    tray::refresh(&h);
                    let _ = h.emit("state-changed", ());
                }
            });

            // SIGTERM/SIGINT: ask Tauri to exit (which unmounts), with a hard
            // fallback if that takes too long.
            let h = handle.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    if supervisor::TERMINATE.load(Ordering::SeqCst) {
                        h.exit(0);
                        std::thread::sleep(Duration::from_secs(25));
                        applog::log("forced exit after signal");
                        std::process::exit(0);
                    }
                }
            });

            if !background {
                app::show_window(&handle);
            }
            // Development aid: open the editor for a mount straight away.
            if let Ok(name) = std::env::var("BUCKETMOUNT_OPEN_EDITOR") {
                let h = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    let _ = h.emit("edit-mount", name);
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start BucketMount");

    tauri_app.run(|handle, event| {
        if let RunEvent::Exit = event {
            applog::log("shutting down: unmounting volumes");
            handle.state::<AppState>().manager.lock().unwrap_or_else(|e| e.into_inner()).stop_all();
            applog::log("bye");
        }
    });
}
