//! sprite-viewer — borderless floating Wayland window that polls the Gremlin
//! daemon for the current sprite animation frame and renders it at 4x scale.
//!
//! Linux/Wayland only.  Usage:
//!   ./target/release/sprite-viewer [scale_factor]

const FRAME_SIZE: u32 = 48;
const DEFAULT_SCALE: u32 = 4;

// ── Linux implementation ──

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use winit::{
        application::ApplicationHandler,
        dpi::PhysicalSize,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        platform::wayland::WindowAttributesExtWayland,
        window::{Window, WindowAttributes, WindowId},
    };
    use softbuffer::{Context, Surface};

    use super::FRAME_SIZE;

    /// Connect to the Gremlin daemon's Unix socket.
    pub fn connect_daemon() -> Result<UnixStream, String> {
        let sock = if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            format!("{dir}/gremlin.sock")
        } else {
            let uid = unsafe { libc::getuid() };
            format!("/tmp/gremlin-{uid}.sock")
        };
        UnixStream::connect(&sock).map_err(|e| format!("cannot connect to {sock}: {e}"))
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
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            .ok()
            .into()
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
            eprintln!("sprite-viewer: window created {}×{}", sz, sz);
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

                    // Poll daemon ~60 Hz
                    let now = Instant::now();
                    if now.duration_since(self.last_poll) >= Duration::from_millis(16) {
                        self.last_poll = now;
                        if let Some(data) = poll_frame(self.daemon_stream.as_mut().unwrap()) {
                            if let Some((rgba, w, h)) = decode_png(&data) {
                                self.last_frame = scale_nearest(&rgba, w, h, self.scale);
                                static mut FIRST: bool = true;
                                unsafe {
                                    if FIRST {
                                        eprintln!("sprite-viewer: first frame received ({}×{})", w, h);
                                        FIRST = false;
                                    }
                                }
                            }
                        }
                    }

                    // Render
                    if !self.last_frame.is_empty() {
                        if let Ok(ctx) = Context::new(window) {
                            if let Ok(mut surface) = Surface::new(&ctx, window) {
                                if let Ok(mut buffer) = surface.buffer_mut() {
                                    let dst = buffer.as_mut();
                                    for (i, chunk) in self.last_frame.chunks_exact(4).enumerate() {
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
                    }

                    // Chain next redraw to keep the loop alive
                    window.request_redraw();
                }
                _ => {}
            }
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
