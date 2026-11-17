use crate::config::{
    now_unix_ms, sync_cosmic_background_file, DaemonState, IpcCommand, OrderMode, SlideshowConfig,
};
use crate::wayland::{DecodedWallpaper, WaylandRenderer};
use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct RotationPlaylist {
    enabled: Vec<PathBuf>,
    order_mode: OrderMode,
    seq_index: usize,
    shuffle_bag: Vec<PathBuf>,
    history: Vec<PathBuf>,
    current: Option<PathBuf>,
    planned_next: Option<PathBuf>,
}

impl RotationPlaylist {
    fn new(config: &SlideshowConfig) -> Self {
        let mut p = Self {
            enabled: Vec::new(),
            order_mode: config.order_mode,
            seq_index: 0,
            shuffle_bag: Vec::new(),
            history: Vec::new(),
            current: None,
            planned_next: None,
        };
        p.rebuild(config);
        p
    }

    fn rebuild(&mut self, config: &SlideshowConfig) {
        let new_enabled = config.enabled_wallpapers();
        let mode_changed = self.order_mode != config.order_mode;
        let list_changed = self.enabled != new_enabled;

        self.enabled = new_enabled;
        self.order_mode = config.order_mode;

        if self.enabled.is_empty() {
            self.shuffle_bag.clear();
            self.planned_next = None;
            return;
        }

        if list_changed || mode_changed {
            self.shuffle_bag.clear();
            if let Some(curr) = &self.current {
                if let Some(pos) = self.enabled.iter().position(|p| p == curr) {
                    self.seq_index = (pos + 1) % self.enabled.len();
                } else {
                    self.seq_index %= self.enabled.len();
                }
            } else {
                self.seq_index = 0;
            }
            self.planned_next = None;
        }

        if self.planned_next.is_none()
            || !self
                .planned_next
                .as_ref()
                .map(|p| self.enabled.contains(p))
                .unwrap_or(false)
        {
            self.planned_next = self.draw_candidate();
        }
    }

    fn draw_candidate(&mut self) -> Option<PathBuf> {
        if self.enabled.is_empty() {
            return None;
        }
        if self.enabled.len() == 1 {
            return Some(self.enabled[0].clone());
        }

        match self.order_mode {
            OrderMode::Sequential => {
                let idx = self.seq_index % self.enabled.len();
                self.seq_index = (idx + 1) % self.enabled.len();
                let candidate = self.enabled[idx].clone();
                if Some(&candidate) == self.current.as_ref() && self.enabled.len() > 1 {
                    let next_idx = self.seq_index % self.enabled.len();
                    self.seq_index = (next_idx + 1) % self.enabled.len();
                    Some(self.enabled[next_idx].clone())
                } else {
                    Some(candidate)
                }
            }
            OrderMode::Shuffle => {
                if self.shuffle_bag.is_empty() {
                    let mut bag = self.enabled.clone();
                    let mut rng = rand::thread_rng();
                    bag.shuffle(&mut rng);
                    // Avoid immediate repeat across bag boundary when popping from end
                    if bag.len() > 1 && bag.last() == self.current.as_ref() {
                        let last_idx = bag.len() - 1;
                        bag.swap(0, last_idx);
                    }
                    self.shuffle_bag = bag;
                }
                self.shuffle_bag.pop()
            }
        }
    }

    fn advance_next(&mut self) -> Option<PathBuf> {
        if self.enabled.is_empty() {
            return None;
        }
        let next = self.planned_next.take().or_else(|| self.draw_candidate())?;
        if let Some(curr) = self.current.take() {
            if self.history.last() != Some(&curr) {
                self.history.push(curr);
                if self.history.len() > 64 {
                    self.history.remove(0);
                }
            }
        }
        self.current = Some(next.clone());
        self.planned_next = self.draw_candidate();
        Some(next)
    }

