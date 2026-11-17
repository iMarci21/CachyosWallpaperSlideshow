use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeUnit {
    Seconds,
    Minutes,
    Hours,
}

impl TimeUnit {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimeUnit::Seconds => "Seconds",
            TimeUnit::Minutes => "Minutes",
            TimeUnit::Hours => "Hours",
        }
    }

    pub fn short_suffix(&self) -> &'static str {
        match self {
            TimeUnit::Seconds => "s",
            TimeUnit::Minutes => "min",
            TimeUnit::Hours => "hr",
        }
    }

    pub fn multiplier_secs(&self) -> f64 {
        match self {
            TimeUnit::Seconds => 1.0,
            TimeUnit::Minutes => 60.0,
            TimeUnit::Hours => 3600.0,
        }
    }

    pub fn slider_range(&self) -> std::ops::RangeInclusive<f64> {
        match self {
            TimeUnit::Seconds => 3.0..=300.0,
            TimeUnit::Minutes => 0.5..=120.0,
            TimeUnit::Hours => 0.25..=24.0,
        }
    }

    pub fn jitter_slider_max(&self, base_value: f64) -> f64 {
        match self {
            TimeUnit::Seconds => base_value.max(10.0),
            TimeUnit::Minutes => base_value.max(5.0),
            TimeUnit::Hours => base_value.max(2.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderMode {
    Shuffle,
    Sequential,
}

impl OrderMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderMode::Shuffle => "Shuffle (Random)",
            OrderMode::Sequential => "Sequential",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScalingMode {
    Zoom,
    Fit,
    Stretch,
}

impl ScalingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScalingMode::Zoom => "Zoom (Fill & Crop)",
            ScalingMode::Fit => "Fit (Letterbox)",
            ScalingMode::Stretch => "Stretch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallpaperItem {
    pub path: PathBuf,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SlideshowConfig {
    pub wallpapers: Vec<WallpaperItem>,
    pub interval_value: f64,
    pub interval_unit: TimeUnit,
    pub randomize_interval: bool,
    pub jitter_value: f64,
    pub order_mode: OrderMode,
    pub fade_duration_secs: f32,
    pub scaling_mode: ScalingMode,
    pub paused: bool,
    pub sync_cosmic_config: bool,
}

impl Default for SlideshowConfig {
    fn default() -> Self {
        let mut wallpapers = Vec::new();
        for path in discover_cosmic_wallpapers() {
            wallpapers.push(WallpaperItem {
                path,
                enabled: true,
            });
        }
        Self {
            wallpapers,
            interval_value: 5.0,
            interval_unit: TimeUnit::Minutes,
            randomize_interval: true,
            jitter_value: 1.0,
            order_mode: OrderMode::Shuffle,
            fade_duration_secs: 1.5,
            scaling_mode: ScalingMode::Zoom,
            paused: false,
            sync_cosmic_config: true,
        }
    }
}

impl SlideshowConfig {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("cachyos-wallpaper-slideshow")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(mut cfg) = serde_json::from_str::<Self>(&data) {
                cfg.sanitize();
                return cfg;
            }
        }
        let default_cfg = Self::default();
        let _ = default_cfg.save();
        default_cfg
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir();
        fs::create_dir_all(&dir).context("Failed to create config directory")?;
        let tmp_path = dir.join("config.json.tmp");
        let data = serde_json::to_string_pretty(self)?;
        fs::write(&tmp_path, data).context("Failed to write temporary config file")?;
        fs::rename(&tmp_path, Self::config_path()).context("Failed to finalize config file")?;
        Ok(())
    }

    pub fn sanitize(&mut self) {
        if self.interval_value <= 0.0 || !self.interval_value.is_finite() {
            self.interval_value = 1.0;
        }
        if self.jitter_value < 0.0 || !self.jitter_value.is_finite() {
            self.jitter_value = 0.0;
        }
        self.fade_duration_secs = self.fade_duration_secs.clamp(0.1, 10.0);
    }

    pub fn base_interval_secs(&self) -> f64 {
        (self.interval_value * self.interval_unit.multiplier_secs()).max(1.0)
    }

    pub fn jitter_secs(&self) -> f64 {
        if self.randomize_interval {
            (self.jitter_value * self.interval_unit.multiplier_secs()).max(0.0)
        } else {
            0.0
        }
    }

    pub fn interval_bounds_secs(&self) -> (f64, f64) {
        let base = self.base_interval_secs();
        let jitter = self.jitter_secs();
        let min_s = (base - jitter).max(1.0);
        let max_s = (base + jitter).max(min_s);
        (min_s, max_s)
    }

    pub fn sample_next_duration(&self) -> Duration {
        let (min_s, max_s) = self.interval_bounds_secs();
        if (max_s - min_s).abs() < f64::EPSILON {
            Duration::from_secs_f64(min_s)
        } else {
            let mut rng = rand::thread_rng();
            let sampled = rng.gen_range(min_s..=max_s);
            Duration::from_secs_f64(sampled)
        }
    }

    pub fn enabled_wallpapers(&self) -> Vec<PathBuf> {
        self.wallpapers
            .iter()
            .filter(|w| w.enabled && w.path.is_file())
            .map(|w| w.path.clone())
            .collect()
    }

    pub fn add_paths<I: IntoIterator<Item = PathBuf>>(&mut self, paths: I) -> usize {
        let existing: HashSet<PathBuf> = self.wallpapers.iter().map(|w| w.path.clone()).collect();
        let mut added = 0;
        for path in paths {
            let canonical = fs::canonicalize(&path).unwrap_or(path);
            if is_supported_image(&canonical) && !existing.contains(&canonical) {
                if !self.wallpapers.iter().any(|w| w.path == canonical) {
                    self.wallpapers.push(WallpaperItem {
                        path: canonical,
                        enabled: true,
                    });
                    added += 1;
                }
            }
        }
        added
    }

    pub fn import_cosmic_wallpapers(&mut self) -> usize {
        let cosmic_paths = discover_cosmic_wallpapers();
        self.add_paths(cosmic_paths)
    }
}

pub fn is_supported_image(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "tif" | "tiff" | "pnm" | "ico"
    )
}

