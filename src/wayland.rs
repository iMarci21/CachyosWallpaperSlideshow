use crate::config::{
    cosmic_bg_state_wallpapers_path, create_brightness_lut, sync_cosmic_background_file,
    wait_for_cosmic_bg_commit, ScalingMode,
};
use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView, RgbaImage};
use rayon::prelude::*;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};

/// Pre-decoded RGBA image in memory so multi-output scaling is fast.
#[derive(Clone)]
pub struct DecodedWallpaper {
    pub image: Arc<DynamicImage>,
}

impl DecodedWallpaper {
    pub fn load(path: &Path) -> Result<Self> {
        let img = image::open(path)
            .with_context(|| format!("Failed to decode wallpaper image: {}", path.display()))?;
        Ok(Self {
            image: Arc::new(img),
        })
    }

    /// Scales the image into a Wayland `Xrgb8888` (little-endian `[B, G, R, 255]`) byte buffer
    /// of exact size `width * height * 4`, with the specified brightness LUT applied.
    pub fn scale_to_xrgb8888(
        &self,
        target_w: u32,
        target_h: u32,
        mode: ScalingMode,
        brightness: i32,
    ) -> Vec<u8> {
        let target_w = target_w.max(1);
        let target_h = target_h.max(1);
        let (src_w, src_h) = self.image.dimensions();
        if src_w == 0 || src_h == 0 {
            return vec![0u8; (target_w * target_h * 4) as usize];
        }

        let rgba: RgbaImage = match mode {
            ScalingMode::Stretch => self
                .image
                .resize_exact(target_w, target_h, image::imageops::FilterType::Triangle)
                .to_rgba8(),
            ScalingMode::Zoom => {
                let scale_x = target_w as f64 / src_w as f64;
                let scale_y = target_h as f64 / src_h as f64;
                let scale = scale_x.max(scale_y);
                let scaled_w = ((src_w as f64 * scale).round() as u32).max(target_w);
                let scaled_h = ((src_h as f64 * scale).round() as u32).max(target_h);
                let resized = self.image.resize_exact(
                    scaled_w,
                    scaled_h,
                    image::imageops::FilterType::Triangle,
                );
                let crop_x = (scaled_w.saturating_sub(target_w)) / 2;
                let crop_y = (scaled_h.saturating_sub(target_h)) / 2;
                resized
                    .crop_imm(crop_x, crop_y, target_w, target_h)
                    .to_rgba8()
            }
            ScalingMode::Fit => {
                let scale_x = target_w as f64 / src_w as f64;
                let scale_y = target_h as f64 / src_h as f64;
                let scale = scale_x.min(scale_y);
                let scaled_w = ((src_w as f64 * scale).round() as u32).clamp(1, target_w);
                let scaled_h = ((src_h as f64 * scale).round() as u32).clamp(1, target_h);
                let resized = self
                    .image
                    .resize_exact(scaled_w, scaled_h, image::imageops::FilterType::Triangle)
                    .to_rgba8();
                let mut canvas =
                    RgbaImage::from_pixel(target_w, target_h, image::Rgba([14, 16, 22, 255]));
                let offset_x = ((target_w - scaled_w) / 2) as i64;
                let offset_y = ((target_h - scaled_h) / 2) as i64;
                image::imageops::overlay(&mut canvas, &resized, offset_x, offset_y);
                canvas
            }
        };

        let lut = create_brightness_lut(brightness);
        let raw = rgba.into_raw();
        let mut xrgb = vec![0u8; raw.len()];
        xrgb.par_chunks_exact_mut(4)
            .zip(raw.par_chunks_exact(4))
            .for_each(|(dst, src)| {
                // Wayland WL_SHM_FORMAT_XRGB8888 on little-endian is [B, G, R, X]
                dst[0] = lut[src[2] as usize];
                dst[1] = lut[src[1] as usize];
                dst[2] = lut[src[0] as usize];
                dst[3] = 255;
            });
        xrgb
    }
}

