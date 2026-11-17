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
    use crate::config::{OrderMode, SlideshowConfig, TimeUnit};
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
}
