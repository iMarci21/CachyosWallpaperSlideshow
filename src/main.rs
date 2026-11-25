mod config;
mod daemon;
mod gui;
mod wayland;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::{format_duration_human, DaemonState, IpcCommand};

#[derive(Parser)]
#[command(
    name = "cachyos-wallpaper-slideshow",
    version,
    about = "Smooth 60 FPS Wayland wallpaper slideshow & rotation manager for CachyOS / COSMIC"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the graphical wallpaper rotation manager (default)
    Gui,
    /// Run the background Wayland crossfade slideshow daemon
    Daemon,
    /// Immediately crossfade to the next wallpaper in the rotation
    Next,
    /// Immediately crossfade to the previous wallpaper
    Prev,
    /// Pause or resume the wallpaper rotation timer
    TogglePause,
    /// Reload configuration in the running daemon
    Reload,
    /// Stop the running background daemon
    Stop,
    /// Show the current status of the wallpaper slideshow daemon
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Gui) {
        Commands::Gui => gui::run_gui(),
        Commands::Daemon => daemon::run_daemon(),
        Commands::Next => {
            if !daemon::is_daemon_running() {
                daemon::spawn_daemon_background()?;
            } else {
                daemon::send_ipc_command(&IpcCommand::Next)?;
            }
            Ok(())
        }
        Commands::Prev => {
            daemon::send_ipc_command(&IpcCommand::Prev)?;
            Ok(())
        }
        Commands::TogglePause => {
            daemon::send_ipc_command(&IpcCommand::TogglePause)?;
            Ok(())
        }
        Commands::Reload => {
            daemon::send_ipc_command(&IpcCommand::Reload)?;
            Ok(())
        }
        Commands::Stop => {
            daemon::stop_daemon()?;
            println!("Stopped cachyos-wallpaper-slideshow daemon.");
            Ok(())
        }
        Commands::Status => {
            if let Some(state) = DaemonState::load() {
                println!("Status: Running (PID {})", state.pid);
                println!("Backend: {}", state.backend_info);
                if let Some(period) = state.active_period {
                    println!("Active Period: {} {}", period.icon(), period.as_str());
                }
                println!("Paused: {}", state.paused);
                println!(
                    "Active Rotation Pool: {} wallpapers",
                    state.active_rotation_count
                );
                if let Some(curr) = &state.current_wallpaper {
                    println!("Current Wallpaper: {}", curr.display());
                }
                if let Some(next) = &state.next_wallpaper {
                    println!("Next Wallpaper: {}", next.display());
                }
                if !state.paused {
                    println!(
                        "Next Switch In: {} (cycle target: {})",
                        format_duration_human(state.remaining_secs()),
                        format_duration_human(state.cycle_duration_ms as f64 / 1000.0)
                    );
                }
            } else {
                println!("Status: Stopped");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{
        create_brightness_lut, format_time_hhmm, OrderMode, SlideshowConfig, TimePeriod, TimeUnit,
        WallpaperSchedule,
    };
    use crate::wayland::blend_xrgb8888;

    #[test]
    fn test_interval_and_randomization_bounds() {
        let mut cfg = SlideshowConfig {
            wallpapers: Vec::new(),
            interval_value: 5.0,
            interval_unit: TimeUnit::Minutes,
            randomize_interval: true,
            jitter_value: 1.5,
            order_mode: OrderMode::Shuffle,
            fade_duration_secs: 1.5,
            scaling_mode: crate::config::ScalingMode::Zoom,
            paused: false,
            sync_cosmic_config: false,
            day_start_minutes: 7 * 60,
            night_start_minutes: 20 * 60,
        };

        let (min_s, max_s) = cfg.interval_bounds_secs();
        assert!((min_s - 210.0).abs() < 1e-6); // 3.5 minutes = 210s
        assert!((max_s - 390.0).abs() < 1e-6); // 6.5 minutes = 390s

        for _ in 0..50 {
            let sampled = cfg.sample_next_duration().as_secs_f64();
            assert!(sampled >= 210.0 && sampled <= 390.0);
        }

        cfg.randomize_interval = false;
        let (min_fixed, max_fixed) = cfg.interval_bounds_secs();
        assert!((min_fixed - 300.0).abs() < 1e-6);
        assert!((max_fixed - 300.0).abs() < 1e-6);
    }

    #[test]
    fn test_smoothstep_crossfade_blending() {
        let from = vec![0u8, 100u8, 200u8, 255u8];
        let to = vec![200u8, 100u8, 0u8, 255u8];
        let mut dst = vec![0u8; 4];

        blend_xrgb8888(&from, &to, &mut dst, 0.0);
        assert_eq!(dst, from);

        blend_xrgb8888(&from, &to, &mut dst, 1.0);
        assert_eq!(dst, to);

        blend_xrgb8888(&from, &to, &mut dst, 0.5);
        assert_eq!(dst, vec![100u8, 100u8, 100u8, 255u8]);
    }

    #[test]
    fn test_brightness_lut() {
        // 0% brightness must be identity
        let lut_identity = create_brightness_lut(0);
        for i in 0..256 {
            assert_eq!(lut_identity[i], i as u8);
        }

        // -100% brightness must be completely black
        let lut_black = create_brightness_lut(-100);
        for &val in &lut_black {
            assert_eq!(val, 0);
        }

        // +100% brightness must be completely white
        let lut_white = create_brightness_lut(100);
        for &val in &lut_white {
            assert_eq!(val, 255);
        }

        // Clamping check
        assert_eq!(create_brightness_lut(-150), lut_black);
        assert_eq!(create_brightness_lut(150), lut_white);

        // Monotonicity check for intermediate values
        for &b in &[-75, -50, -25, 25, 50, 75] {
            let lut = create_brightness_lut(b);
            for i in 1..256 {
                assert!(
                    lut[i] >= lut[i - 1],
                    "LUT must be monotonic at brightness {}",
                    b
                );
            }
        }
    }

    #[test]
    fn test_day_night_schedule_boundaries_and_matching() {
        let mut cfg = SlideshowConfig {
            wallpapers: Vec::new(),
            day_start_minutes: 7 * 60,    // 07:00
            night_start_minutes: 20 * 60, // 20:00
            ..SlideshowConfig::default()
        };

        assert_eq!(format_time_hhmm(7 * 60), "07:00");
        assert_eq!(format_time_hhmm(20 * 60 + 5), "20:05");

        // Normal schedule (07:00..20:00 is Day)
        assert_eq!(cfg.period_for_minutes(0), TimePeriod::Night);
        assert_eq!(cfg.period_for_minutes(6 * 60 + 59), TimePeriod::Night);
        assert_eq!(cfg.period_for_minutes(7 * 60), TimePeriod::Day);
        assert_eq!(cfg.period_for_minutes(12 * 60), TimePeriod::Day);
        assert_eq!(cfg.period_for_minutes(19 * 60 + 59), TimePeriod::Day);
        assert_eq!(cfg.period_for_minutes(20 * 60), TimePeriod::Night);
        assert_eq!(cfg.period_for_minutes(23 * 60 + 59), TimePeriod::Night);

        // Wrapped schedule (e.g., Day starts at 21:00, Night starts at 06:00)
        cfg.day_start_minutes = 21 * 60;
        cfg.night_start_minutes = 6 * 60;
        assert_eq!(cfg.period_for_minutes(22 * 60), TimePeriod::Day);
        assert_eq!(cfg.period_for_minutes(2 * 60), TimePeriod::Day);
        assert_eq!(cfg.period_for_minutes(6 * 60), TimePeriod::Night);
        assert_eq!(cfg.period_for_minutes(15 * 60), TimePeriod::Night);
        assert_eq!(cfg.period_for_minutes(20 * 60 + 59), TimePeriod::Night);
        assert_eq!(cfg.period_for_minutes(21 * 60), TimePeriod::Day);

        // WallpaperSchedule matching
        assert!(WallpaperSchedule::Always.matches_period(TimePeriod::Day));
        assert!(WallpaperSchedule::Always.matches_period(TimePeriod::Night));
        assert!(WallpaperSchedule::Day.matches_period(TimePeriod::Day));
        assert!(!WallpaperSchedule::Day.matches_period(TimePeriod::Night));
        assert!(!WallpaperSchedule::Night.matches_period(TimePeriod::Day));
        assert!(WallpaperSchedule::Night.matches_period(TimePeriod::Night));

        // Backward-compatible config deserialization (missing schedule fields default cleanly)
        let legacy_json = r#"{
            "wallpapers": [{"path": "/tmp/a.png", "enabled": true, "brightness": -20}],
            "interval_value": 10.0,
            "interval_unit": "Minutes",
            "randomize_interval": false,
            "jitter_value": 0.0,
            "order_mode": "Shuffle",
            "fade_duration_secs": 2.5,
            "scaling_mode": "Zoom",
            "paused": false,
            "sync_cosmic_config": true
        }"#;
        let parsed: SlideshowConfig = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.day_start_minutes, 7 * 60);
        assert_eq!(parsed.night_start_minutes, 20 * 60);
        assert_eq!(parsed.wallpapers[0].schedule, WallpaperSchedule::Always);
        assert_eq!(parsed.wallpapers[0].brightness, -20);
    }
}