/// Fast parallel SIMD-friendly fixed-point crossfade between two `Xrgb8888` buffers.
pub fn blend_xrgb8888(from: &[u8], to: &[u8], dst: &mut [u8], progress: f32) {
    let t = progress.clamp(0.0, 1.0);
    // Cubic smoothstep easing: 3t^2 - 2t^3 for a silky visual fade
    let eased = t * t * (3.0 - 2.0 * t);
    let alpha = (eased * 256.0).round() as u32;
    let inv_alpha = 256u32.saturating_sub(alpha);

    if alpha == 0 {
        dst.copy_from_slice(from);
        return;
    }
    if alpha >= 256 {
        dst.copy_from_slice(to);
        return;
    }

    dst.par_chunks_exact_mut(4)
        .zip(from.par_chunks_exact(4).zip(to.par_chunks_exact(4)))
        .for_each(|(d, (f, t_px))| {
            d[0] = (((f[0] as u32 * inv_alpha) + (t_px[0] as u32 * alpha)) >> 8) as u8;
            d[1] = (((f[1] as u32 * inv_alpha) + (t_px[1] as u32 * alpha)) >> 8) as u8;
            d[2] = (((f[2] as u32 * inv_alpha) + (t_px[2] as u32 * alpha)) >> 8) as u8;
            d[3] = 255;
        });
}

struct OutputSurface {
    output: wl_output::WlOutput,
    layer_surface: LayerSurface,
    pool: SlotPool,
    width: u32,
    height: u32,
    configured: bool,
    from_buffer: Vec<u8>,
    to_buffer: Vec<u8>,
}

impl OutputSurface {
    fn prepare_buffers(
        &mut self,
        from_wp: &DecodedWallpaper,
        from_brightness: i32,
        to_wp: &DecodedWallpaper,
        to_brightness: i32,
        mode: ScalingMode,
    ) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        self.from_buffer = from_wp.scale_to_xrgb8888(self.width, self.height, mode, from_brightness);
        self.to_buffer = to_wp.scale_to_xrgb8888(self.width, self.height, mode, to_brightness);
    }

    fn draw_frame(&mut self, blend_progress: f32) -> Result<()> {
        if !self.configured || self.width == 0 || self.height == 0 {
            return Ok(());
        }
        let stride = self.width as i32 * 4;
        let byte_len = (self.width * self.height * 4) as usize;
        if self.to_buffer.len() != byte_len {
            return Ok(());
        }

        let (buffer, canvas) = self
            .pool
            .create_buffer(
                self.width as i32,
                self.height as i32,
                stride,
                wl_shm::Format::Xrgb8888,
            )
            .context("Failed to allocate SHM buffer")?;

        if self.from_buffer.len() == byte_len {
            blend_xrgb8888(
                &self.from_buffer,
                &self.to_buffer,
                &mut canvas[..byte_len],
                blend_progress,
            );
        } else {
            canvas[..byte_len].copy_from_slice(&self.to_buffer);
        }

        let wl_surf = self.layer_surface.wl_surface();
        buffer
            .attach_to(wl_surf)
            .context("Failed to attach SHM buffer to layer surface")?;
        wl_surf.damage_buffer(0, 0, self.width as i32, self.height as i32);
        wl_surf.commit();
        Ok(())
    }
}

pub struct WaylandLayerState {
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    shm_state: Shm,
    layer_shell: LayerShell,
    surfaces: HashMap<u32, OutputSurface>,
    from_wallpaper: Option<DecodedWallpaper>,
    from_brightness: i32,
    to_wallpaper: Option<DecodedWallpaper>,
    to_brightness: i32,
    scaling_mode: ScalingMode,
}

impl WaylandLayerState {
    fn create_surface_for_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        let id = info.id;
        if self.surfaces.contains_key(&id) {
            return;
        }

