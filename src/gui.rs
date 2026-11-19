use crate::config::{
    collect_images_from_dir, format_duration_human, is_supported_image, DaemonState, IpcCommand,
    OrderMode, ScalingMode, SlideshowConfig, TimeUnit,
};
use crate::daemon::{
    is_daemon_running, is_systemd_autostart_enabled, send_ipc_command, set_systemd_autostart,
    spawn_daemon_background, stop_daemon,
};
use eframe::egui::{
    self, Color32, CornerRadius, FontId, RichText, Sense, Stroke, TextureHandle, TextureOptions,
    Vec2,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

struct ThumbnailResult {
    path: PathBuf,
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    orig_dims: (u32, u32),
}

enum ThumbState {
    Loading,
    Ready {
        texture: TextureHandle,
        orig_dims: (u32, u32),
    },
    Failed,
}

pub struct SlideshowApp {
    config: SlideshowConfig,
    saved_config: SlideshowConfig,
    auto_save: bool,
    status_toast: Option<(String, Instant)>,
    daemon_state: Option<DaemonState>,
    last_state_poll: Instant,
    systemd_autostart: bool,
    search_query: String,
    filter_only_selected: bool,
    thumbnails: HashMap<PathBuf, ThumbState>,
    thumb_tx: Sender<ThumbnailResult>,
    thumb_rx: Receiver<ThumbnailResult>,
    in_flight_thumbs: HashSet<PathBuf>,
}

impl SlideshowApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::configure_theme(&cc.egui_ctx);

        let config = SlideshowConfig::load();
        let saved_config = config.clone();

        // Automatically start the background daemon if not already running
        if !is_daemon_running() {
            let _ = spawn_daemon_background();
        }

        let (thumb_tx, thumb_rx) = channel();

        Self {
            config,
            saved_config,
            auto_save: false,
            status_toast: None,
            daemon_state: DaemonState::load(),
            last_state_poll: Instant::now() - Duration::from_secs(5),
            systemd_autostart: is_systemd_autostart_enabled(),
            search_query: String::new(),
            filter_only_selected: false,
            thumbnails: HashMap::new(),
            thumb_tx,
            thumb_rx,
            in_flight_thumbs: HashSet::new(),
        }
    }

    fn configure_theme(ctx: &egui::Context) {
        let mut visuals = egui::Visuals::dark();
        // CachyOS / COSMIC inspired dark slate + emerald/cyan accent palette
        visuals.window_fill = Color32::from_rgb(17, 20, 28);
        visuals.panel_fill = Color32::from_rgb(22, 26, 36);
        visuals.faint_bg_color = Color32::from_rgb(28, 33, 46);
        visuals.extreme_bg_color = Color32::from_rgb(12, 14, 20);
        visuals.selection.bg_fill = Color32::from_rgb(0, 180, 150);
        visuals.selection.stroke = Stroke::new(1.5_f32, Color32::from_rgb(80, 245, 210));
        visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(28, 34, 46);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(35, 42, 58);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(46, 56, 78);
        visuals.widgets.active.bg_fill = Color32::from_rgb(0, 165, 140);
        ctx.set_visuals(visuals);
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.status_toast = Some((msg.into(), Instant::now()));
    }

    fn has_unsaved_changes(&self) -> bool {
        self.config != self.saved_config
    }

    fn save_and_notify_daemon(&mut self, show_msg: bool) {
        self.config.sanitize();
        if let Err(err) = self.config.save() {
            self.show_toast(format!("Failed to save config: {err}"));
            return;
        }
        self.saved_config = self.config.clone();
        if is_daemon_running() {
            let _ = send_ipc_command(&IpcCommand::Reload);
        } else {
            let _ = spawn_daemon_background();
        }
        if show_msg {
            let enabled_count = self.config.enabled_wallpapers().len();
            self.show_toast(format!(
                "Saved rotation ({enabled_count} wallpapers active)"
            ));
        }
    }

    fn maybe_auto_save(&mut self) {
        if self.auto_save && self.has_unsaved_changes() {
            self.save_and_notify_daemon(false);
        }
    }

    fn request_thumbnail(&mut self, path: &PathBuf, ctx: &egui::Context) {
        if self.thumbnails.contains_key(path) || self.in_flight_thumbs.contains(path) {
            return;
        }
        if self.in_flight_thumbs.len() >= 8 {
            return;
        }

        self.in_flight_thumbs.insert(path.clone());
        self.thumbnails.insert(path.clone(), ThumbState::Loading);

        let path_clone = path.clone();
        let tx = self.thumb_tx.clone();
        let ctx_clone = ctx.clone();

        rayon::spawn(move || {
            if let Ok(img) = image::open(&path_clone) {
                let orig_dims = (img.width(), img.height());
                let thumb = img.thumbnail(320, 180).to_rgba8();
                let width = thumb.width() as usize;
                let height = thumb.height() as usize;
                let pixels = thumb.into_raw();
                let _ = tx.send(ThumbnailResult {
                    path: path_clone,
                    width,
                    height,
                    pixels,
                    orig_dims,
                });
            } else {
                let _ = tx.send(ThumbnailResult {
                    path: path_clone,
                    width: 0,
                    height: 0,
                    pixels: Vec::new(),
                    orig_dims: (0, 0),
                });
            }
            ctx_clone.request_repaint();
        });
    }

    fn poll_thumbnails(&mut self, ctx: &egui::Context) {
        while let Ok(res) = self.thumb_rx.try_recv() {
            self.in_flight_thumbs.remove(&res.path);
            if res.width > 0 && res.height > 0 {
                let color_image =
                    egui::ColorImage::from_rgba_unmultiplied([res.width, res.height], &res.pixels);
                let texture = ctx.load_texture(
                    res.path.to_string_lossy(),
                    color_image,
                    TextureOptions::LINEAR,
                );
                self.thumbnails.insert(
                    res.path,
                    ThumbState::Ready {
                        texture,
                        orig_dims: res.orig_dims,
                    },
                );
            } else {
                self.thumbnails.insert(res.path, ThumbState::Failed);
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let mut to_add = Vec::new();
        for file in dropped {
            if let Some(path) = file.path {
                if path.is_dir() {
                    to_add.extend(collect_images_from_dir(&path));
                } else if is_supported_image(&path) {
                    to_add.push(path);
                }
            }
        }
        if !to_add.is_empty() {
            let added = self.config.add_paths(to_add);
            if added > 0 {
                self.maybe_auto_save();
                self.show_toast(format!("Added {added} photo(s) via drag & drop"));
            }
        }
    }

    fn render_top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.heading(
                RichText::new("🖼 CachyOS Wallpaper Slideshow")
                    .color(Color32::from_rgb(90, 240, 210))
                    .strong(),
            );
            ui.add_space(12.0);

            let running = self.daemon_state.is_some();
            let paused = self
                .daemon_state
                .as_ref()
                .map(|s| s.paused)
                .unwrap_or(self.config.paused);

            let (badge_text, badge_color) = if !running {
                ("● Daemon Stopped", Color32::from_rgb(235, 95, 95))
            } else if paused {
                ("⏸ Rotation Paused", Color32::from_rgb(240, 190, 75))
            } else {
                ("● Smooth Rotation Active", Color32::from_rgb(70, 230, 155))
            };

            egui::Frame::NONE
                .fill(Color32::from_rgb(30, 36, 50))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(egui::Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.label(RichText::new(badge_text).color(badge_color).strong());
                });

            if let Some(state) = &self.daemon_state {
                ui.label(
                    RichText::new(format!("• {}", state.backend_info))
                        .small()
                        .color(Color32::from_gray(165)),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let stop_quit_btn = egui::Button::new(
                    RichText::new("⏹ Stop Daemon & Quit")
                        .strong()
                        .color(Color32::from_rgb(255, 225, 225)),
                )
                .fill(Color32::from_rgb(135, 38, 48));

                if ui
                    .add(stop_quit_btn)
                    .on_hover_text(
                        "Completely stop the background slideshow daemon and close this window",
                    )
                    .clicked()
                {
                    let _ = stop_daemon();
                    self.daemon_state = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                let unsaved = self.has_unsaved_changes();
                let save_btn = egui::Button::new(
                    RichText::new(if unsaved {
                        "💾 Save & Apply Selection *"
                    } else {
                        "✔ Selection Saved"
                    })
                    .strong()
                    .color(if unsaved {
                        Color32::from_rgb(15, 20, 25)
                    } else {
                        Color32::from_gray(220)
                    }),
                )
                .fill(if unsaved {
                    Color32::from_rgb(65, 225, 180)
                } else {
                    Color32::from_rgb(42, 50, 68)
                });

                if ui.add(save_btn).clicked() {
                    self.save_and_notify_daemon(true);
                }

                ui.checkbox(&mut self.auto_save, "Auto-apply");

                if let Some((msg, ts)) = &self.status_toast {
                    if ts.elapsed() < Duration::from_secs(4) {
                        ui.label(
                            RichText::new(msg)
                                .color(Color32::from_rgb(120, 240, 195))
                                .italics(),
                        );
                    }
                }
            });
        });
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // 1. Live Playback & Countdown Card
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(26, 31, 44))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("⏱ Live Rotation Status")
                            .font(FontId::proportional(16.0))
                            .strong()
                            .color(Color32::from_rgb(120, 235, 210)),
                    );
                    ui.add_space(6.0);

                    if let Some(state) = self.daemon_state.clone() {
                        let curr_name = state
                            .current_wallpaper
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "None".to_string());
                        let next_name = state
                            .next_wallpaper
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "None".to_string());

                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Current:").color(Color32::from_gray(165)));
                            ui.label(RichText::new(curr_name).strong());
                        });

                        // Current wallpaper live brightness slider
                        if let Some(curr_path) = &state.current_wallpaper {
                            if let Some(idx) = self
                                .config
                                .wallpapers
                                .iter()
                                .position(|w| &w.path == curr_path)
                            {
                                let mut brightness = self.config.wallpapers[idx].brightness;
                                let mut brightness_changed = false;
                                ui.horizontal(|ui| {
                                    let b_color = if brightness == 0 {
                                        Color32::from_gray(160)
                                    } else if brightness > 0 {
                                        Color32::from_rgb(255, 215, 80)
                                    } else {
                                        Color32::from_rgb(100, 165, 240)
                                    };
                                    ui.label(
                                        RichText::new("☀ Brightness:")
                                            .small()
                                            .color(b_color)
                                            .strong(),
                                    );
                                    let s_resp = ui.add(
                                        egui::Slider::new(&mut brightness, -100..=100)
                                            .suffix("%")
                                            .show_value(true),
                                    );
                                    if s_resp.changed() {
                                        brightness_changed = true;
                                    }
                                    if brightness != 0
                                        && ui
                                            .small_button("↺")
                                            .on_hover_text("Reset to 0%")
                                            .clicked()
                                    {
                                        brightness = 0;
                                        brightness_changed = true;
                                    }
                                });
                                if brightness_changed {
                                    self.config.wallpapers[idx].brightness = brightness;
                                    self.maybe_auto_save();
                                }
                            }
                        }

                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Up Next:").color(Color32::from_gray(165)));
                            ui.label(RichText::new(next_name).color(Color32::from_gray(205)));
                        });

                        ui.add_space(6.0);
                        let rem = state.remaining_secs();
                        let total_cycle = state.cycle_duration_ms as f64 / 1000.0;
                        let prog = if state.paused {
                            0.0
                        } else {
                            state.progress_fraction()
                        };

                        let bar_text = if state.transitioning {
                            "Crossfading wallpaper now...".to_string()
                        } else if state.paused {
                            "Paused".to_string()
                        } else {
                            format!(
                                "Next switch in {} (cycle: {})",
                                format_duration_human(rem),
                                format_duration_human(total_cycle)
                            )
                        };

                        ui.add(
                            egui::ProgressBar::new(prog)
                                .text(bar_text)
                                .animate(state.transitioning),
                        );

                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("⏮ Prev").clicked() {
                                let _ = send_ipc_command(&IpcCommand::Prev);
                            }
                            let pause_label = if state.paused {
                                "▶ Resume"
                            } else {
                                "⏸ Pause"
                            };
                            if ui.button(pause_label).clicked() {
                                let _ = send_ipc_command(&IpcCommand::TogglePause);
                                self.config.paused = !state.paused;
                                self.saved_config.paused = self.config.paused;
                            }
                            if ui
                                .button(
                                    RichText::new("⏭ Fade Next Now")
                                        .color(Color32::from_rgb(90, 240, 200)),
                                )
                                .clicked()
                            {
                                if self.has_unsaved_changes() {
                                    self.save_and_notify_daemon(false);
                                }
                                let _ = send_ipc_command(&IpcCommand::Next);
                            }
                        });

                        ui.add_space(6.0);
                        let stop_btn = egui::Button::new(
                            RichText::new("⏹ Stop & Exit Background Daemon")
                                .color(Color32::from_rgb(255, 195, 195))
                                .strong(),
                        )
                        .fill(Color32::from_rgb(95, 32, 40));

                        if ui
                            .add_sized([ui.available_width(), 28.0], stop_btn)
                            .on_hover_text(
                                "Completely shut down the background slideshow daemon process",
                            )
                            .clicked()
                        {
                            let _ = stop_daemon();
                            self.daemon_state = None;
                            self.show_toast("Background daemon stopped completely");
                        }
                    } else {
                        ui.label(
                            RichText::new("Background daemon is currently stopped.")
                                .color(Color32::from_rgb(235, 140, 140)),
                        );
                        if ui.button("▶ Start Background Daemon Now").clicked() {
                            self.save_and_notify_daemon(false);
                            let _ = spawn_daemon_background();
                            self.show_toast("Started background slideshow daemon");
                        }
                    }

                    ui.add_space(6.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        let mut autostart = self.systemd_autostart;
                        if ui
                            .checkbox(&mut autostart, "Autostart daemon on login (systemd)")
                            .changed()
                        {
                            if let Err(err) = set_systemd_autostart(autostart) {
                                self.show_toast(format!("Systemd update error: {err}"));
                            } else {
                                self.systemd_autostart = autostart;
                                self.show_toast(if autostart {
                                    "Enabled systemd user service for login autostart"
                                } else {
                                    "Disabled systemd autostart service"
                                });
                            }
                        }
                    });
                });

            ui.add_space(12.0);

            // 2. Time Interval & Randomization Card
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(26, 31, 44))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("⏳ Switch Interval & Jitter")
                            .font(FontId::proportional(16.0))
                            .strong()
                            .color(Color32::from_rgb(120, 235, 210)),
                    );
                    ui.add_space(6.0);

                    // Time unit selector
                    ui.horizontal(|ui| {
                        ui.label("Time Unit:");
                        for unit in [TimeUnit::Seconds, TimeUnit::Minutes, TimeUnit::Hours] {
                            let selected = self.config.interval_unit == unit;
                            if ui.selectable_label(selected, unit.as_str()).clicked() && !selected {
                                // Convert current value smoothly or keep sensible defaults
                                let old_secs = self.config.base_interval_secs();
                                let old_jitter_secs = self.config.jitter_secs();
                                self.config.interval_unit = unit;
                                self.config.interval_value =
                                    ((old_secs / unit.multiplier_secs()) * 10.0).round() / 10.0;
                                if self.config.interval_value <= 0.1 {
                                    self.config.interval_value = 1.0;
                                }
                                self.config.jitter_value =
                                    ((old_jitter_secs / unit.multiplier_secs()) * 10.0).round()
                                        / 10.0;
                                self.maybe_auto_save();
                            }
                        }
                    });

                    ui.add_space(6.0);
                    let suffix = self.config.interval_unit.short_suffix();
                    let range = self.config.interval_unit.slider_range();

                    ui.label(RichText::new("Base Time Between Switches:").strong());
                    ui.horizontal(|ui| {
                        let slider_resp = ui.add(
                            egui::Slider::new(&mut self.config.interval_value, range)
                                .step_by(0.5)
                                .suffix(format!(" {suffix}")),
                        );
                        let field_resp = ui.add(
                            egui::DragValue::new(&mut self.config.interval_value)
                                .speed(0.5)
                                .range(0.1..=10000.0)
                                .suffix(format!(" {suffix}")),
                        );
                        if slider_resp.changed() || field_resp.changed() {
                            self.maybe_auto_save();
                        }
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    if ui
                        .checkbox(
                            &mut self.config.randomize_interval,
                            RichText::new("🎲 Randomize time interval (Jitter)").strong(),
                        )
                        .changed()
                    {
                        self.maybe_auto_save();
                    }
                    ui.label(
                        RichText::new(
                            "Adds/subtracts a random offset each cycle so switches aren't uniform.",
                        )
                        .small()
                        .color(Color32::from_gray(165)),
                    );

                    if self.config.randomize_interval {
                        ui.add_space(4.0);
                        let max_jitter = self
                            .config
                            .interval_unit
                            .jitter_slider_max(self.config.interval_value);
                        ui.horizontal(|ui| {
                            ui.label("± Variance:");
                            let j_slider = ui.add(
                                egui::Slider::new(&mut self.config.jitter_value, 0.0..=max_jitter)
                                    .step_by(0.25)
                                    .suffix(format!(" {suffix}")),
                            );
                            let j_field = ui.add(
                                egui::DragValue::new(&mut self.config.jitter_value)
                                    .speed(0.25)
                                    .range(0.0..=10000.0)
                                    .suffix(format!(" {suffix}")),
                            );
                            if j_slider.changed() || j_field.changed() {
                                self.maybe_auto_save();
                            }
                        });
                    }

                    let (min_s, max_s) = self.config.interval_bounds_secs();
                    ui.add_space(4.0);
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(18, 22, 32))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            if (max_s - min_s).abs() < 0.5 {
                                ui.label(
                                    RichText::new(format!(
                                        "Fixed switch every {}",
                                        format_duration_human(min_s)
                                    ))
                                    .color(Color32::from_rgb(110, 230, 195)),
                                );
                            } else {
                                ui.label(
                                    RichText::new(format!(
                                        "Effective window: {}  to  {}",
                                        format_duration_human(min_s),
                                        format_duration_human(max_s)
                                    ))
                                    .color(Color32::from_rgb(110, 230, 195)),
                                );
                            }
                        });
                });

            ui.add_space(12.0);

            // 3. Crossfade & Order Settings Card
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(26, 31, 44))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("✨ Crossfade & Order")
                            .font(FontId::proportional(16.0))
                            .strong()
                            .color(Color32::from_rgb(120, 235, 210)),
                    );
                    ui.add_space(6.0);

                    ui.label(RichText::new("Rotation Order:").strong());
                    ui.horizontal(|ui| {
                        for mode in [OrderMode::Shuffle, OrderMode::Sequential] {
                            let is_sel = self.config.order_mode == mode;
                            if ui.selectable_label(is_sel, mode.as_str()).clicked() && !is_sel {
                                self.config.order_mode = mode;
                                self.maybe_auto_save();
                            }
                        }
                    });

                    ui.add_space(8.0);
                    ui.label(RichText::new("Smooth Crossfade Duration:").strong());
                    ui.horizontal(|ui| {
                        let f_slider = ui.add(
                            egui::Slider::new(&mut self.config.fade_duration_secs, 0.5..=5.0)
                                .step_by(0.1)
                                .suffix(" s"),
                        );
                        let f_drag = ui.add(
                            egui::DragValue::new(&mut self.config.fade_duration_secs)
                                .speed(0.1)
                                .range(0.5..=10.0)
                                .suffix(" s"),
                        );
                        if f_slider.changed() || f_drag.changed() {
                            self.maybe_auto_save();
                        }
                    });

                    ui.add_space(8.0);
                    ui.label(RichText::new("Monitor Scaling Mode:").strong());
                    egui::ComboBox::from_id_salt("scaling_mode_combo")
                        .selected_text(self.config.scaling_mode.as_str())
                        .show_ui(ui, |ui| {
                            for mode in [ScalingMode::Zoom, ScalingMode::Fit, ScalingMode::Stretch]
                            {
                                if ui
                                    .selectable_value(
                                        &mut self.config.scaling_mode,
                                        mode,
                                        mode.as_str(),
                                    )
                                    .changed()
                                {
                                    self.maybe_auto_save();
                                }
                            }
                        });
                });
        });
    }

    fn render_gallery(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Gallery Toolbar
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(RichText::new("➕ Add Photos...").strong())
                .on_hover_text("Select one or more downloaded photos to load into the list")
                .clicked()
            {
                let start_dir = dirs::download_dir()
                    .or_else(dirs::picture_dir)
                    .unwrap_or_else(|| PathBuf::from("/home"));
                if let Some(files) = rfd::FileDialog::new()
                    .set_directory(start_dir)
                    .add_filter(
                        "Images",
                        &[
                            "png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff", "pnm",
                        ],
                    )
                    .pick_files()
                {
                    let added = self.config.add_paths(files);
                    self.maybe_auto_save();
                    self.show_toast(format!("Loaded {added} new photo(s)"));
                }
            }

            if ui
                .button("📁 Add Folder...")
                .on_hover_text("Load all images from a folder")
                .clicked()
            {
                let start_dir = dirs::picture_dir()
                    .or_else(dirs::download_dir)
                    .unwrap_or_else(|| PathBuf::from("/home"));
                if let Some(dir) = rfd::FileDialog::new()
                    .set_directory(start_dir)
                    .pick_folder()
                {
                    let images = collect_images_from_dir(&dir);
                    let added = self.config.add_paths(images);
                    self.maybe_auto_save();
                    self.show_toast(format!("Loaded {added} photo(s) from folder"));
                }
            }

            if ui
                .button("📥 Import COSMIC Wallpapers")
                .on_hover_text("Import custom wallpapers configured in COSMIC Settings")
                .clicked()
            {
                let added = self.config.import_cosmic_wallpapers();
                self.maybe_auto_save();
                self.show_toast(format!("Imported {added} photo(s) from COSMIC Settings"));
            }

            ui.separator();

            if ui.button("✅ Select All").clicked() {
                for w in &mut self.config.wallpapers {
                    w.enabled = true;
                }
                self.maybe_auto_save();
            }

            if ui.button("⬜ Unselect All").clicked() {
                for w in &mut self.config.wallpapers {
                    w.enabled = false;
                }
                self.maybe_auto_save();
            }

            if ui.button("🔀 Invert").clicked() {
                for w in &mut self.config.wallpapers {
                    w.enabled = !w.enabled;
                }
                self.maybe_auto_save();
            }

            if ui
                .button(
                    RichText::new("🗑 Remove Unselected").color(Color32::from_rgb(235, 130, 130)),
                )
                .on_hover_text(
                    "Remove unchecked photos from the list (does not delete files on disk)",
                )
                .clicked()
            {
                let before = self.config.wallpapers.len();
                self.config.wallpapers.retain(|w| w.enabled);
                let removed = before.saturating_sub(self.config.wallpapers.len());
                self.maybe_auto_save();
                self.show_toast(format!("Removed {removed} unselected photo(s) from list"));
            }
        });

        ui.add_space(6.0);

        // Filter & Selection Counter Bar
        let total_count = self.config.wallpapers.len();
        let selected_count = self.config.wallpapers.iter().filter(|w| w.enabled).count();

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "Rotation Pool: {selected_count} of {total_count} selected"
                ))
                .strong()
                .color(Color32::from_rgb(110, 235, 200)),
            );
            ui.add_space(12.0);
            ui.checkbox(&mut self.filter_only_selected, "Show only selected");
            ui.add_space(12.0);
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Filter by filename or folder...")
                    .desired_width(220.0),
            );
            if !self.search_query.is_empty() && ui.small_button("✕").clicked() {
                self.search_query.clear();
            }
        });

        ui.separator();

        if self.config.wallpapers.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(
                        "No wallpapers loaded yet.\nClick '➕ Add Photos...', '📁 Add Folder...', or drag & drop images here!",
                    )
                    .font(FontId::proportional(17.0))
                    .color(Color32::from_gray(160)),
                );
            });
            return;
        }

        let current_active_path = self
            .daemon_state
            .as_ref()
            .and_then(|s| s.current_wallpaper.clone());

        let query = self.search_query.trim().to_ascii_lowercase();
        let only_sel = self.filter_only_selected;

        let visible_indices: Vec<usize> = self
            .config
            .wallpapers
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if only_sel && !item.enabled {
                    return false;
                }
                if !query.is_empty() {
                    let s = item.path.to_string_lossy().to_ascii_lowercase();
                    if !s.contains(&query) {
                        return false;
                    }
                }
                true
            })
            .map(|(i, _)| i)
            .collect();

        // Request thumbnails for visible items
        for &idx in &visible_indices {
            let path = self.config.wallpapers[idx].path.clone();
            self.request_thumbnail(&path, ctx);
        }

        let mut remove_idx: Option<usize> = None;
        let mut set_now_path: Option<PathBuf> = None;
        let mut any_toggled = false;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let avail_w = ui.available_width().max(240.0);
                let card_w = 235.0_f32;
                let spacing = 12.0_f32;
                let cols = ((avail_w + spacing) / (card_w + spacing)).floor().max(1.0) as usize;

                egui::Grid::new("wallpaper_cards_grid")
                    .spacing(Vec2::new(spacing, spacing))
                    .show(ui, |ui| {
                        for (grid_pos, &idx) in visible_indices.iter().enumerate() {
                            let item = &mut self.config.wallpapers[idx];
                            let is_active =
                                current_active_path.as_ref() == Some(&item.path);
                            let file_exists = item.path.is_file();

                            let border_color = if is_active {
                                Color32::from_rgb(70, 245, 190)
                            } else if item.enabled {
                                Color32::from_rgb(45, 145, 125)
                            } else {
                                Color32::from_rgb(45, 52, 68)
                            };

                            let card_bg = if item.enabled {
                                Color32::from_rgb(28, 35, 50)
                            } else {
                                Color32::from_rgb(19, 23, 32)
                            };

                            egui::Frame::NONE
                                .fill(card_bg)
                                .stroke(Stroke::new(
                                    if is_active { 2.2_f32 } else { 1.2_f32 },
                                    border_color,
                                ))
                                .corner_radius(CornerRadius::same(10))
                                .inner_margin(egui::Margin::same(8))
                                .show(ui, |ui| {
                                    ui.set_width(card_w - 16.0);
                                    ui.vertical(|ui| {
                                        // Thumbnail box (16:9 -> 219 x 123)
                                        let thumb_size = Vec2::new(card_w - 16.0, 124.0);
                                        let (rect, thumb_resp) =
                                            ui.allocate_exact_size(thumb_size, Sense::click());

                                        ui.painter().rect_filled(
                                            rect,
                                            CornerRadius::same(6),
                                            Color32::from_rgb(13, 16, 23),
                                        );

                                        let mut dims_label = String::new();
                                        match self.thumbnails.get(&item.path) {
                                            Some(ThumbState::Ready { texture, orig_dims }) => {
                                                dims_label =
                                                    format!("{}×{}", orig_dims.0, orig_dims.1);
                                                let tex_size = texture.size_vec2();
                                                if tex_size.x > 0.0 && tex_size.y > 0.0 {
                                                    let scale = (rect.width() / tex_size.x)
                                                        .min(rect.height() / tex_size.y);
                                                    let draw_size = tex_size * scale;
                                                    let draw_rect = egui::Rect::from_center_size(
                                                        rect.center(),
                                                        draw_size,
                                                    );

                                                    // Visual brightness simulation directly on the thumbnail!
                                                    let base_level = if item.enabled { 255.0 } else { 135.0 };
                                                    let tint = if item.brightness < 0 {
                                                        let factor = (1.0 + (item.brightness as f32 / 100.0)).clamp(0.0, 1.0);
                                                        let val = (base_level * factor).round() as u8;
                                                        Color32::from_rgb(val, val, val)
                                                    } else {
                                                        Color32::from_rgb(base_level as u8, base_level as u8, base_level as u8)
                                                    };

                                                    ui.painter().image(
                                                        texture.id(),
                                                        draw_rect,
                                                        egui::Rect::from_min_max(
                                                            egui::pos2(0.0, 0.0),
                                                            egui::pos2(1.0, 1.0),
                                                        ),
                                                        tint,
                                                    );

                                                    if item.brightness > 0 {
                                                        let b = (item.brightness as f32 / 100.0).clamp(0.0, 1.0);
                                                        let alpha = (b * 255.0).round() as u8;
                                                        ui.painter().rect_filled(
                                                            draw_rect,
                                                            CornerRadius::same(6),
                                                            Color32::from_rgba_unmultiplied(255, 255, 255, alpha),
                                                        );
                                                    }
                                                }
                                            }
                                            Some(ThumbState::Loading) | None => {
                                                ui.painter().text(
                                                    rect.center(),
                                                    egui::Align2::CENTER_CENTER,
                                                    "Loading preview...",
                                                    FontId::proportional(12.0),
                                                    Color32::from_gray(130),
                                                );
                                            }
                                            Some(ThumbState::Failed) => {
                                                ui.painter().text(
                                                    rect.center(),
                                                    egui::Align2::CENTER_CENTER,
                                                    if file_exists {
                                                        "Preview unavailable"
                                                    } else {
                                                        "Missing file"
                                                    },
                                                    FontId::proportional(12.0),
                                                    Color32::from_rgb(220, 110, 110),
                                                );
                                            }
                                        }

                                        // Active badge overlay
                                        if is_active {
                                            let badge_rect = egui::Rect::from_min_size(
                                                rect.min + Vec2::new(6.0, 6.0),
                                                Vec2::new(82.0, 20.0),
                                            );
                                            ui.painter().rect_filled(
                                                badge_rect,
                                                CornerRadius::same(4),
                                                Color32::from_rgba_unmultiplied(0, 185, 145, 230),
                                            );
                                            ui.painter().text(
                                                badge_rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                "ACTIVE NOW",
                                                FontId::proportional(10.5),
                                                Color32::from_rgb(10, 15, 18),
                                            );
                                        }

                                        // Clicking thumbnail toggles selection in rotation
                                        if thumb_resp
                                            .on_hover_text("Click to select / unselect for rotation")
                                            .clicked()
                                        {
                                            item.enabled = !item.enabled;
                                            any_toggled = true;
                                        }

                                        ui.add_space(6.0);

                                        // Filename
                                        let filename = item
                                            .path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().to_string())
                                            .unwrap_or_else(|| item.path.display().to_string());

                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&filename)
                                                    .strong()
                                                    .color(if item.enabled {
                                                        Color32::from_gray(240)
                                                    } else {
                                                        Color32::from_gray(145)
                                                    }),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(item.path.display().to_string());

                                        // Controls row: Checkbox + Resolution + "Fade Now" + Remove
                                        ui.horizontal(|ui| {
                                            if ui
                                                .checkbox(&mut item.enabled, "In Rotation")
                                                .changed()
                                            {
                                                any_toggled = true;
                                            }

                                            if !dims_label.is_empty() {
                                                ui.label(
                                                    RichText::new(dims_label)
                                                        .small()
                                                        .color(Color32::from_gray(135)),
                                                );
                                            }

                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui
                                                        .small_button("✕")
                                                        .on_hover_text("Remove from list")
                                                        .clicked()
                                                    {
                                                        remove_idx = Some(idx);
                                                    }
                                                    if ui
                                                        .small_button("▶")
                                                        .on_hover_text(
                                                            "Fade desktop to this wallpaper right now",
                                                        )
                                                        .clicked()
                                                    {
                                                        set_now_path = Some(item.path.clone());
                                                    }
                                                },
                                            );
                                        });

                                        // Brightness adjustment row (-100% to +100%)
                                        ui.add_space(2.0);
                                        ui.horizontal(|ui| {
                                            let b_color = if item.brightness == 0 {
                                                Color32::from_gray(160)
                                            } else if item.brightness > 0 {
                                                Color32::from_rgb(255, 215, 80)
                                            } else {
                                                Color32::from_rgb(100, 165, 240)
                                            };
                                            ui.label(RichText::new("☀").color(b_color).strong());
                                            let s_resp = ui.add(
                                                egui::Slider::new(&mut item.brightness, -100..=100)
                                                    .suffix("%")
                                                    .show_value(true),
                                            );
                                            if s_resp.changed() {
                                                any_toggled = true;
                                            }
                                            if item.brightness != 0
                                                && ui
                                                    .small_button("↺")
                                                    .on_hover_text("Reset brightness to 0%")
                                                    .clicked()
                                            {
                                                item.brightness = 0;
                                                any_toggled = true;
                                            }
                                        });
                                    });
                                });

                            if (grid_pos + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
            });

        if let Some(idx) = remove_idx {
            self.config.wallpapers.remove(idx);
            any_toggled = true;
        }

        if any_toggled {
            self.maybe_auto_save();
        }

        if let Some(target) = set_now_path {
            if !is_daemon_running() {
                let _ = spawn_daemon_background();
                std::thread::sleep(Duration::from_millis(250));
            }
            let _ = send_ipc_command(&IpcCommand::SetWallpaper(target));
        }
    }
}

impl eframe::App for SlideshowApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_thumbnails(ctx);
        self.handle_dropped_files(ctx);

        if self.last_state_poll.elapsed() >= Duration::from_millis(250) {
            self.daemon_state = DaemonState::load();
            self.last_state_poll = Instant::now();
        }

        egui::TopBottomPanel::top("top_header_panel")
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(18, 22, 32))
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| {
                self.render_top_bar(ui, ctx);
            });

        egui::SidePanel::left("controls_sidebar")
            .resizable(true)
            .default_width(350.0)
            .width_range(310.0..=440.0)
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(20, 24, 34))
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                self.render_sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(15, 18, 26))
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                self.render_gallery(ui, ctx);
            });

        // Keep countdown bar smooth
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

pub fn run_gui() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CachyOS Wallpaper Slideshow")
            .with_inner_size([1160.0, 740.0])
            .with_min_inner_size([820.0, 520.0])
            .with_app_id("cachyos-wallpaper-slideshow"),
        ..Default::default()
    };

    eframe::run_native(
        "CachyOS Wallpaper Slideshow",
        options,
        Box::new(|cc| Ok(Box::new(SlideshowApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}
