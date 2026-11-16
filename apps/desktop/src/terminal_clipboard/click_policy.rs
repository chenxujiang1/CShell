use super::*;

pub(crate) fn system_policy(scale: f64) -> (std::time::Duration, [f64; 2]) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{
            HiDpi::GetSystemMetricsForDpi,
            Input::KeyboardAndMouse::GetDoubleClickTime,
            WindowsAndMessaging::{SM_CXDOUBLECLK, SM_CYDOUBLECLK},
        };
        // SAFETY: read-only user32 queries with valid metric constants; no pointers.
        #[allow(unsafe_code)]
        let (ms, width, height) = unsafe {
            let dpi = (scale * 96.0).clamp(48.0, 768.0) as u32;
            (
                GetDoubleClickTime(),
                GetSystemMetricsForDpi(SM_CXDOUBLECLK, dpi),
                GetSystemMetricsForDpi(SM_CYDOUBLECLK, dpi),
            )
        };
        (
            std::time::Duration::from_millis(u64::from(ms.max(1))),
            [
                f64::from(width.max(2)) / 2.0,
                f64::from(height.max(2)) / 2.0,
            ],
        )
    }
    #[cfg(target_os = "macos")]
    {
        let seconds = objc2_app_kit::NSEvent::doubleClickInterval();
        let seconds = if seconds.is_finite() && seconds > 0.0 {
            seconds
        } else {
            0.5
        };
        (
            std::time::Duration::from_secs_f64(seconds),
            [4.0 * scale; 2],
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        (std::time::Duration::from_millis(500), [4.0 * scale; 2])
    }
}

pub(crate) fn register(
    state: &mut TerminalClickState,
    point: TerminalCellPoint,
    position: Option<winit::dpi::PhysicalPosition<f64>>,
    now: std::time::Instant,
    interval: std::time::Duration,
    tolerance: [f64; 2],
) -> u8 {
    let continues = state.last.is_some_and(|(time, previous)| {
        let close = match (state.last_position, position) {
            (Some(a), Some(b)) => {
                (a.x - b.x).abs() <= tolerance[0] && (a.y - b.y).abs() <= tolerance[1]
            }
            _ => previous == point,
        };
        close && now.saturating_duration_since(time) <= interval
    });
    state.count = if continues && state.count < 3 {
        state.count + 1
    } else {
        1
    };
    state.last = Some((now, point));
    state.last_position = position;
    state.count
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_slop_crosses_cells_but_never_turns_four_clicks_into_select_all() {
        let now = std::time::Instant::now();
        let mut state = TerminalClickState::default();
        let point = TerminalCellPoint { row: 0, column: 1 };
        for (n, expected) in [1, 2, 3, 1].into_iter().enumerate() {
            assert_eq!(
                register(
                    &mut state,
                    TerminalCellPoint {
                        column: n as u16,
                        ..point
                    },
                    Some(winit::dpi::PhysicalPosition::new(10.0 + n as f64, 20.0)),
                    now + std::time::Duration::from_millis(n as u64 * 100),
                    std::time::Duration::from_millis(500),
                    [4.0; 2]
                ),
                expected
            );
        }
        assert_eq!(
            register(
                &mut state,
                point,
                Some(winit::dpi::PhysicalPosition::new(100.0, 20.0)),
                now + std::time::Duration::from_millis(400),
                std::time::Duration::from_millis(500),
                [4.0; 2]
            ),
            1
        );
        assert_eq!(
            register(
                &mut state,
                point,
                Some(winit::dpi::PhysicalPosition::new(100.0, 20.0)),
                now + std::time::Duration::from_secs(2),
                std::time::Duration::from_millis(500),
                [4.0; 2]
            ),
            1
        );
    }
}
