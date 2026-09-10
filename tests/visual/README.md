# Terminal visual corpus

Run the interactive corpus with:

```powershell
cargo xtask visual-corpus
cargo xtask log-corpus
cargo xtask log-window-e2e
cargo xtask session-log
```

The launcher renders `terminal-unicode-v1.ansi.txt` through the production
`AlacrittyTerminalEngine → FrameSnapshot → cosmic-text/Swash → wgpu` path. It
does not connect to the daemon or create a PTY.

`log-corpus` exercises the separate `LogPage → LogSurfaceModel → wgpu` path,
including styled byte ranges, grapheme shaping, stable row IDs, and visible-row
clipping. It also runs without a daemon or PTY.

`log-window-e2e` continuously scrolls across bounded pages, jumps to distant
stable `LineId` anchors, alternates real window sizes, and requires successful
background reflow and wgpu presentation. It exits non-zero on failure or timeout.

`session-log` exercises live `PTY → journal → incremental 64-bit line index →
bounded IPC LogPage → LogSurfaceModel → wgpu` delivery. Journal reads and IPC
decoding stay on the desktop background connection thread, outside redraw.

For each Windows, Linux, and macOS target, record the OS version, scale factor,
GPU/backend, and available CJK/Emoji fonts. Verify:

1. ASCII columns and box drawing stay aligned.
2. Simplified/Traditional Chinese, Japanese, and Korean do not overlap adjacent cells.
3. NFC and NFD accents have equivalent placement; stacked combining marks are not clipped.
4. Wide glyphs and full-width ASCII occupy two cells without duplicate drawing.
5. Emoji sequences do not leave stale atlas pixels; record whether the platform fallback is color or monochrome.
6. ANSI 16-color, 256-color, True Color, bold, italic, underline, and inverse rows remain distinct.
7. Resize and DPI changes do not cause jumps, missing glyphs, or device errors.

Screenshot baselines are platform-specific because the renderer deliberately
uses system fallback fonts. Do not compare Windows, Linux, and macOS pixels as
if they used the same font files.