    fn advance_prev(&mut self) -> Option<PathBuf> {
        if let Some(prev) = self.history.pop() {
            if prev.is_file() {
                if let Some(curr) = self.current.take() {
                    self.planned_next = Some(curr);
                }
                self.current = Some(prev.clone());
                return Some(prev);
            }
        }
        self.advance_next()
    }

    fn force_current(&mut self, path: PathBuf) {
        if let Some(curr) = self.current.take() {
            if curr != path {
                self.history.push(curr);
            }
        }
        self.current = Some(path);
        if self.planned_next == self.current {
            self.planned_next = self.draw_candidate();
        }
    }
}

pub fn send_ipc_command(cmd: &IpcCommand) -> Result<()> {
    let sock_path = DaemonState::socket_path();
    let mut stream = UnixStream::connect(&sock_path)
        .context("Could not connect to wallpaper daemon socket (is the daemon running?)")?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let payload = serde_json::to_vec(cmd)?;
    stream.write_all(&payload)?;
    Ok(())
}

pub fn is_daemon_running() -> bool {
    DaemonState::load().is_some()
}

pub fn spawn_daemon_background() -> Result<()> {
    if is_daemon_running() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("Failed to locate current executable")?;
    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn background daemon")?;
    Ok(())
}

pub fn systemd_service_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("systemd/user/cachyos-wallpaper-slideshow.service")
}

pub fn is_systemd_autostart_enabled() -> bool {
    systemd_service_path().exists()
}

pub fn set_systemd_autostart(enable: bool) -> Result<()> {
    let service_path = systemd_service_path();
    if enable {
        if let Some(parent) = service_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let exe = std::env::current_exe().context("Failed to resolve executable path")?;
        let unit = format!(
            "[Unit]\n\
             Description=CachyOS Wallpaper Slideshow Daemon (Smooth Crossfade)\n\
             After=graphical-session.target\n\
             PartOf=graphical-session.target\n\n\
             [Service]\n\
             Type=simple\n\
             ExecStart=\"{}\" daemon\n\
             Restart=on-failure\n\
             RestartSec=3\n\n\
             [Install]\n\
             WantedBy=graphical-session.target default.target\n",
            exe.display()
        );
        fs::write(&service_path, unit)?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let _ = Command::new("systemctl")
            .args([
                "--user",
                "enable",
                "--now",
                "cachyos-wallpaper-slideshow.service",
            ])
            .status();
    } else {
        let _ = Command::new("systemctl")
            .args([
                "--user",
                "disable",
                "--now",
                "cachyos-wallpaper-slideshow.service",
            ])
            .status();
        if service_path.exists() {
            let _ = fs::remove_file(&service_path);
        }
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }
    Ok(())
}

fn apply_wallpaper(
    renderer: &mut Option<WaylandRenderer>,
    path: &Path,
    config: &SlideshowConfig,
    fade_secs: f32,
) {
    if let Some(wl) = renderer.as_mut() {
        match DecodedWallpaper::load(path) {
            Ok(decoded) => {
                if let Err(err) = wl.transition_to(decoded, config.scaling_mode, fade_secs) {
                    eprintln!("Wayland transition error: {err:#}");
                }
            }
            Err(err) => {
                eprintln!("Failed to decode image {}: {err:#}", path.display());
            }
        }
    }
    if config.sync_cosmic_config {
        sync_cosmic_background_file(path, config.scaling_mode);
    }
}

