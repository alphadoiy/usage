//! Read-only macOS presence checks. Missing GUI state never enables a push.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    Away,
    Present,
    Unknown,
}

impl Presence {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Away => "已锁屏或所有显示器熄屏",
            Self::Present => "已解锁且显示器亮屏",
            Self::Unknown => "无法确认锁屏或显示器状态，自动推送暂不发送",
        }
    }
}

fn classify(locked: Option<bool>, displays_asleep: Option<bool>) -> Presence {
    match (locked, displays_asleep) {
        (Some(true), _) | (_, Some(true)) => Presence::Away,
        (Some(false), Some(false)) => Presence::Present,
        _ => Presence::Unknown,
    }
}

#[cfg(target_os = "macos")]
pub(super) fn current() -> Presence {
    // SAFETY: the native calls below are read-only. CF objects returned by Copy
    // and Create are released, and dictionary values are type-checked before use.
    unsafe {
        let locked = mac::locked();
        let asleep = mac::displays_asleep();
        tracing::info!(
            event = "bark_screen_state",
            lock_state = match locked {
                Some(true) => "locked",
                Some(false) => "unlocked",
                None => "unknown",
            },
            display_state = match asleep {
                Some(true) => "asleep",
                Some(false) => "awake",
                None => "unknown",
            }
        );
        classify(locked, asleep)
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn current() -> Presence {
    tracing::info!(
        event = "bark_screen_state",
        lock_state = "unsupported",
        display_state = "unsupported"
    );
    classify(None, None)
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;

    type CFRef = *const c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFRef;
        fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayIsAsleep(display: u32) -> u32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(allocator: CFRef, text: *const i8, encoding: u32) -> CFRef;
        fn CFDictionaryGetValue(dictionary: CFRef, key: CFRef) -> CFRef;
        fn CFGetTypeID(value: CFRef) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: CFRef) -> u8;
        fn CFRelease(value: CFRef);
    }

    pub(super) unsafe fn locked() -> Option<bool> {
        // SAFETY: valid constant UTF-8 key; the session dictionary is owned here,
        // its value is borrowed until CFRelease, and null results are handled.
        unsafe {
            let dictionary = CGSessionCopyCurrentDictionary();
            if dictionary.is_null() {
                return None;
            }
            let key = CFStringCreateWithCString(
                std::ptr::null(),
                c"CGSSessionScreenIsLocked".as_ptr(),
                0x0800_0100,
            );
            if key.is_null() {
                CFRelease(dictionary);
                return None;
            }
            let value = CFDictionaryGetValue(dictionary, key);
            let locked = if value.is_null() {
                // This key is omitted by WindowServer for an unlocked session.
                Some(false)
            } else if CFGetTypeID(value) == CFBooleanGetTypeID() {
                Some(CFBooleanGetValue(value) != 0)
            } else {
                None
            };
            CFRelease(key);
            CFRelease(dictionary);
            locked
        }
    }

    pub(super) unsafe fn displays_asleep() -> Option<bool> {
        // SAFETY: count and display buffers are valid for the advertised sizes.
        // Re-query errors and an empty/headless display list are unknown states.
        unsafe {
            let mut count = 0;
            if CGGetOnlineDisplayList(0, std::ptr::null_mut(), &mut count) != 0
                || count == 0
                || count > 128
            {
                return None;
            }
            let mut displays = vec![0; count as usize];
            if CGGetOnlineDisplayList(count, displays.as_mut_ptr(), &mut count) != 0
                || count == 0
                || count as usize > displays.len()
            {
                return None;
            }
            Some(
                displays[..count as usize]
                    .iter()
                    .all(|&id| CGDisplayIsAsleep(id) != 0),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lock_or_all_displays_asleep_allows_automatic_notifications() {
        assert_eq!(classify(Some(true), Some(false)), Presence::Away);
        assert_eq!(classify(Some(false), Some(true)), Presence::Away);
        assert_eq!(classify(Some(true), None), Presence::Away);
        assert_eq!(classify(None, Some(true)), Presence::Away);
        assert_eq!(classify(Some(false), Some(false)), Presence::Present);
        for state in [(None, None), (None, Some(false)), (Some(false), None)] {
            assert_eq!(classify(state.0, state.1), Presence::Unknown);
        }
    }
}
