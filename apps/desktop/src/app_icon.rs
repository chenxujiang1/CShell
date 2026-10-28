pub(crate) fn window_icon() -> Option<winit::window::Icon> {
    winit::window::Icon::from_rgba(
        include_bytes!("../assets/icons/cshell-64.rgba").to_vec(),
        64,
        64,
    )
    .ok()
}
