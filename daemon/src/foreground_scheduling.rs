//! Native latency requests exist only while a subscribed terminal has recent output.
//! Kept on the parser thread; dropping it releases the native request.
#![allow(unsafe_code)]

#[cfg(windows)]
#[derive(Debug, Default)]
pub(crate) struct ForegroundScheduling {
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
pub(crate) struct ForegroundScheduling {
    token: Option<
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
    >,
}

#[cfg(target_os = "macos")]
impl ForegroundScheduling {
    pub(crate) fn update(&mut self, active: bool) {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        if active == self.token.is_some() {
            return;
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
pub(crate) struct ForegroundScheduling {
    _private: (),
}

#[cfg(not(any(windows, target_os = "macos")))]
impl ForegroundScheduling {
    pub(crate) fn update(&mut self, _active: bool) {}
}
