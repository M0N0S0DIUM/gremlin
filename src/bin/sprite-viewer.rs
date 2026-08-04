//! sprite-viewer — borderless floating Wayland window that polls the Gremlin
//! daemon for the current sprite animation frame and renders it via wgpu.
//! Roams the screen with a lazy random walk — desktop pet behaviour.
//! Left-click opens a zenity/rofi/wofi dialog to ask Gremlin a question.
//!
//! Rendering: a single textured quad, nearest-neighbour sampled, GPU-scaled
//! from the native 48x48 frame up to the window size. The fragment shader
//! premultiplies alpha before blending — this is what actually fixes the
//! "garbled sprite" bug (softbuffer's raw ARGB8888 path required a manual
//! CPU premultiply hack; wgpu's surface + blend state do it properly and
//! also gets us vsync for free instead of a bare `request_redraw()` busy loop).
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
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::process::Command;
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

    /// Send one command to Hyprland's control socket and close it immediately —
    /// Hyprland processes these synchronously, so holding the connection open
    /// could stall the compositor.
    fn hyprland_dispatch(command: &str) -> Result<(), String> {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map_err(|_| "XDG_RUNTIME_DIR is not set".to_string())?;
        let instance = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is not set".to_string())?;
        let socket = format!("{runtime_dir}/hypr/{instance}/.socket.sock");
        let mut stream =
            UnixStream::connect(&socket).map_err(|e| format!("cannot connect to {socket}: {e}"))?;
        stream
            .write_all(command.as_bytes())
            .map_err(|e| format!("cannot send Hyprland command: {e}"))?;
        stream
            .shutdown(Shutdown::Both)
            .map_err(|e| format!("cannot close Hyprland IPC socket: {e}"))
    }

    /// Send a single position update to Hyprland's command socket.  Wayland
    /// intentionally does not let clients position their own windows, so
    /// winit's `set_outer_position` is a no-op there.  Hyprland's IPC is the
    /// compositor-approved path for moving this explicitly floating window.
    fn move_sprite_with_hyprland(x: f64, y: f64) -> Result<(), String> {
        hyprland_dispatch(&format!(
            "dispatch movewindowpixel exact {} {},class:^(gremlin-sprite)$",
            x.round() as i32,
            y.round() as i32,
        ))
    }

    /// Last-resort geometry fix when the compositor tiled us instead of
    /// honouring our 192×192 request (i.e. the user's windowrule is missing).
    /// Floats the window and pins its size so the sprite isn't stretched across
    /// a whole workspace column.
    fn force_sprite_geometry_with_hyprland(sz: u32) -> Result<(), String> {
        const SEL: &str = "class:^(gremlin-sprite)$";
        hyprland_dispatch(&format!("dispatch setfloating {SEL}"))?;
        hyprland_dispatch(&format!("dispatch resizewindowpixel exact {sz} {sz},{SEL}"))?;
        hyprland_dispatch(&format!("dispatch pin {SEL}"))
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

    /// Call `sprite_current_frame` directly on the daemon (bypasses LLM) and
    /// decode the base64 PNG it returns.
    pub fn poll_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
        let req = serde_json::json!({"tool":"sprite_current_frame"});
        let mut req_bytes = serde_json::to_vec(&req).ok()?;
        req_bytes.push(b'\n');
        stream.write_all(&req_bytes).ok()?;

        let resp = read_reply(stream)?;
        let b64 = resp["response"]
            .as_str()?
            .strip_prefix("data:image/png;base64,")?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()
    }

    /// Decode a raw PNG, return RGBA pixels + dimensions.
    pub fn decode_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let img = image::load_from_memory(data).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        Some((img.into_raw(), w, h))
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
            String::from_utf8(output.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        }

        /// Show an info dialog with the response.
        fn info(&self, title: &str, text: &str) -> bool {
            let args: Vec<&str> = match self {
                Self::Zenity => vec!["--info", "--title", title, "--text", text, "--width=500"],
                Self::Rofi => vec![
                    "-e",
                    "-no-fixed-num-lines",
                    "-theme-str",
                    "window {width: 500;}",
                ],
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
        Command::new("which")
            .arg(cmd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Read one newline-delimited JSON reply from the daemon. Loops until the
    /// newline arrives instead of trusting a single `read()` — a long LLM answer
    /// (or a base64 PNG frame) routinely spans multiple kernel reads.
    fn read_reply(stream: &mut UnixStream) -> Option<serde_json::Value> {
        let mut buf = Vec::with_capacity(64 * 1024);
        let mut chunk = [0u8; 16384];
        loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None; // peer closed mid-response
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') {
                break;
            }
            if buf.len() > 8 * 1024 * 1024 {
                return None; // corrupt stream guard
            }
        }
        let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
        serde_json::from_slice(&buf[..end]).ok()
    }

    /// Send a message to the daemon and return the response text.
    fn ask_daemon(daemon_stream: &mut UnixStream, question: &str) -> Option<String> {
        let req = serde_json::json!({"message": question});
        let mut req_bytes = serde_json::to_vec(&req).ok()?;
        req_bytes.push(b'\n');
        daemon_stream.write_all(&req_bytes).ok()?;

        let resp = read_reply(daemon_stream)?;
        resp["response"].as_str().map(|s| s.to_string())
    }

    // ── wgpu render state ──

    /// Everything needed to draw one textured quad. Rebuilt once at window
    /// creation; the sprite texture itself is re-uploaded (and resized if the
    /// daemon's frame dimensions ever change) on every poll.
    struct Gpu {
        surface: wgpu::Surface<'static>,
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: wgpu::SurfaceConfiguration,
        pipeline: wgpu::RenderPipeline,
        bind_group_layout: wgpu::BindGroupLayout,
        sampler: wgpu::Sampler,
        texture: wgpu::Texture,
        bind_group: wgpu::BindGroup,
        tex_size: (u32, u32),
    }

    const SHADER_SRC: &str = r#"
        struct VsOut {
            @builtin(position) pos: vec4<f32>,
            @location(0) uv: vec2<f32>,
        };

        @vertex
        fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
            var positions = array<vec2<f32>, 6>(
                vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
                vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
            );
            var uvs = array<vec2<f32>, 6>(
                vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 0.0),
                vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0),
            );
            var out: VsOut;
            out.pos = vec4<f32>(positions[i], 0.0, 1.0);
            out.uv = uvs[i];
            return out;
        }

        @group(0) @binding(0) var tex: texture_2d<f32>;
        @group(0) @binding(1) var samp: sampler;

        @fragment
        fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
            let color = textureSample(tex, samp, in.uv);
            // Premultiply — matches the surface's premultiplied blend state
            // below. This is the fix for the garbled/opaque-box sprite bug.
            return vec4<f32>(color.rgb * color.a, color.a);
        }
    "#;

    impl Gpu {
        fn new(window: Arc<Window>, width: u32, height: u32) -> Option<Self> {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::PRIMARY,
                ..Default::default()
            });
            let surface = instance.create_surface(window.clone()).ok()?;
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))?;
            let (device, queue) = pollster::block_on(adapter.request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("gremlin-sprite-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                },
                None,
            ))
            .ok()?;

            let caps = surface.get_capabilities(&adapter);
            let format = caps
                .formats
                .iter()
                .find(|f| !f.is_srgb())
                .copied()
                .unwrap_or(caps.formats[0]);
            // Prefer a premultiplied compositor mode to match the shader's
            // premultiply step; fall back to whatever the compositor offers.
            let alpha_mode = [
                wgpu::CompositeAlphaMode::PreMultiplied,
                wgpu::CompositeAlphaMode::PostMultiplied,
                wgpu::CompositeAlphaMode::Inherit,
            ]
            .into_iter()
            .find(|m| caps.alpha_modes.contains(m))
            .unwrap_or(caps.alpha_modes[0]);

            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: width.max(1),
                height: height.max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                alpha_mode,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(&device, &config);

            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("gremlin-sprite-shader"),
                source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
            });

            let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("gremlin-sprite-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("gremlin-sprite-pipeline-layout"),
                bind_group_layouts: &[&bind_group_layout],
                push_constant_ranges: &[],
            });

            // Standard premultiplied-alpha blend: the fragment shader already
            // multiplied rgb by alpha, so color blends with ONE/ONE_MINUS_SRC_ALPHA.
            let blend = wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
            };

            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("gremlin-sprite-pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_main",
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs_main",
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });

            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("gremlin-sprite-sampler"),
                mag_filter: wgpu::FilterMode::Nearest, // pixel art — no blurring on upscale
                min_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            });

            let (texture, bind_group) =
                Self::make_texture(&device, &bind_group_layout, &sampler, FRAME_SIZE, FRAME_SIZE);

            Some(Self {
                surface,
                device,
                queue,
                config,
                pipeline,
                bind_group_layout,
                sampler,
                texture,
                bind_group,
                tex_size: (FRAME_SIZE, FRAME_SIZE),
            })
        }

        fn make_texture(
            device: &wgpu::Device,
            layout: &wgpu::BindGroupLayout,
            sampler: &wgpu::Sampler,
            w: u32,
            h: u32,
        ) -> (wgpu::Texture, wgpu::BindGroup) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("gremlin-sprite-texture"),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("gremlin-sprite-bind-group"),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            (texture, bind_group)
        }

        fn resize(&mut self, width: u32, height: u32) {
            if width == 0 || height == 0 {
                return;
            }
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }

        /// Upload a new RGBA frame, recreating the texture if dimensions changed.
        fn upload_frame(&mut self, rgba: &[u8], w: u32, h: u32) {
            if (w, h) != self.tex_size {
                let (texture, bind_group) =
                    Self::make_texture(&self.device, &self.bind_group_layout, &self.sampler, w, h);
                self.texture = texture;
                self.bind_group = bind_group;
                self.tex_size = (w, h);
            }
            self.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }

        /// Draw the current texture to the surface. Errors (e.g. a transient
        /// `Lost`/`Outdated` surface after a compositor resize race) are
        /// logged and swallowed — the next redraw will retry.
        fn render(&mut self) {
            let frame = match self.surface.get_current_texture() {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("sprite-viewer: get_current_texture failed: {e}");
                    return;
                }
            };
            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("gremlin-sprite-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
            self.queue.submit(Some(encoder.finish()));
            frame.present();
        }
    }

    // ── Main application ──

    pub struct App {
        window: Option<Arc<Window>>,
        gpu: Option<Gpu>,
        daemon_stream: Arc<Mutex<Option<UnixStream>>>,
        daemon_connected: bool, // tracked so reconnects log on change, not every retry
        last_frame_rgba: Option<(Vec<u8>, u32, u32)>,
        last_poll: Instant,
        last_reconnect_attempt: Instant,
        display_size: u32,
        // ── roaming state ──
        pos_x: f64,
        pos_y: f64,
        vel_x: f64,
        vel_y: f64,
        last_dir_change: Instant,
        last_compositor_move: Instant,
        hyprland_roaming: bool,
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
                gpu: None,
                daemon_stream: Arc::new(Mutex::new(None)),
                daemon_connected: true, // main() hands us a live stream
                last_frame_rgba: None,
                last_poll: Instant::now(),
                last_reconnect_attempt: Instant::now(),
                display_size: FRAME_SIZE * scale,
                pos_x: 0.0,
                pos_y: 0.0,
                vel_x: 0.0,
                vel_y: 0.0,
                last_dir_change: Instant::now(),
                last_compositor_move: Instant::now() - Duration::from_secs(1),
                hyprland_roaming: std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
                screen_w: 1920,
                screen_h: 1080,
                rng: rand::thread_rng(),
                dialog_backend,
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
            let logical = PhysicalSize::new(sz, sz);
            let attrs = WindowAttributes::default()
                .with_inner_size(logical)
                // Pin min == max so a tiling compositor can't stretch us to fill
                // a workspace column. Without this Hyprland tiles the surface to
                // the full monitor when its windowrule doesn't match, which is
                // exactly the "gremlin takes up a whole panel" bug.
                .with_min_inner_size(logical)
                .with_max_inner_size(logical)
                .with_resizable(false)
                .with_decorations(false)
                .with_title("gremlin-sprite")
                .with_name("gremlin-sprite", "gremlin-sprite")
                .with_visible(true)
                .with_transparent(true);

            let window = Arc::new(
                event_loop
                    .create_window(attrs)
                    .expect("failed to create Wayland window"),
            );

            let size = window.inner_size();
            self.gpu = Gpu::new(window.clone(), size.width, size.height);
            if self.gpu.is_none() {
                eprintln!("sprite-viewer: failed to initialize wgpu — no renderer available");
            }

            // Sniff monitor geometry for edge-bouncing
            if let Some(monitor) = window.current_monitor() {
                let s = monitor.size();
                self.screen_w = s.width.max(sz + 64);
                self.screen_h = s.height.max(sz + 64);
            }
            self.pos_x = (self.screen_w.saturating_sub(sz + 32)) as f64;
            self.pos_y = (self.screen_h.saturating_sub(sz + 32)) as f64;
            self.randomize_velocity();

            // Report what the compositor ACTUALLY gave us, not what we asked
            // for — a mismatch here is the tell that the windowrule didn't
            // match and we got tiled.
            eprintln!(
                "sprite-viewer: requested {sz}×{sz}, got {}×{} on {}×{}, roaming from ({:.0},{:.0})",
                size.width, size.height, self.screen_w, self.screen_h, self.pos_x, self.pos_y
            );
            if size.width != sz || size.height != sz {
                eprintln!(
                    "sprite-viewer: compositor ignored our size — forcing it via Hyprland IPC. \
                     Add this to ~/.config/hypr/hyprland.conf to fix it properly:\n  \
                     windowrule = match:class ^(gremlin-sprite)$, float on, pin on, noborder on, \
                     noshadow on, nofocus on, noanim on, size {sz} {sz}"
                );
                // Self-heal: don't just render wrong because the user's config
                // is missing a line. Float + resize ourselves over IPC.
                if let Err(e) = force_sprite_geometry_with_hyprland(sz) {
                    eprintln!("sprite-viewer: could not self-correct geometry: {e}");
                }
            }
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
                WindowEvent::Resized(size) => {
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.resize(size.width, size.height);
                    }
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
                                            self.last_frame_rgba = Some((rgba, w, h));
                                        }
                                    }
                                    None => poll_failed = true,
                                }
                            }
                        }
                        // If the daemon connection died (restarted, socket closed),
                        // drop it and retry a fresh connection every couple of
                        // seconds instead of freezing on the last frame forever.
                        // Only log on state CHANGE — a persistent failure used to
                        // print every 2s forever and drown the terminal.
                        if poll_failed
                            && now.duration_since(self.last_reconnect_attempt)
                                >= Duration::from_secs(2)
                        {
                            self.last_reconnect_attempt = now;
                            if self.daemon_connected {
                                eprintln!("sprite-viewer: lost daemon connection, reconnecting...");
                                self.daemon_connected = false;
                            }
                            *self.daemon_stream.lock().unwrap() = None;
                            if let Ok(stream) = connect_daemon_once() {
                                eprintln!("sprite-viewer: reconnected to daemon");
                                *self.daemon_stream.lock().unwrap() = Some(stream);
                                self.daemon_connected = true;
                            }
                        }
                    }

                    // ── Roam: update position ──
                    self.update_roam(now);

                    let Some(window) = self.window.as_ref() else {
                        return;
                    };

                    // ── Render ──
                    if let Some(gpu) = self.gpu.as_mut() {
                        let (rgba, w, h): (&[u8], u32, u32) = match &self.last_frame_rgba {
                            Some((data, w, h)) => (data, *w, *h),
                            None => (placeholder_rgba(), FRAME_SIZE, FRAME_SIZE),
                        };
                        gpu.upload_frame(rgba, w, h);
                        gpu.render();
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

    /// Dark green radial gradient shown until the first real frame arrives —
    /// keeps the window visibly non-blank on startup instead of flashing
    /// fully transparent/black for a frame or two.
    fn placeholder_rgba() -> &'static [u8] {
        use std::sync::LazyLock;
        static PLACEHOLDER: LazyLock<Vec<u8>> = LazyLock::new(|| {
            let sz = FRAME_SIZE as usize;
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
        &PLACEHOLDER
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
                None => {
                    eprintln!("sprite-viewer: no dialog backend available (install zenity/rofi/wofi)");
                    *self.dialog_active.lock().unwrap() = false;
                    return;
                }
            };

            let daemon_stream = Arc::clone(&self.daemon_stream);
            let active_flag = Arc::clone(&self.dialog_active);

            std::thread::spawn(move || {
                let _guard = DialogGuard(&active_flag);

                // 1) Show entry dialog
                let question = match backend.entry("Gremlin", "Ask me anything...") {
                    Some(q) if !q.trim().is_empty() => q.trim().to_string(),
                    _ => {
                        eprintln!("sprite-viewer: dialog cancelled or empty input");
                        return;
                    }
                };

                eprintln!("sprite-viewer: sending question to daemon: {}", question);

                // 2) Send to daemon
                let response = {
                    let mut guard = daemon_stream.lock().unwrap();
                    guard.as_mut().and_then(|s| ask_daemon(s, &question))
                };

                // 3) Show response
                let response = match response {
                    Some(r) => {
                        eprintln!("sprite-viewer: got response ({} chars)", r.len());
                        r
                    }
                    None => {
                        eprintln!("sprite-viewer: daemon returned no response");
                        "No response from daemon.".to_string()
                    }
                };

                // Try to show response; if it fails, log it so we know
                if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    backend.info("Gremlin", &response)
                })) {
                    eprintln!("sprite-viewer: dialog backend info() panicked: {:?}", e);
                }
            });
        }

        /// Advance the roaming position one tick, bouncing off screen edges.
        fn update_roam(&mut self, now: Instant) {
            let dt = 0.016; // ~60fps tick
            let sz = self.display_size as f64;
            let margin = 16.0;

            // Change direction every 2-5 seconds
            if now.duration_since(self.last_dir_change)
                > Duration::from_secs_f64(self.rng.gen_range(2.0..5.0))
            {
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

            if self.pos_x < min_x {
                self.pos_x = min_x;
                self.vel_x = self.vel_x.abs();
            }
            if self.pos_x > max_x {
                self.pos_x = max_x;
                self.vel_x = -self.vel_x.abs();
            }
            if self.pos_y < min_y {
                self.pos_y = min_y;
                self.vel_y = self.vel_y.abs();
            }
            if self.pos_y > max_y {
                self.pos_y = max_y;
                self.vel_y = -self.vel_y.abs();
            }

            if self.hyprland_roaming {
                // Limit IPC updates to 30 Hz. Hyprland handles commands
                // synchronously, so sending one for every 60 Hz redraw can
                // needlessly make the compositor feel sluggish.
                if now.duration_since(self.last_compositor_move) >= Duration::from_millis(33) {
                    self.last_compositor_move = now;
                    if let Err(e) = move_sprite_with_hyprland(self.pos_x, self.pos_y) {
                        eprintln!("sprite-viewer: disabling Hyprland roaming: {e}");
                        self.hyprland_roaming = false;
                    }
                }
            } else if let Some(ref window) = self.window {
                // Kept for X11, where clients can still position their own windows.
                let _ = window.set_outer_position(PhysicalPosition::new(self.pos_x, self.pos_y));
            }
        }

        fn randomize_velocity(&mut self) {
                // Mostly stationary: 80% chance of near-zero velocity, 20% gentle drift
                if self.rng.gen_bool(0.8) {
                    self.vel_x = 0.0;
                    self.vel_y = 0.0;
                } else {
                    let speed: f64 = self.rng.gen_range(5.0..20.0); // gentle drift
                    let angle: f64 = self.rng.gen_range(0.0..std::f64::consts::TAU);
                    self.vel_x = angle.cos() * speed;
                    self.vel_y = angle.sin() * speed;
                }
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
