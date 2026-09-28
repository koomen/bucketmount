//! macOS notifications posted as BucketMount itself (the UserNotifications
//! framework), and the permission to show them.
//!
//! The framework only works inside an app bundle. A bare binary (a dev
//! build from `cargo run`) falls back to `osascript`, which posts as Script
//! Editor and cannot tell whether anything is shown.

use crate::applog::log;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread};
use objc2_foundation::{NSBundle, NSError, NSString, NSUUID};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse, UNNotificationSettings,
    UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use serde::Serialize;
use std::process::{Command, Stdio};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Not checked yet.
    Unknown,
    /// Never asked: the next request shows the system prompt.
    NotAsked,
    /// Turned off by the user; only System Settings can turn it back on.
    Denied,
    Allowed,
    /// Not running from an app bundle; notifications go through osascript.
    Unsupported,
}

static PERMISSION: Mutex<Permission> = Mutex::new(Permission::Unknown);
/// A permission request is waiting for the user. macOS reports Denied
/// until they answer.
static ASKING: AtomicBool = AtomicBool::new(false);

struct Hooks {
    /// The permission changed.
    changed: Box<dyn Fn() + Send + Sync>,
    /// The user clicked one of our notifications.
    clicked: Box<dyn Fn() + Send + Sync>,
}
static HOOKS: OnceLock<Hooks> = OnceLock::new();

pub fn permission() -> Permission {
    *PERMISSION.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_permission(p: Permission) {
    let prev = std::mem::replace(&mut *PERMISSION.lock().unwrap_or_else(|e| e.into_inner()), p);
    if prev != p {
        log(format!("notification permission: {p:?}"));
        if let Some(h) = HOOKS.get() {
            (h.changed)();
        }
    }
}

fn bundled() -> bool {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
}

fn center() -> Option<Retained<UNUserNotificationCenter>> {
    bundled().then(UNUserNotificationCenter::currentNotificationCenter)
}

define_class!(
    // Shows our notifications even while BucketMount is the active app
    // (the window is open), and opens the window when one is clicked.
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "BucketMountNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            _response: &UNNotificationResponse,
            handler: &block2::DynBlock<dyn Fn()>,
        ) {
            if let Some(h) = HOOKS.get() {
                (h.clicked)();
            }
            handler.call(());
        }
    }
);

impl Delegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// Call once at startup. Checks the permission and, if the user has never
/// been asked, asks (macOS shows its prompt once).
pub fn init(changed: impl Fn() + Send + Sync + 'static, clicked: impl Fn() + Send + Sync + 'static) {
    let _ = HOOKS.set(Hooks { changed: Box::new(changed), clicked: Box::new(clicked) });
    let Some(c) = center() else {
        set_permission(Permission::Unsupported);
        return;
    };
    let delegate = Delegate::new();
    c.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The center holds its delegate weakly; ours lives as long as the app.
    std::mem::forget(delegate);
    check(true);
}

/// Re-read the permission (it can change in System Settings at any time).
pub fn refresh() {
    check(false);
}

fn check(ask_if_new: bool) {
    let Some(c) = center() else { return };
    let block = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        let p = match status {
            UNAuthorizationStatus::NotDetermined => Permission::NotAsked,
            UNAuthorizationStatus::Denied if ASKING.load(Ordering::SeqCst) => Permission::NotAsked,
            UNAuthorizationStatus::Denied => Permission::Denied,
            _ => Permission::Allowed,
        };
        set_permission(p);
        if ask_if_new && p == Permission::NotAsked {
            request();
        }
    });
    c.getNotificationSettingsWithCompletionHandler(&block);
}

/// Ask for permission. Shows the system prompt if the user was never asked;
/// otherwise macOS answers with the current setting without asking.
pub fn request() {
    let Some(c) = center() else { return };
    ASKING.store(true, Ordering::SeqCst);
    let block = RcBlock::new(|granted: Bool, err: *mut NSError| {
        ASKING.store(false, Ordering::SeqCst);
        if let Some(e) = unsafe { err.as_ref() } {
            log(format!("notification permission request failed: {}", e.localizedDescription()));
        }
        log(format!("notification permission request answered: granted={}", granted.as_bool()));
        refresh();
    });
    c.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &block,
    );
}

/// Post a notification. Silently dropped when the user has turned them off;
/// the window shows that.
pub fn post(title: &str, body: &str) {
    let Some(c) = center() else {
        return post_osascript(title, body);
    };
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    let id = NSUUID::UUID().UUIDString();
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
    let title = title.to_string();
    let done = RcBlock::new(move |err: *mut NSError| {
        if let Some(e) = unsafe { err.as_ref() } {
            log(format!("notification '{title}' not shown: {}", e.localizedDescription()));
        }
    });
    c.addNotificationRequest_withCompletionHandler(&request, Some(&done));
}

fn post_osascript(title: &str, body: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", esc(body), esc(title));
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// System Settings → Notifications, on BucketMount's page.
pub fn open_settings() {
    crate::mac::open_url("x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=com.bucketmount.desktop");
}