pub fn run_daemon() -> Result<()> {
    // If another instance is already running, exit cleanly
    if is_daemon_running() {
        return Ok(());
    }

    let sock_path = DaemonState::socket_path();
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("Failed to bind IPC socket at {}", sock_path.display()))?;
    listener.set_nonblocking(true)?;

    let mut config = SlideshowConfig::load();
    let mut renderer = match WaylandRenderer::new(config.scaling_mode) {
        Ok(r) => Some(r),
        Err(err) => {
            eprintln!(
                "Wayland LayerShell unavailable ({err:#}), falling back to COSMIC config sync"
            );
            None
        }
    };

    let backend_info = if renderer.is_some() {
        "Wayland Layer-Shell (60 FPS Crossfade)".to_string()
    } else {
        "COSMIC Config Sync".to_string()
    };

    let mut playlist = RotationPlaylist::new(&config);
    let mut state = DaemonState {
        pid: std::process::id(),
        running: true,
        paused: config.paused,
        current_wallpaper: None,
        next_wallpaper: None,
        cycle_started_unix_ms: now_unix_ms(),
        cycle_duration_ms: 0,
        active_rotation_count: playlist.enabled.len(),
        transitioning: false,
        backend_info,
        updated_unix_ms: now_unix_ms(),
    };

    // Perform initial wallpaper setup immediately
    if let Some(first_wp) = playlist.advance_next() {
        state.transitioning = true;
        state.current_wallpaper = Some(first_wp.clone());
        state.next_wallpaper = playlist.planned_next.clone();
        state.save();

        // First frame fast fade (0.4s) so startup feels responsive
        apply_wallpaper(&mut renderer, &first_wp, &config, 0.4);
        state.transitioning = false;
    }

    let mut cycle_start = Instant::now();
    let mut current_target_duration = config.sample_next_duration();
    state.cycle_started_unix_ms = now_unix_ms();
    state.cycle_duration_ms = current_target_duration.as_millis() as u64;
    state.next_wallpaper = playlist.planned_next.clone();
    state.save();

    let mut last_config_mtime = fs::metadata(SlideshowConfig::config_path())
        .and_then(|m| m.modified())
        .ok();

    loop {
        if let Some(wl) = renderer.as_mut() {
            let _ = wl.dispatch_pending();
        }

        // Check if config file was updated on disk
        let current_mtime = fs::metadata(SlideshowConfig::config_path())
            .and_then(|m| m.modified())
            .ok();
        let mut should_reload = false;
        if current_mtime != last_config_mtime && current_mtime.is_some() {
            last_config_mtime = current_mtime;
            should_reload = true;
        }

        // Process incoming IPC commands
        while let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = Vec::new();
            if stream.read_to_end(&mut buf).is_ok() {
                if let Ok(cmd) = serde_json::from_slice::<IpcCommand>(&buf) {
                    match cmd {
                        IpcCommand::Reload => {
                            should_reload = true;
                        }
                        IpcCommand::TogglePause => {
                            config.paused = !config.paused;
                            let _ = config.save();
                            last_config_mtime = fs::metadata(SlideshowConfig::config_path())
                                .and_then(|m| m.modified())
                                .ok();
                            state.paused = config.paused;
                            if !config.paused {
                                cycle_start = Instant::now();
                                current_target_duration = config.sample_next_duration();
                                state.cycle_started_unix_ms = now_unix_ms();
                                state.cycle_duration_ms =
                                    current_target_duration.as_millis() as u64;
                            }
                            state.save();
                        }
                        IpcCommand::Next => {
                            if let Some(next_wp) = playlist.advance_next() {
                                state.transitioning = true;
                                state.current_wallpaper = Some(next_wp.clone());
                                state.next_wallpaper = playlist.planned_next.clone();
                                state.save();

                                apply_wallpaper(
                                    &mut renderer,
                                    &next_wp,
                                    &config,
                                    config.fade_duration_secs,
                                );

                                state.transitioning = false;
                                cycle_start = Instant::now();
                                current_target_duration = config.sample_next_duration();
                                state.cycle_started_unix_ms = now_unix_ms();
                                state.cycle_duration_ms =
                                    current_target_duration.as_millis() as u64;
                                state.save();
                            }
                        }
                        IpcCommand::Prev => {
                            if let Some(prev_wp) = playlist.advance_prev() {
                                state.transitioning = true;
                                state.current_wallpaper = Some(prev_wp.clone());
                                state.next_wallpaper = playlist.planned_next.clone();
                                state.save();

                                apply_wallpaper(
                                    &mut renderer,
                                    &prev_wp,
                                    &config,
                                    config.fade_duration_secs,
                                );

                                state.transitioning = false;
                                cycle_start = Instant::now();
                                current_target_duration = config.sample_next_duration();
                                state.cycle_started_unix_ms = now_unix_ms();
                                state.cycle_duration_ms =
                                    current_target_duration.as_millis() as u64;
                                state.save();
                            }
                        }
                        IpcCommand::SetWallpaper(target_path) => {
                            if target_path.is_file() {
                                playlist.force_current(target_path.clone());
                                state.transitioning = true;
                                state.current_wallpaper = Some(target_path.clone());
                                state.next_wallpaper = playlist.planned_next.clone();
                                state.save();

                                apply_wallpaper(
                                    &mut renderer,
                                    &target_path,
                                    &config,
                                    config.fade_duration_secs,
                                );

                                state.transitioning = false;
                                cycle_start = Instant::now();
                                current_target_duration = config.sample_next_duration();
                                state.cycle_started_unix_ms = now_unix_ms();
                                state.cycle_duration_ms =
                                    current_target_duration.as_millis() as u64;
                                state.save();
                            }
                        }
                        IpcCommand::Stop => {
                            let _ = fs::remove_file(&sock_path);
                            let _ = fs::remove_file(DaemonState::state_path());
                            return Ok(());
                        }
                    }
                }
            }
        }

        if should_reload {
            let new_cfg = SlideshowConfig::load();
            let timing_changed = (new_cfg.base_interval_secs() - config.base_interval_secs()).abs()
                > f64::EPSILON
                || new_cfg.randomize_interval != config.randomize_interval
                || (new_cfg.jitter_secs() - config.jitter_secs()).abs() > f64::EPSILON;
            let scaling_changed = new_cfg.scaling_mode != config.scaling_mode;

            config = new_cfg;
            playlist.rebuild(&config);
            state.paused = config.paused;
            state.active_rotation_count = playlist.enabled.len();
            state.next_wallpaper = playlist.planned_next.clone();

            // If current wallpaper was unselected or removed, switch immediately to an enabled one
            let current_still_enabled = state
                .current_wallpaper
                .as_ref()
                .map(|c| playlist.enabled.contains(c))
                .unwrap_or(false);

            if !current_still_enabled && !playlist.enabled.is_empty() {
                if let Some(next_wp) = playlist.advance_next() {
                    state.transitioning = true;
                    state.current_wallpaper = Some(next_wp.clone());
                    state.next_wallpaper = playlist.planned_next.clone();
                    state.save();

                    apply_wallpaper(&mut renderer, &next_wp, &config, config.fade_duration_secs);
                    state.transitioning = false;
                }
                cycle_start = Instant::now();
                current_target_duration = config.sample_next_duration();
                state.cycle_started_unix_ms = now_unix_ms();
                state.cycle_duration_ms = current_target_duration.as_millis() as u64;
            } else if scaling_changed {
                if let Some(curr) = state.current_wallpaper.clone() {
                    apply_wallpaper(&mut renderer, &curr, &config, 0.25);
                }
            }

            if timing_changed {
                cycle_start = Instant::now();
                current_target_duration = config.sample_next_duration();
                state.cycle_started_unix_ms = now_unix_ms();
                state.cycle_duration_ms = current_target_duration.as_millis() as u64;
            }
            state.save();
        }

        // Check rotation timer
        if !config.paused
            && !playlist.enabled.is_empty()
            && cycle_start.elapsed() >= current_target_duration
        {
            if let Some(next_wp) = playlist.advance_next() {
                state.transitioning = true;
                state.current_wallpaper = Some(next_wp.clone());
                state.next_wallpaper = playlist.planned_next.clone();
                state.save();

                apply_wallpaper(&mut renderer, &next_wp, &config, config.fade_duration_secs);
                state.transitioning = false;
            }
            cycle_start = Instant::now();
            current_target_duration = config.sample_next_duration();
            state.cycle_started_unix_ms = now_unix_ms();
            state.cycle_duration_ms = current_target_duration.as_millis() as u64;
            state.next_wallpaper = playlist.planned_next.clone();
            state.save();
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}