pub fn collect_images_from_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if is_supported_image(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

pub fn discover_cosmic_wallpapers() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Some(config_dir) = dirs::config_dir() else {
        return found;
    };

    // 1. Check com.system76.CosmicSettings.Wallpaper/v1/custom-images
    let custom_images_path = config_dir
        .join("cosmic")
        .join("com.system76.CosmicSettings.Wallpaper")
        .join("v1")
        .join("custom-images");

    if let Ok(content) = fs::read_to_string(&custom_images_path) {
        for line in content.lines() {
            let trimmed = line.trim().trim_matches(',').trim();
            if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() > 2 {
                let p = PathBuf::from(&trimmed[1..trimmed.len() - 1]);
                if is_supported_image(&p) && !found.contains(&p) {
                    found.push(p);
                }
            }
        }
    }

    // 2. Check com.system76.CosmicBackground/v1/all
    let all_bg_path = config_dir
        .join("cosmic")
        .join("com.system76.CosmicBackground")
        .join("v1")
        .join("all");

    if let Ok(content) = fs::read_to_string(&all_bg_path) {
        if let Some(start) = content.find("Path(\"") {
            let rest = &content[start + 6..];
            if let Some(end) = rest.find("\")") {
                let p = PathBuf::from(&rest[..end]);
                if is_supported_image(&p) && !found.contains(&p) {
                    found.push(p);
                }
            }
        }
    }

    found
}

/// Updates COSMIC's background configuration file so COSMIC Settings and lock screen stay synced.
pub fn sync_cosmic_background_file(image_path: &Path, scaling_mode: ScalingMode) {
    let Some(config_dir) = dirs::config_dir() else {
        return;
    };
    let all_bg_path = config_dir
        .join("cosmic")
        .join("com.system76.CosmicBackground")
        .join("v1")
        .join("all");

    if !all_bg_path.parent().map(|p| p.exists()).unwrap_or(false) {
        return;
    }

    let cosmic_scaling = match scaling_mode {
        ScalingMode::Zoom => "Zoom",
        ScalingMode::Fit => "Fit",
        ScalingMode::Stretch => "Stretch",
    };

    let escaped_path = image_path.to_string_lossy().replace('"', "\\\"");
    let ron_content = format!(
        "(\n    output: \"all\",\n    source: Path(\"{}\"),\n    filter_by_theme: true,\n    rotation_frequency: 0,\n    filter_method: Lanczos,\n    scaling_mode: {},\n    sampling_method: Alphanumeric,\n)\n",
        escaped_path, cosmic_scaling
    );

    let _ = fs::write(&all_bg_path, ron_content);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcCommand {
    Next,
    Prev,
    SetWallpaper(PathBuf),
    Reload,
    TogglePause,
    Stop,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub running: bool,
    pub paused: bool,
    pub current_wallpaper: Option<PathBuf>,
    pub next_wallpaper: Option<PathBuf>,
    pub cycle_started_unix_ms: u64,
    pub cycle_duration_ms: u64,
    pub active_rotation_count: usize,
    pub transitioning: bool,
    pub backend_info: String,
    pub updated_unix_ms: u64,
}

impl DaemonState {
    pub fn runtime_dir() -> PathBuf {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    }

    pub fn state_path() -> PathBuf {
        Self::runtime_dir().join("cachyos-wallpaper-slideshow-state.json")
    }

    pub fn socket_path() -> PathBuf {
        Self::runtime_dir().join("cachyos-wallpaper-slideshow.sock")
    }

    pub fn load() -> Option<Self> {
        let data = fs::read_to_string(Self::state_path()).ok()?;
        let state: Self = serde_json::from_str(&data).ok()?;
        // Verify process is alive
        if state.pid > 0 && Path::new(&format!("/proc/{}", state.pid)).exists() {
            Some(state)
        } else {
            None
        }
    }

    pub fn save(&mut self) {
        self.updated_unix_ms = now_unix_ms();
        let path = Self::state_path();
        let tmp = path.with_extension("json.tmp");
        if let Ok(data) = serde_json::to_string(&self) {
            if fs::write(&tmp, data).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
        }
    }

    pub fn remaining_secs(&self) -> f64 {
        if self.paused || self.cycle_duration_ms == 0 {
            return 0.0;
        }
        let now = now_unix_ms();
        let end = self.cycle_started_unix_ms.saturating_add(self.cycle_duration_ms);
        if now >= end {
            0.0
        } else {
            (end - now) as f64 / 1000.0
        }
    }

    pub fn progress_fraction(&self) -> f32 {
        if self.cycle_duration_ms == 0 {
            return 0.0;
        }
        let now = now_unix_ms();
        let elapsed = now.saturating_sub(self.cycle_started_unix_ms);
        (elapsed as f32 / self.cycle_duration_ms as f32).clamp(0.0, 1.0)
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn format_duration_human(secs: f64) -> String {
    let total_secs = secs.round().max(0.0) as u64;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{}h {:02}m {:02}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {:02}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}
