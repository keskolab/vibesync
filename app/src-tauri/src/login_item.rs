//! Start at login.
//!
//! macOS: the app registers ITSELF through SMAppService (macOS 13+), so
//! System Settings ▸ Login Items lists "VibeSync" with its icon. The
//! autostart plugin's LaunchAgent is a bare background item, which macOS
//! labels with the signing certificate's name instead of the app's, both
//! there and in its "Background Items Added" notification. The LaunchAgent
//! stays as the fallback where SMAppService can't register (older macOS, an
//! unbundled dev build) and is the only mechanism on other platforms.

use vibesync_engine as engine;

/// Why the app registration didn't happen.
#[derive(Debug)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum RegisterError {
    /// No SMAppService here: not macOS, macOS < 13, or not a .app bundle.
    Unavailable,
    /// The user switched VibeSync off in System Settings; only they can
    /// switch it back on — falling back to the LaunchAgent would override
    /// their choice.
    NeedsApproval,
    Failed(String),
}

/// The two ways to start at login, behind one seam so the switching logic
/// is testable without touching the machine's real login items.
trait Launchers {
    fn app_on(&self) -> bool;
    fn register_app(&self) -> Result<(), RegisterError>;
    fn unregister_app(&self) -> Result<(), String>;
    fn open_login_items_settings(&self);
    fn agent_on(&self) -> bool;
    fn enable_agent(&self) -> Result<(), String>;
    fn disable_agent(&self) -> Result<(), String>;
}

fn is_on(l: &impl Launchers) -> bool {
    l.app_on() || l.agent_on()
}

fn enable(l: &impl Launchers) -> Result<(), String> {
    match l.register_app() {
        Ok(()) => {
            // Drop the old entry, or Login Items keeps listing it unnamed.
            let _ = l.disable_agent();
            Ok(())
        }
        Err(RegisterError::NeedsApproval) => {
            l.open_login_items_settings();
            Err("Allow VibeSync in System Settings ▸ General ▸ Login Items".into())
        }
        Err(RegisterError::Failed(why)) => {
            engine::dlog::warn(|| format!("login item: app registration failed ({why}), using LaunchAgent"));
            l.enable_agent()
        }
        Err(RegisterError::Unavailable) => l.enable_agent(),
    }
}

fn disable(l: &impl Launchers) -> Result<(), String> {
    let app = if l.app_on() { l.unregister_app() } else { Ok(()) };
    let agent = l.disable_agent();
    app.and(agent)
}

/// Every launch: move a LaunchAgent left by 0.3.1 and earlier over to the
/// app registration. Where that can't register, rewrite the agent instead:
/// it records an absolute executable path, which goes stale when the binary
/// is renamed, moved, or rebuilt elsewhere — launchd then resurrects an
/// ancient build as a frozen tray icon (live case: a two-day-old
/// `vibesync-app` started at login long after the rename to `VibeSync`).
fn heal(l: &impl Launchers) {
    if !l.agent_on() {
        return;
    }
    if l.app_on() || l.register_app().is_ok() {
        let _ = l.disable_agent();
    } else {
        let _ = l.enable_agent();
    }
}

struct System<'a>(&'a tauri::AppHandle);

impl Launchers for System<'_> {
    fn app_on(&self) -> bool {
        #[cfg(target_os = "macos")]
        return app_service::is_on();
        #[cfg(not(target_os = "macos"))]
        false
    }

    fn register_app(&self) -> Result<(), RegisterError> {
        #[cfg(target_os = "macos")]
        return app_service::register();
        #[cfg(not(target_os = "macos"))]
        Err(RegisterError::Unavailable)
    }

    fn unregister_app(&self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        return app_service::unregister();
        #[cfg(not(target_os = "macos"))]
        Ok(())
    }

    fn open_login_items_settings(&self) {
        #[cfg(target_os = "macos")]
        app_service::open_settings();
    }

    fn agent_on(&self) -> bool {
        use tauri_plugin_autostart::ManagerExt;
        self.0.autolaunch().is_enabled().unwrap_or(false)
    }

    fn enable_agent(&self) -> Result<(), String> {
        use tauri_plugin_autostart::ManagerExt;
        self.0.autolaunch().enable().map_err(|e| e.to_string())
    }

    fn disable_agent(&self) -> Result<(), String> {
        use tauri_plugin_autostart::ManagerExt;
        self.0.autolaunch().disable().map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "macos")]
mod app_service {
    use super::RegisterError;
    use objc2::{rc::Retained, runtime::AnyClass};
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    /// This app's registration — `None` before macOS 13 (the class doesn't
    /// exist; touching it would crash) or outside a .app bundle.
    fn service() -> Option<Retained<SMAppService>> {
        let bundled = std::env::current_exe()
            .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
            .unwrap_or(false);
        if !bundled || AnyClass::get(c"SMAppService").is_none() {
            return None;
        }
        Some(unsafe { SMAppService::mainAppService() })
    }

    pub fn is_on() -> bool {
        service().is_some_and(|s| unsafe { s.status() } == SMAppServiceStatus::Enabled)
    }

    pub fn register() -> Result<(), RegisterError> {
        let s = service().ok_or(RegisterError::Unavailable)?;
        unsafe { s.registerAndReturnError() }.map_err(|e| {
            if unsafe { s.status() } == SMAppServiceStatus::RequiresApproval {
                RegisterError::NeedsApproval
            } else {
                RegisterError::Failed(format!("{e:?}"))
            }
        })
    }

    pub fn unregister() -> Result<(), String> {
        match service() {
            Some(s) => unsafe { s.unregisterAndReturnError() }.map_err(|e| format!("{e:?}")),
            None => Ok(()),
        }
    }

