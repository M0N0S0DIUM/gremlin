//! sprite-viewer — borderless floating Wayland window that polls the Gremlin
//! daemon for the current sprite animation frame and renders it at 4x scale.
//! Roams the screen with a lazy random walk — desktop pet behaviour.
//! Left-click opens a zenity/rofi/wofi dialog to ask Gremlin a question.
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
    use std::process::Command;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use rand::Rng;
    use winit::{
        application::ApplicationHandler,
        dpi::{PhysicalPosition, PhysicalSize},
        event::{MouseButton, WindowEvent},
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        platform::wayland::WindowAttributesExtWayland,
        window::{Window, WindowAttributes, WindowId},
    };
    use softbuffer::{Context, Surface};

    use super::FRAME_SIZE;

    // ── Socket connection with retry ──

    /// Try to connect to the daemon socket once.
    fn try_connect_once() -> Result<UnixStream, String> {
        let sock = socket_path();
        UnixStream::connect(&sock).map_err(|e| format!("{sock}: {e}"))
    }

    fn socket_path() -> String {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            format!("{dir}/gremlin.sock")
        } else {
            let uid = unsafe { libc::getuid() };
            format!("/tmp/gremlin-{uid}.sock")
        }
    }

    /// Connect to the Gremlin daemon's Unix socket.
    /// Retries with backoff for up to ~60s — at login the daemon (systemd)
    /// may still be starting when Hyprland's exec-once launches us.
    pub fn connect_daemon() -> Result<UnixStream, String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut delay = Duration::from_millis(500);
        loop {
            match try_connect_once() {
                Ok(s) => return Ok(s),
                Err(e) if Instant::now() < deadline => {
                    eprintln!("sprite-viewer: waiting for daemon ({e}), retrying...");
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
                Err(e) => return Err(format!("cannot connect to daemon after 60s: {e}")),
            }
        }
    }

    /// Connect without retry — used for reconnection after a poll failure.
    fn connect_daemon_once() -> Result<UnixStream, String> {
        try_connect_once()
    }

    // ── Daemon protocol ──

    /// Call `sprite_current_frame` directly on the daemon (bypasses LLM).
    /// Reads the full response by looping until a newline is seen or the
    /// buffer is exhausted, instead of trusting a single `read()` call to
    /// return the whole frame — a base64+JSON-wrapped 192×192 PNG can exceed
    /// a single fixed-size read, especially over a Unix socket where the
    /// kernel is free to hand back partial writes in arbitrarily small chunks.
    pub fn poll_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
        let req = serde_json::json!({"tool":"sprite_current_frame"});
        let mut req_bytes = serde_json::to_vec(&req).ok()?;
        req_bytes.push(b'\n');
        stream.write_all(&req_bytes).ok()?;

        let mut buf = Vec::with_capacity(64 * 1024);
        let mut chunk = [0u8; 16384];
        loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                // Peer closed the connection mid-response.
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') {
                break;
            }
            // Sanity cap: a single sprite frame response should never approach
            // this size; bail rather than buffer unboundedly on a corrupt stream.
            if buf.len() > 8 * 1024 * 1024 {
                return None;
            }
        }

        let line_end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
        let resp: serde_json::Value = serde_json::from_slice(&buf[..line_end]).ok()?;
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

    // ── Dialog subsystem ──

    /// Available dialog backends, in priority order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum DialogBackend {
        Zenity,
        Rofi,
        Wofi,
    }

    impl DialogBackend {
        /// Detect the first available backend on the system.
        fn detect() -> Option<Self> {
            if command_exists("zenity") {
                return Some(Self::Zenity);
            }
            if command_exists("rofi") {
                return Some(Self::Rofi);
            }
            if command_exists("wofi") {
                return Some(Self::Wofi);
            }
            None
        }

        /// Show an entry dialog, return the user's input (trimmed).
        fn entry(&self, title: &str, prompt: &str) -> Option<String> {
            let args: Vec<&str> = match self {
                Self::Zenity => vec!["--entry", "--title", title, "--text", prompt, "--width=400"],
                Self::Rofi => vec!["-dmenu", "-p", prompt, "-theme-str", "window {width: 400;}"],
                Self::Wofi => vec!["--dmenu", "-p", prompt, "--width", "400"],
            };
            let output = Command::new(self.cmd()).args(args).output().ok()?;
            if !output.status.success() {
                return None;
            }
            String::from_utf8(output.stdout).ok().map(|s| s.trim().to_string())
        }

        /// Show an info dialog with the response.
        fn info(&self, title: &str, text: &str) -> bool {
            let args: Vec<&str> = match self {
                Self::Zenity => vec!["--info", "--title", title, "--text", text, "--width=500"],
                Self::Rofi => vec!["-e", "-no-fixed-num-lines", "-theme-str", "window {width: 500;}"],
                Self::Wofi => vec!["--show", "dmenu", "-p", title],
            };
            let mut child = match Command::new(self.cmd())
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(_) => return false,
            };
            if let Some(stdin) = child.stdin.as_mut() {
                use std::io::Write;
                if stdin.write_all(text.as_bytes()).is_err() {
                    return false;
                }
            }
            child.wait().map(|s| s.success()).unwrap_or(false)
        }

        fn cmd(&self) -> &'static str {
            match self {
                Self::Zenity => "zenity",
                Self::Rofi => "rofi",
                Self::Wofi => "wofi",
            }
        }
    }

    fn command_exists(cmd: &str) -> bool {
        Command::new("which").arg(cmd).output().map(|o| o.status.success()).unwrap_or(false)
    }

    /// Send a message to the daemon and return the response text.
    fn ask_daemon(daemon_stream: &mut UnixStream, question: &str) -> Option<String> {
        let req = serde_json::json!({"message": question});
        let mut req_bytes = serde_json::to_vec(&req).ok()?;
        req_bytes.push(b'\n');
        daemon_stream.write_all(&req_bytes).ok()?;

        let mut buf = [0u8; 65536];
        let n = daemon_stream.read(&mut buf).ok()?;
        let resp: serde_json::Value = serde_json::from_slice(&buf[..n]).ok()?;
        resp["response"].as_str().map(|s| s.to_string())
    }

    // ── Main application ──

    pub struct App {
        window: Option<Rc<Window>>,
        surface: Option<Surface<Rc<Window>, Rc<Window>>>,
        scale: u32,
        daemon_stream: Arc<Mutex<Option<UnixStream>>>,
        last_frame: Vec<u8>,
        last_poll: Instant,
        last_reconnect_attempt: Instant,
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
        // ── dialog state ──
        dialog_backend: Option<DialogBackend>,
        dialog_active: Arc<Mutex<bool>>, // prevents re-entrant dialogs
    }

    impl App {
        pub fn new(scale: u32) -> Self {
            let dialog_backend = DialogBackend::detect();
            if dialog_backend.is_none() {
                eprintln!("sprite-viewer: no dialog backend found (zenity/rofi/wofi not in PATH); click-to-ask disabled");
            }
            Self {
                window: None,
                surface: None,
                scale,
                daemon_stream: Arc::new(Mutex::new(None)),
                last_frame: Vec::new(),
                last_poll: Instant::now(),
                last_reconnect_attempt: Instant::now(),
                display_size: FRAME_SIZE * scale,
                pos_x: 0.0,
                pos_y: 0.0,
                vel_x: 0.0,
                vel_y: 0.0,
                last_dir_change: Instant::now(),
                screen_w: 1920,
                screen_h: 1080,
                rng: rand::thread_rng(),
                dialog_backend: DialogBackend::detect(),
                dialog_active: Arc::new(Mutex::new(false)),
            }
        }

        pub fn set_daemon_stream(&mut self, stream: UnixStream) {
            *self.daemon_stream.lock().unwrap() = Some(stream);
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

            let window = Rc::new(
                event_loop
                    .create_window(attrs)
                    .expect("failed to create Wayland window"),
            );

            // Create the softbuffer Context+Surface once here, not per frame.
            // softbuffer 0.4's Context/Surface are generic over the window handle type;
            // with an owned Rc<Window> the types become nameable and storable.
            match Context::new(window.clone()) {
                Ok(ctx) => match Surface::new(&ctx, window.clone()) {
                    Ok(surface) => self.surface = Some(surface),
                    Err(e) => eprintln!("sprite-viewer: failed to create surface: {e}"),
                },
                Err(e) => eprintln!("sprite-viewer: failed to create softbuffer context: {e}"),
            }

            // Sniff monitor geometry for edge-bouncing
            if let Some(monitor) = window.current_monitor() {
                let s = monitor.size();
                self.screen_w = s.width.max(sz + 64);
                self.screen_h = s.height.max(sz + 64);
                self.pos_x = (self.screen_w.saturating_sub(sz + 32)) as f64;
                self.pos_y = (self.screen_h.saturating_sub(sz + 32)) as f64;
            } else {
                self.pos_x = (self.screen_w.saturating_sub(sz + 32)) as f64;
                self.pos_y = (self.screen_h.saturating_sub(sz + 32)) as f64;
            }
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
                    if self.window.is_none() {
                        return;
                    }

                    let now = Instant::now();

                    // ── Poll daemon ~60 Hz, with reconnect-on-failure ──
                    if now.duration_since(self.last_poll) >= Duration::from_millis(16) {
                        self.last_poll = now;
                        let mut poll_failed = false;
                        if let Ok(mut guard) = self.daemon_stream.try_lock() {
                            if let Some(stream) = guard.as_mut() {
                                match poll_frame(stream) {
                                    Some(data) => {
                                        if let Some((rgba, w, h)) = decode_png(&data) {
                                            self.last_frame = scale_nearest(&rgba, w, h, self.scale);
                                        }
                                    }
                                    None => poll_failed = true,
                                }
                            }
                        }
                        // If the daemon connection died (restarted, socket closed),
                        // drop it and retry a fresh connection every couple of
                        // seconds instead of freezing on the last frame forever.
                        if poll_failed
                            && now.duration_since(self.last_reconnect_attempt) >= Duration::from_secs(2)
                        {
                            self.last_reconnect_attempt = now;
                            *self.daemon_stream.lock().unwrap() = None;
                            if let Ok(stream) = connect_daemon_once() {
                                eprintln!("sprite-viewer: reconnected to daemon");
                                *self.daemon_stream.lock().unwrap() = Some(stream);
                            }
                        }
                    }

                    // ── Roam: update position ──
                    self.update_roam(now);

                    let Some(window) = self.window.as_ref() else {
                        return;
                    };

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

                    if let Some(surface) = self.surface.as_mut() {
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

                    window.request_redraw();
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    if state.is_pressed() && button == MouseButton::Left {
                        self.handle_click();
                    }
                }
                _ => {}
            }
        }
    }

    impl App {
        /// Handle left-click: show entry dialog → send to daemon → show response.
        fn handle_click(&self) {
            // Prevent re-entrant dialogs (double-click, rapid clicks, etc.)
            let mut active = self.dialog_active.lock().unwrap();
            if *active {
                return;
            }
            *active = true;
            drop(active); // release lock before spawning

            let backend = match self.dialog_backend {
                Some(b) => b,
                None => return, // no backend available
            };

            let daemon_stream = Arc::clone(&self.daemon_stream);
            let active_flag = Arc::clone(&self.dialog_active);

            std::thread::spawn(move || {
                let _guard = DialogGuard(&active_flag);

                // 1) Show entry dialog
                let question = match backend.entry("Gremlin", "Ask me anything...") {
                    Some(q) if !q.trim().is_empty() => q.trim().to_string(),
                    _ => return, // cancelled or empty
                };

                // 2) Send to daemon
                let response = {
                    let mut guard = daemon_stream.lock().unwrap();
                    guard.as_mut().and_then(|s| ask_daemon(s, &question))
                };

                // 3) Show response
                let response = response.unwrap_or_else(|| "No response from daemon.".to_string());
                let _ = backend.info("Gremlin", &response);
            });
        }

        /// Advance the roaming position one tick, bouncing off screen edges.
        fn update_roam(&mut self, now: Instant) {
            let dt = 0.016; // ~60fps tick
            let sz = self.display_size as f64;
            let margin = 16.0;

            // Change direction every 2-5 seconds
            if now.duration_since(self.last_dir_change) > Duration::from_secs_f64(self.rng.gen_range(2.0..5.0)) {
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
            let speed: f64 = self.rng.gen_range(40.0..120.0);
            let angle: f64 = self.rng.gen_range(0.0..std::f64::consts::TAU);
            self.vel_x = angle.cos() * speed;
            self.vel_y = angle.sin() * speed;
        }
    }

    // RAII guard to reset the dialog_active flag when the thread exits.
    struct DialogGuard<'a>(&'a Mutex<bool>);
    impl<'a> Drop for DialogGuard<'a> {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = false;
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

#[cfg(target_os = "linux")]
fn connect_daemon_once() -> Result<std::os::unix::net::UnixStream, String> {
    let sock = if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{dir}/gremlin.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        format!("/tmp/gremlin-{uid}.sock")
    };
    std::os::unix::net::UnixStream::connect(&sock).map_err(|e| format!("{sock}: {e}"))
}

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