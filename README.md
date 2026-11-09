# Brightness Tray

An ultra-light Windows tray app for changing the brightness of external monitors over DDC/CI.

- **~1.6 MB** private memory, ~300 KB single `.exe`, no installer, no runtime
- Lives only in the system tray. It never appears in the taskbar or Alt+Tab
- Written in Rust on raw Win32 (`windows-sys`), no UI framework

## Usage

| Action | Result |
|---|---|
| Left-click tray icon | Opens a slider for each monitor |
| Drag / mouse wheel on a slider | Changes brightness (wheel = 5% steps) |
| Wheel over empty space | Changes all monitors together |
| Esc / click outside | Closes the popup |
| Right-click tray icon | Start with Windows · Refresh monitors · Exit |

Monitors are detected again after display changes and after the PC wakes from sleep. The popup and icon follow the Windows light/dark theme.

## Notes

- Your monitor must have **DDC/CI enabled** (usually in the monitor's OSD menu).
- Built-in laptop panels don't support DDC/CI and are hidden.
- Close other brightness-control tools so they don't send conflicting commands.
- "Start with Windows" saves the exe's current location, so move the exe to a permanent folder before you enable it.

## Build

```bash
cargo build --release
```

The binary is written to `target/release/brightness-tray.exe`.

## License

MIT