        let surface = self.compositor_state.create_surface(qh);
        // Empty input region so mouse clicks/scrolls pass straight through
        if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor_state)
        {
            surface.set_input_region(Some(region.wl_region()));
        }

        let layer_surface = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Bottom,
            Some("cachyos-wallpaper-crossfade"),
            Some(&output),
        );
        layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.set_size(0, 0);
        layer_surface.commit();

        let Ok(pool) = SlotPool::new(1920 * 1080 * 4, &self.shm_state) else {
            return;
        };

        self.surfaces.insert(
            id,
            OutputSurface {
                output,
                layer_surface,
                pool,
                width: 0,
                height: 0,
                configured: false,
                from_buffer: Vec::new(),
                to_buffer: Vec::new(),
            },
        );
    }

    fn spawn_all_output_surfaces(&mut self, qh: &QueueHandle<Self>) {
        let outputs: Vec<_> = self.output_state.outputs().collect();
        for output in outputs {
            self.create_surface_for_output(qh, output);
        }
    }

    fn destroy_all_output_surfaces(&mut self) {
        self.surfaces.clear();
    }
}

impl CompositorHandler for WaylandLayerState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandLayerState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.retain(|_, s| s.output != output);
    }
}

impl LayerShellHandler for WaylandLayerState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        self.surfaces.retain(|_, s| &s.layer_surface != layer);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let new_w = configure.new_size.0.max(1);
        let new_h = configure.new_size.1.max(1);
        let mode = self.scaling_mode;
        let from_wp = self.from_wallpaper.as_ref();
        let to_wp = self.to_wallpaper.as_ref();

        for surf in self.surfaces.values_mut() {
            if &surf.layer_surface == layer {
                surf.width = new_w;
                surf.height = new_h;
                surf.configured = true;
                if let (Some(f_wp), Some(t_wp)) = (from_wp, to_wp) {
                    surf.prepare_buffers(f_wp, self.from_brightness, t_wp, self.to_brightness, mode);
                    let _ = surf.draw_frame(0.0);
                }
                break;
            }
        }
    }
}

impl ShmHandler for WaylandLayerState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl ProvidesRegistryState for WaylandLayerState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(WaylandLayerState);
delegate_output!(WaylandLayerState);
delegate_shm!(WaylandLayerState);
delegate_layer!(WaylandLayerState);
delegate_registry!(WaylandLayerState);

pub struct WaylandRenderer {
    conn: Connection,
    event_queue: EventQueue<WaylandLayerState>,
    state: WaylandLayerState,
}

impl WaylandRenderer {
    pub fn new(
        initial_scaling: ScalingMode,
        initial_wallpaper: Option<&Path>,
        initial_brightness: i32,
    ) -> Result<Self> {
        let conn = Connection::connect_to_env().context("Failed to connect to Wayland display")?;
        let (globals, mut event_queue) =
            registry_queue_init(&conn).context("Failed to initialize Wayland registry")?;
        let qh = event_queue.handle();

        let compositor_state =
            CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
        let shm_state = Shm::bind(&globals, &qh).context("wl_shm not available")?;
        let layer_shell = LayerShell::bind(&globals, &qh)
            .context("zwlr_layer_shell_v1 not supported by compositor")?;
        let output_state = OutputState::new(&globals, &qh);
        let registry_state = RegistryState::new(&globals);

        let from_wallpaper = initial_wallpaper.and_then(|p| DecodedWallpaper::load(p).ok());

        let mut state = WaylandLayerState {
            registry_state,
            compositor_state,
            output_state,
            shm_state,
            layer_shell,
            surfaces: HashMap::new(),
            from_wallpaper,
            from_brightness: initial_brightness,
            to_wallpaper: None,
            to_brightness: 0,
            scaling_mode: initial_scaling,
        };

        // Roundtrip only to discover outputs; do NOT create persistent surfaces while idle!
        event_queue.roundtrip(&mut state)?;

        Ok(Self {
            conn,
            event_queue,
            state,
        })
    }