    pub fn open_settings() {
        if service().is_some() {
            unsafe { SMAppService::openSystemSettingsLoginItems() };
        }
    }
}

pub fn is_enabled(app: &tauri::AppHandle) -> bool {
    is_on(&System(app))
}

pub fn set_enabled(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let l = System(app);
    if enabled { enable(&l) } else { disable(&l) }
}

pub fn heal_on_launch(app: &tauri::AppHandle) {
    heal(&System(app));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Login items as plain flags; `register` decides how the app
    /// registration answers.
    struct Fake {
        app: Cell<bool>,
        agent: Cell<bool>,
        register: fn() -> Result<(), RegisterError>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(app: bool, agent: bool, register: fn() -> Result<(), RegisterError>) -> Self {
            Fake { app: Cell::new(app), agent: Cell::new(agent), register, calls: RefCell::new(vec![]) }
        }
        fn called(&self, name: &str) -> bool {
            self.calls.borrow().contains(&name)
        }
    }

    impl Launchers for Fake {
        fn app_on(&self) -> bool {
            self.app.get()
        }
        fn register_app(&self) -> Result<(), RegisterError> {
            self.calls.borrow_mut().push("register_app");
            let r = (self.register)();
            if r.is_ok() {
                self.app.set(true);
            }
            r
        }
        fn unregister_app(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("unregister_app");
            self.app.set(false);
            Ok(())
        }
        fn open_login_items_settings(&self) {
            self.calls.borrow_mut().push("open_settings");
        }
        fn agent_on(&self) -> bool {
            self.agent.get()
        }
        fn enable_agent(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("enable_agent");
            self.agent.set(true);
            Ok(())
        }
        fn disable_agent(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("disable_agent");
            self.agent.set(false);
            Ok(())
        }
    }

    fn ok() -> Result<(), RegisterError> {
        Ok(())
    }
    fn unavailable() -> Result<(), RegisterError> {
        Err(RegisterError::Unavailable)
    }
    fn failed() -> Result<(), RegisterError> {
        Err(RegisterError::Failed("launchd said no".into()))
    }
    fn needs_approval() -> Result<(), RegisterError> {
        Err(RegisterError::NeedsApproval)
    }

    /// Switching on registers the app itself — listed as "VibeSync" — and
    /// leaves no LaunchAgent behind.
    #[test]
    fn enabling_registers_the_app_not_a_launch_agent() {
        let l = Fake::new(false, false, ok);
        enable(&l).unwrap();
        assert!(l.app.get());
        assert!(!l.agent.get(), "no bare background item");
        assert!(is_on(&l));
    }

    #[test]
    fn enabling_clears_a_leftover_launch_agent() {
        let l = Fake::new(false, true, ok);
        enable(&l).unwrap();
        assert!(l.app.get() && !l.agent.get());
    }

    /// Dev builds and macOS < 13 still start at login, the old way.
    #[test]
    fn enabling_falls_back_to_the_agent_where_the_app_cant_register() {
        for register in [unavailable, failed] {
            let l = Fake::new(false, false, register);
            enable(&l).unwrap();
            assert!(!l.app.get() && l.agent.get());
            assert!(is_on(&l));
        }
    }

    /// The user switched VibeSync off in System Settings: send them there
    /// instead of sneaking back in through the LaunchAgent.
    #[test]
    fn enabling_after_the_user_blocked_it_opens_settings_without_fallback() {
        let l = Fake::new(false, false, needs_approval);
        assert!(enable(&l).is_err(), "the switch flips back");
        assert!(l.called("open_settings"));
        assert!(!l.agent.get());
        assert!(!is_on(&l));
    }

    #[test]
    fn disabling_removes_both_kinds() {
        let l = Fake::new(true, true, ok);
        disable(&l).unwrap();
        assert!(!l.app.get() && !l.agent.get());
        assert!(!is_on(&l));
    }

    #[test]
    fn disabling_when_off_is_harmless() {
        let l = Fake::new(false, false, ok);
        disable(&l).unwrap();
        assert!(!l.called("unregister_app"), "nothing registered to remove");
        assert!(!is_on(&l));
    }

    /// Upgrading from 0.3.1: the agent becomes an app registration on the
    /// first launch — the user keeps starting at login, now as "VibeSync".
    #[test]
    fn launch_moves_an_old_agent_to_the_app_registration() {
        let l = Fake::new(false, true, ok);
        heal(&l);
        assert!(l.app.get() && !l.agent.get());
    }

    #[test]
    fn launch_drops_a_duplicate_agent_when_the_app_is_already_registered() {
        let l = Fake::new(true, true, failed);
        heal(&l);
        assert!(!l.called("register_app"));
        assert!(l.app.get() && !l.agent.get());
    }

    /// Where the app can't register, keep the old self-heal: rewrite the
    /// agent so it points at the current executable.
    #[test]
    fn launch_rewrites_the_agent_where_the_app_cant_register() {
        for register in [unavailable, failed, needs_approval] {
            let l = Fake::new(false, true, register);
            heal(&l);
            assert!(l.called("enable_agent"));
            assert!(l.agent.get() && !l.app.get());
        }
    }

    /// Start at login is off: launching must never switch it on.
    #[test]
    fn launch_leaves_login_off_when_it_is_off() {
        let l = Fake::new(false, false, ok);
        heal(&l);
        assert!(l.calls.borrow().is_empty());
        assert!(!is_on(&l));
    }

    #[test]
    fn launch_leaves_an_app_registration_alone() {
        let l = Fake::new(true, false, ok);
        heal(&l);
        assert!(l.calls.borrow().is_empty());
        assert!(is_on(&l));
    }
}
