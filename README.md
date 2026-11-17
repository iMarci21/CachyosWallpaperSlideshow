# CachyOS Wallpaper Slideshow

A native Rust wallpaper slideshow & rotation manager with **60 FPS Wayland Layer-Shell crossfades** (currently broken, only swaps statically, to be fixed soon), built for **CachyOS (COSMIC Desktop & Wayland compositors)**.

## Why This Exists
COSMIC Settings' built-in wallpaper rotation only rotates when pointed at a single directory source, and does not crossfade or rotate custom multi-folder image selections. `cachyos-wallpaper-slideshow` provides:
- **Arbitrary Photo Selection**: Load photos from anywhere (`~/Downloads`, `~/Pictures`, or drag-and-drop), toggle individual wallpapers in/out of the active rotation pool, and save your selection.
- **Silky 60 FPS Smoothstep Crossfades**: Renders directly to `zwlr_layer_shell_v1` (`Layer::Background`) across all monitors using parallel `rayon` pixel blending, while keeping COSMIC's background configuration synced.
- **Flexible Timing + Randomization (Jitter)**: Set the base switch interval in **Seconds**, **Minutes**, or **Hours** using both a slider and a direct numeric input field, and optionally add a **random time offset (`± variance`)** to each switch so transitions never feel mechanical or uniform.
- **Shuffle & Sequential Modes**: Switch between a non-repeating random shuffle deck and sequential ordering.
- **Persistent Background Daemon**: Keeps rotating wallpapers in the background even when the GUI is closed, with one-click `systemd --user` login autostart.

## Usage

### Launch the GUI Manager
```bash
cargo run --release
# or after building:
./target/release/cachyos-wallpaper-slideshow
```

### CLI & Keybinding Controls
You can also control the background daemon from the terminal or bind keys in COSMIC Settings:
```bash
# Immediately crossfade to the next wallpaper in rotation
cachyos-wallpaper-slideshow next

# Crossfade back to the previous wallpaper
cachyos-wallpaper-slideshow prev

# Pause or resume the rotation timer
cachyos-wallpaper-slideshow toggle-pause

# Check current daemon status, active wallpaper, and countdown
cachyos-wallpaper-slideshow status

# Stop the background daemon
cachyos-wallpaper-slideshow stop
```

## Configuration
Saved automatically to `~/.config/cachyos-wallpaper-slideshow/config.json`.