    pub fn dispatch_pending(&mut self) -> Result<()> {
        self.event_queue.dispatch_pending(&mut self.state)?;
        self.conn.flush()?;
        if let Some(guard) = self.conn.prepare_read() {
            let _ = guard.read();
        }
        self.event_queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    /// Performs a smooth 60 FPS crossfade on `Layer::Bottom` (strictly above `cosmic-bg`'s
    /// `Layer::Background` and strictly below all windows), triggers `cosmic-bg` underneath
    /// at the start of the fade so `cosmic-bg` can never jump in front of our animation,
    /// holds the 100% frame until `cosmic-bg` finishes committing, and then unmaps cleanly.
    pub fn transition_to(
        &mut self,
        target_path: &Path,
        wallpaper: DecodedWallpaper,
        brightness: i32,
        scaling_mode: ScalingMode,
        fade_duration_secs: f32,
        sync_cosmic: bool,
    ) -> Result<()> {
        self.dispatch_pending()?;
        self.state.scaling_mode = scaling_mode;

        let Some(prev_wp) = self.state.from_wallpaper.clone() else {
            // No previous image to crossfade from; hand directly to cosmic-bg
            if sync_cosmic {
                sync_cosmic_background_file(target_path, scaling_mode);
            }
            self.state.from_wallpaper = Some(wallpaper);
            self.state.from_brightness = brightness;
            return Ok(());
        };

        if fade_duration_secs <= 0.05 {
            if sync_cosmic {
                sync_cosmic_background_file(target_path, scaling_mode);
            }
            self.state.from_wallpaper = Some(wallpaper);
            self.state.from_brightness = brightness;
            return Ok(());
        }

        // 1. Create temporary `Layer::Bottom` surfaces and cover `Layer::Background` with frame 0.0
        self.state.from_wallpaper = Some(prev_wp);
        self.state.to_wallpaper = Some(wallpaper.clone());
        self.state.to_brightness = brightness;

        let qh = self.event_queue.handle();
        self.state.spawn_all_output_surfaces(&qh);
        // First roundtrip receives LayerSurface::configure and commits frame 0.0
        self.event_queue.roundtrip(&mut self.state)?;
        // Second roundtrip ensures cosmic-comp has latched our opaque frame 0.0 buffer on Layer::Bottom
        self.event_queue.roundtrip(&mut self.state)?;

        // 2. Record `cosmic-bg` state file mtime and trigger `cosmic-bg` reload underneath our
        //    `Layer::Bottom` surface. Because `Layer::Bottom` is strictly above `Layer::Background`,
        //    `cosmic-bg` will never appear on top of our crossfade even if it finishes in 0.7s!
        let prev_state_mtime = cosmic_bg_state_wallpapers_path()
            .and_then(|p| fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());

        if sync_cosmic {
            sync_cosmic_background_file(target_path, scaling_mode);
        }

        // 3. Animate smoothstep crossfade at 60 FPS for the full requested duration (e.g. 2.5s)
        let duration = Duration::from_secs_f32(fade_duration_secs.clamp(0.5, 10.0));
        let frame_step = Duration::from_micros(16_667); // ~60 FPS
        let start = Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= duration {
                break;
            }
            let progress = (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0);
            for surf in self.state.surfaces.values_mut() {
                let _ = surf.draw_frame(progress);
            }
            self.conn.flush()?;
            self.event_queue.dispatch_pending(&mut self.state)?;

            let frame_elapsed = start.elapsed().saturating_sub(elapsed);
            if frame_step > frame_elapsed {
                std::thread::sleep(frame_step - frame_elapsed);
            }
        }

        // 4. Commit 100% final frame on our `Layer::Bottom` overlay
        for surf in self.state.surfaces.values_mut() {
            let _ = surf.draw_frame(1.0);
        }
        self.conn.flush()?;
        self.event_queue.roundtrip(&mut self.state)?;

        // 5. If the user chose a very short crossfade (e.g. < 0.5s), `cosmic-bg` might still be
        //    decoding underneath on `Layer::Background`. Hold our 100% `Layer::Bottom` frame
        //    until `cosmic-bg` has finished updating its state file and settled!
        if sync_cosmic {
            wait_for_cosmic_bg_commit(target_path, prev_state_mtime, Duration::from_millis(3000));
        }

        // 6. Destroy our temporary `Layer::Bottom` surfaces so `cosmic-bg` owns the static background alone
        self.state.destroy_all_output_surfaces();
        self.state.to_wallpaper = None;
        self.state.from_wallpaper = Some(wallpaper);
        self.state.from_brightness = brightness;
        self.conn.flush()?;
        self.event_queue.roundtrip(&mut self.state)?;

        Ok(())
    }
}
