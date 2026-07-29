//! sprite-viewer — borderless floating Wayland window that polls the Gremlin
//! daemon for the current sprite animation frame and renders it at 4x scale.
//! Roams the screen with a lazy random walk — desktop pet behaviour.
//!
//! Linux/Wayland only.  Usage:
//!   ./target/release/sprite-viewer [scale_factor]

#[allow(dead_code)]
const FRAME_SIZE: u32 = 48;
#[allow(dead_code)]
const DEFAULT_SCALE: u32 = 4;

// ── Linux implementation ──

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use rand::Rng;
    use winit::{
        application::ApplicationHandler,
        dpi::{PhysicalPosition, PhysicalSize},
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        platform::wayland::WindowAttributesExtWayland,
        window::{Window, WindowAttributes, WindowId},
    };
    use softbuffer::{Context, Surface};

    use super::FRAME_SIZE;

    /// Connect to the Gremlin daemon's Unix socket.
    /// Retries with backoff for up to ~60s — at login the daemon (systemd)
    /// may still be starting when Hyprland's exec-once launches us.
    pub fn connect_daemon() -> Result<UnixStream, String> {
        let sock = if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            format!("{dir}/gremlin.sock")
        } else {
            let uid = unsafe { libc::getuid() };
            format!("/tmp/gremlin-{uid}.sock")
        };
        let mut delay = std::time::Duration::from_millis(500);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match UnixStream::connect(&sock) {
                Ok(s) => return Ok(s),
                Err(e) if Instant::now() < deadline => {
                    eprintln!("sprite-viewer: waiting for daemon at {sock} ({e}), retrying...");
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_secs(5));
                }
                Err(e) => return Err(format!("cannot connect to {sock} after 60s: {e}")),
            }
        }
    }

    /// Call `sprite_current_frame` directly on the daemon (bypasses LLM).
    pub fn poll_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
        let req = serde_json::json!({"tool":"sprite_current_frame"});
        let mut req_bytes = serde_json::to_vec(&req).ok()?;
        req_bytes.push(b'\n');
        stream.write_all(&req_bytes).ok()?;

        let mut buf = [0u8; 16384];
        let n = stream.read(&mut buf).ok()?;
        let resp: serde_json::Value = serde_json::from_slice(&buf[..n]).ok()?;
        let body = resp["response"].as_str()?;

        let b64 = body.strip_prefix("data:image/png;base64,")?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()
    }

    /// Decode a raw PNG, return RGBA pixels + dimensions.
    pub fn decode_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let img = image::load_from_memory(data).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        Some((img.into_raw(), w, h))
    }

    /// Nearest-neighbour upscale.
    pub fn scale_nearest(src: &[u8], sw: u32, sh: u32, factor: u32) -> Vec<u8> {
        let dw = sw * factor;
        let dh = sh * factor;
        let mut dst = vec![0u8; (dw * dh * 4) as usize];
        for y in 0..dh {
            for x in 0..dw {
                let sx = x / factor;
                let sy = y / factor;
                let si = ((sy * sw + sx) * 4) as usize;
                let di = ((y * dw + x) * 4) as usize;
                dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }
        dst
    }

    pub struct App {
        window: Option<Window>,
        scale: u32,
        daemon_stream: Option<UnixStream>,
        last_frame: Vec<u8>,
        last_poll: Instant,
        display_size: u32,
        // ── roaming state ──
        pos_x: f64,
        pos_y: f64,
        vel_x: f64,
        vel_y: f64,
        last_dir_change: Instant,
        screen_w: u32,
        screen_h: u32,
        rng: rand::rngs::ThreadRng,
    }

    impl App {
        pub fn new(scale: u32) -> Self {
            Self {
                window: None,
                scale,
                daemon_stream: None,
                last_frame: Vec::new(),
                last_poll: Instant::now(),
                display_size: FRAME_SIZE * scale,
                pos_x: 0.0,
                pos_y: 0.0,
                vel_x: 0.0,
                vel_y: 0.0,
                last_dir_change: Instant::now(),
                screen_w: 1920,
                screen_h: 1080,
                rng: rand::thread_rng(),
            }
        }

        pub fn set_daemon_stream(&mut self, stream: UnixStream) {
            self.daemon_stream = Some(stream);
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }

            let sz = self.display_size;
            let attrs = WindowAttributes::default()
                .with_inner_size(PhysicalSize::new(sz, sz))
                .with_title("gremlin-sprite")
                .with_name("gremlin-sprite", "gremlin-sprite")
                .with_visible(true);

            let window = event_loop
                .create_window(attrs)
                .expect("failed to create Wayland window");

            // Sniff monitor geometry for edge-bouncing
            if let Some(monitor) = window.current_monitor() {
                let s = monitor.size();
                self.screen_w = s.width.max(sz + 64);
                self.screen_h = s.height.max(sz + 64);
                // Start bottom-right
                self.pos_x = (self.screen_w.saturating_sub(sz + 32)) as f64;
                self.pos_y = (self.screen_h.saturating_sub(sz + 32)) as f64;
            } else {
                // Fallback: position at bottom-right of a reasonable default
                self.pos_x = (self.screen_w.saturating_sub(sz + 32)) as f64;
                self.pos_y = (self.screen_h.saturating_sub(sz + 32)) as f64;
            }
            // Pick initial drift
            self.randomize_velocity();

            eprintln!(
                "sprite-viewer: window {}×{} on {}×{}, roaming from ({:.0},{:.0})",
                sz, sz, self.screen_w, self.screen_h, self.pos_x, self.pos_y
            );
            let _ = window.set_outer_position(PhysicalPosition::new(self.pos_x, self.pos_y));
            window.request_redraw();
            self.window = Some(window);
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _id: WindowId,
            event: WindowEvent,
        ) {
            match event {
                WindowEvent::CloseRequested => {
                    event_loop.exit();
                }
                WindowEvent::RedrawRequested => {
                    let Some(window) = self.window.as_ref() else { return };

                    let now = Instant::now();

                    // ── Poll daemon ~60 Hz ──
                    if now.duration_since(self.last_poll) >= Duration::from_millis(16) {
                        self.last_poll = now;
                        if let Some(stream) = self.daemon_stream.as_mut() {
                            if let Some(data) = poll_frame(stream) {
                                if let Some((rgba, w, h)) = decode_png(&data) {
                                    self.last_frame = scale_nearest(&rgba, w, h, self.scale);
                                }
                            }
                        }
                    }

                    // ── Roam: update position ──
                    self.update_roam(now);

                    // ── Render ──
                    let render_buf: &[u8] = if !self.last_frame.is_empty() {
                        &self.last_frame
                    } else {
                        // Placeholder until first frame arrives: dark green square
                        // so the window isn't transparent/invisible on startup.
                        use std::sync::LazyLock;
                        static PLACEHOLDER: LazyLock<Vec<u8>> = LazyLock::new(|| {
                                let sz = (FRAME_SIZE * 4) as usize;
                                let mut buf = vec![0u8; sz * sz * 4];
                                for y in 0..sz {
                                    for x in 0..sz {
                                        let i = (y * sz + x) * 4;
                                        // Dark CRT green with slight center glow
                                        let dx = (x as f64 - sz as f64 / 2.0).abs();
                                        let dy = (y as f64 - sz as f64 / 2.0).abs();
                                        let dist = (dx * dx + dy * dy).sqrt() / (sz as f64 / 2.0);
                                        let g = ((1.0 - dist) * 40.0).clamp(0.0, 40.0) as u8;
                                        buf[i] = 0;
                                        buf[i + 1] = g;
                                        buf[i + 2] = 0;
                                        buf[i + 3] = 255;
                                    }
                                }
                                buf
                            });
                        &*PLACEHOLDER
                    };

                    if let Ok(ctx) = Context::new(window) {
                        if let Ok(mut surface) = Surface::new(&ctx, window) {
                            if let Ok(mut buffer) = surface.buffer_mut() {
                                let dst = buffer.as_mut();
                                for (i, chunk) in render_buf.chunks_exact(4).enumerate() {
                                    if i < dst.len() {
                                        let r = chunk[0] as u32;
                                        let g = chunk[1] as u32;
                                        let b = chunk[2] as u32;
                                        let a = chunk[3] as u32;
                                        dst[i] = (a << 24) | (r << 16) | (g << 8) | b;
                                    }
                                }
                                let _ = buffer.present();
                            }
                        }
                    }

                    window.request_redraw();
                }
                _ => {}
            }
        }
    }

    impl App {
        /// Advance the roaming position one tick, bouncing off screen edges.
        fn update_roam(&mut self, now: Instant) {
            let dt = 0.016; // ~60fps tick
            let sz = self.display_size as f64;
            let margin = 16.0;

            // Change direction every 2-5 seconds
            if now.duration_since(self.last_dir_change) > Duration::from_secs_f64(self.rng.random_range(2.0..5.0)) {
                self.randomize_velocity();
                self.last_dir_change = now;
            }

            // Apply velocity
            self.pos_x += self.vel_x * dt;
            self.pos_y += self.vel_y * dt;

            // Bounce off edges — clamp to [margin, screen - sz - margin]
            let max_x = ((self.screen_w as f64) - sz - margin).max(margin);
            let max_y = ((self.screen_h as f64) - sz - margin).max(margin);
            let min_x = margin;
            let min_y = margin;

            if self.pos_x < min_x { self.pos_x = min_x; self.vel_x = self.vel_x.abs(); }
            if self.pos_x > max_x { self.pos_x = max_x; self.vel_x = -self.vel_x.abs(); }
            if self.pos_y < min_y { self.pos_y = min_y; self.vel_y = self.vel_y.abs(); }
            if self.pos_y > max_y { self.pos_y = max_y; self.vel_y = -self.vel_y.abs(); }

            if let Some(ref window) = self.window {
                let _ = window.set_outer_position(PhysicalPosition::new(self.pos_x, self.pos_y));
            }
        }

        fn randomize_velocity(&mut self) {
            // Lazy drift: 40-120 px/sec in a random direction
            let speed: f64 = self.rng.random_range(40.0..120.0);
            let angle: f64 = self.rng.random_range(0.0..std::f64::consts::TAU);
            self.vel_x = angle.cos() * speed;
            self.vel_y = angle.sin() * speed;
        }
    }

    /// Run the event loop (called from main).
    pub fn run(stream: UnixStream, scale: u32) {
        let event_loop = EventLoop::new().expect("failed to create event loop");
        event_loop.set_control_flow(ControlFlow::Poll);

        let mut app = App::new(scale);
        app.set_daemon_stream(stream);
        let _ = event_loop.run_app(&mut app);
    }
}

// ── Entry point ──

fn main() {
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "sprite-viewer: Wayland/Linux only. \
             This binary renders a floating sprite window for Gremlin."
        );
        std::process::exit(1);
    }

    #[cfg(target_os = "linux")]
    {
        let scale: u32 = std::env::args()
            .nth(1)
            .and_then(|a| a.parse().ok())
            .unwrap_or(DEFAULT_SCALE);

        let stream = linux_impl::connect_daemon().expect(
            "Gremlin daemon not running? Start it first: gremlin daemon\n\
             Socket is at $XDG_RUNTIME_DIR/gremlin.sock",
        );

        linux_impl::run(stream, scale);
    }
}