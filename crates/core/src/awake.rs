//! Keeps the Mac from going to sleep while work is under way (a download, an agent's turn), so
//! leaving it alone doesn't stop the work. The display may still sleep. Held as [`Awake`]
//! guards: the first one taken asks macOS to stay awake, the last one dropped lets it sleep.

use std::sync::Mutex;

/// How many guards are held, and macOS's id for the request while any are.
static HELD: Mutex<(usize, Option<u32>)> = Mutex::new((0, None));

/// Keeps the Mac awake until dropped.
#[must_use = "the Mac may sleep as soon as this is dropped"]
pub struct Awake(());

impl Awake {
    pub fn hold() -> Self {
        let mut held = HELD.lock().unwrap();
        held.0 += 1;
        if held.0 == 1 {
            held.1 = platform::prevent_sleep();
        }
        Awake(())
    }

    /// Whether anything keeps the Mac awake right now.
    pub fn held() -> bool {
        HELD.lock().unwrap().0 > 0
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap();
        held.0 -= 1;
        if held.0 == 0
            && let Some(id) = held.1.take()
        {
            platform::allow_sleep(id);
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;

    use objc2_foundation::NSString;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: u32 = 255;

    /// An idle-sleep assertion, which `pmset -g assertions` lists under the app's name.
    pub fn prevent_sleep() -> Option<u32> {
        let kind = NSString::from_str("PreventUserIdleSystemSleep");
        let name = NSString::from_str("A model is downloading or an agent is working");
        let mut id = 0;
        // SAFETY: NSString is toll-free bridged to CFString, and both outlive the call.
        let status = unsafe { IOPMAssertionCreateWithName((&*kind as *const NSString).cast(), LEVEL_ON, (&*name as *const NSString).cast(), &mut id) };
        if status != 0 {
            tracing::warn!("couldn't keep the Mac awake (IOKit error {status:#x})");
            return None;
        }
        Some(id)
    }

    pub fn allow_sleep(id: u32) {
        // SAFETY: `id` came from IOPMAssertionCreateWithName and is released once.
        unsafe { IOPMAssertionRelease(id) };
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn prevent_sleep() -> Option<u32> {
        None
    }

    pub fn allow_sleep(_: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_guard_lets_the_mac_sleep() {
        let first = Awake::hold();
        let second = Awake::hold();
        #[cfg(target_os = "macos")]
        {
            assert!(HELD.lock().unwrap().1.is_some());
            let listed = std::process::Command::new("/usr/bin/pmset").args(["-g", "assertions"]).output().unwrap();
            assert!(String::from_utf8_lossy(&listed.stdout).contains("A model is downloading or an agent is working"));
        }
        drop(first);
        assert!(Awake::held());
        drop(second);
        assert!(!Awake::held());
        assert!(HELD.lock().unwrap().1.is_none());
    }
}
