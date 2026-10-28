use crate::sftp_settings::WindowPlacement;
#[derive(Clone, Copy)]
pub(super) struct Screen {
    pub origin: [i32; 2],
    pub size: [u32; 2],
    pub scale: f64,
}
/// Keep the whole normal window, including title bar, reachable after monitor/DPI changes.
pub(super) fn restore(saved: Option<WindowPlacement>, screens: &[Screen]) -> WindowPlacement {
    let mut value = saved
        .unwrap_or(WindowPlacement {
            position: None,
            inner_size: [1560.0, 900.0],
            maximized: false,
        })
        .sanitized();
    let Some(screen) = value
        .position
        .and_then(|p| {
            screens.iter().find(|screen| {
                i64::from(p[0]) >= i64::from(screen.origin[0])
                    && i64::from(p[1]) >= i64::from(screen.origin[1])
                    && i64::from(p[0]) < i64::from(screen.origin[0]) + i64::from(screen.size[0])
                    && i64::from(p[1]) < i64::from(screen.origin[1]) + i64::from(screen.size[1])
            })
        })
        .or(screens.first())
    else {
        return value;
    };
    let scale = if screen.scale.is_finite() && screen.scale > 0.0 {
        screen.scale
    } else {
        1.0
    };
    let limits = [
        (f64::from(screen.size[0]) / scale - 32.0).max(320.0),
        (f64::from(screen.size[1]) / scale - 80.0).max(240.0),
    ];
    for index in 0..2 {
        value.inner_size[index] = value.inner_size[index].min(limits[index]);
    }
    if let Some(position) = &mut value.position {
        for index in 0..2 {
            let extent =
                value.inner_size[index] * scale + if index == 0 { 16.0 } else { 64.0 } * scale;
            let low = f64::from(screen.origin[index]);
            let high = (low + f64::from(screen.size[index]) - extent).max(low);
            position[index] = (f64::from(position[index])).clamp(low, high).round() as i32;
        }
    }
    value
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_keeps_negative_monitor_positions_and_clamps_disconnected_screens() {
        let screens = [
            Screen {
                origin: [0, 0],
                size: [1920, 1080],
                scale: 1.0,
            },
            Screen {
                origin: [-2560, 0],
                size: [2560, 1440],
                scale: 1.5,
            },
        ];
        let normal = WindowPlacement {
            position: Some([-2200, 120]),
            inner_size: [1100.0, 700.0],
            maximized: true,
        };
        let restored = restore(Some(normal.clone()), &screens);
        assert_eq!(restored.position, normal.position);
        assert_eq!(restored.inner_size, normal.inner_size);
        assert!(restored.maximized);
        let removed = restore(Some(normal), &screens[..1]);
        assert_eq!(removed.position.unwrap()[0], 0);
        assert!(removed.inner_size[0] <= 1888.0);
        assert!(removed.inner_size[1] <= 1000.0);
    }
    #[test]
    fn corrupt_extreme_geometry_and_new_dpi_stay_inside_a_small_screen() {
        let screens = [Screen {
            origin: [80, 100],
            size: [1280, 720],
            scale: 1.5,
        }];
        let restored = restore(
            Some(WindowPlacement {
                position: Some([i32::MAX, i32::MIN]),
                inner_size: [f64::INFINITY, 1e12],
                maximized: false,
            }),
            &screens,
        );
        let position = restored.position.unwrap();
        assert!(position[0] >= 80 && position[1] >= 100);
        assert!(restored.inner_size[0] * 1.5 + f64::from(position[0]) <= 1360.0);
        assert!(restored.inner_size[1] * 1.5 + f64::from(position[1]) <= 820.0);
        assert!(restore(None, &[]).position.is_none());
    }
}
