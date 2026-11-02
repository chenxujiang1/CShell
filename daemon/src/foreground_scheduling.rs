//! Native latency requests exist only while a subscribed terminal has recent output.
//! Kept on the parser thread; dropping it releases the native request.
#![allow(unsafe_code)]

#[cfg(windows)]
#[derive(Debug, Default)]
pub struct ForegroundScheduling {
    requested: bool,
    held: bool,
}

#[cfg(windows)]
impl ForegroundScheduling {
    pub(crate) fn update(&mut self, active: bool) {
        if self.requested == active {
            return;
        }
        self.requested = active;
        if active {
            // SAFETY: 1 ms is a valid period. Every successful request is paired
            // below, including parser shutdown; no process handles are involved.
            self.held = unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) } == 0;
            if !self.held {
                tracing::debug!("native terminal timer request unavailable");
            }
        } else if self.held {
            // SAFETY: release exactly the successful request made above.
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
            self.held = false;
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub struct ForegroundScheduling {
    token: Option<
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
    >,
    original_qos: Option<(u32, i32)>,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_get_qos_class_np(
        thread: libc::pthread_t,
        class: *mut u32,
        relative: *mut i32,
    ) -> i32;
    fn pthread_set_qos_class_self_np(class: u32, relative: i32) -> i32;
}

#[cfg(target_os = "macos")]
impl ForegroundScheduling {
    pub(crate) fn update(&mut self, active: bool) {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        if active == self.token.is_some() {
            return;
        }
        if active {
            let (mut class, mut relative) = (0, 0);
            // SAFETY: the current pthread is live and both outputs are valid.
            let read = unsafe {
                pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative)
            };
            // Apple qos.h: USER_INTERACTIVE=0x21, DEFAULT=0x15. Unspecified
            // threads can be promoted, but cannot be restored to the sentinel 0.
            if read == 0 {
                // SAFETY: this changes only this parser/caller thread using a valid class.
                let set = unsafe { pthread_set_qos_class_self_np(0x21, 0) };
                if set == 0 {
                    self.original_qos = Some(if class == 0 {
                        (0x15, 0)
                    } else {
                        (class, relative)
                    });
                } else {
                    tracing::debug!(code = set, "native terminal QoS request unavailable");
                }
            }
        }
        // Parser threads have no Cocoa event loop to drain autoreleased objects.
        objc2::rc::autoreleasepool(|_| {
            let process = NSProcessInfo::processInfo();
            if active {
                self.token = Some(process.beginActivityWithOptions_reason(
                    NSActivityOptions::LatencyCritical
                        | NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
                    &NSString::from_str("Render active terminal output"),
                ));
            } else if let Some(token) = self.token.take() {
                // SAFETY: this token came from beginActivity on NSProcessInfo and is
                // ended exactly once on the same parser thread. System sleep remains allowed.
                unsafe { process.endActivity(&token) };
            }
        });
        if !active && let Some((class, relative)) = self.original_qos.take() {
            // SAFETY: restore the QoS captured on this same thread above.
            unsafe { pthread_set_qos_class_self_np(class, relative) };
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl Drop for ForegroundScheduling {
    fn drop(&mut self) {
        self.update(false);
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
#[derive(Debug, Default)]
pub struct ForegroundScheduling {
    _private: (),
}

#[cfg(not(any(windows, target_os = "macos")))]
impl ForegroundScheduling {
    pub(crate) fn update(&mut self, _active: bool) {}
}

impl ForegroundScheduling {
    /// Scope a foreground consumer on its current thread. Native state is
    /// restored on drop; keep the scope on the thread that created it.
    pub fn enter() -> Self {
        let mut scope = Self::default();
        scope.update(true);
        scope
    }
}
