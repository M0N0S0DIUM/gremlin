use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use image::{GenericImageView, ImageBuffer, ImageFormat, Rgba};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::error::GremlinError;

/// Animation state definition from frame map
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationState {
    pub name: String,
    pub frames: Vec<usize>,
    pub fps: u32,
    pub loop_anim: bool,
    pub desc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteSheetMeta {
    pub sheet_file: String,
    pub sheet_dimensions: String,
    pub frame_size: u32,
    pub total_frames: usize,
    pub layout: String,
    pub note: String,
    pub states: Vec<AnimationState>,
}

/// A single extracted frame (owned RGBA pixels)
pub type Frame = ImageBuffer<Rgba<u8>, Vec<u8>>;

/// Loaded sprite sheet with pre-extracted frames
pub struct SpriteSheet {
    pub meta: SpriteSheetMeta,
    pub frames: Vec<Frame>,
    #[allow(dead_code)] // stored for completeness, accessible via meta.frame_size
    pub frame_size: u32,
}

impl SpriteSheet {
    /// Load sprite sheet from directory containing sheet PNG and frame-map JSON
    pub fn load_from_dir<P: AsRef<Path>>(dir: P) -> Result<Self, GremlinError> {
        let dir = dir.as_ref();
        let meta_path = dir.join("sprite-sheet-frame-map.json");
        let sheet_path = dir.join("sprite-sheet-full.png");

        // Load metadata
        let meta_json = fs::read_to_string(&meta_path)
            .map_err(|e| GremlinError::Tool(format!("Failed to read frame map: {e}")))?;
        let meta: SpriteSheetMeta = serde_json::from_str(&meta_json)
            .map_err(|e| GremlinError::Tool(format!("Failed to parse frame map JSON: {e}")))?;

        // Load sprite sheet image
        let sheet_img = image::open(&sheet_path)
            .map_err(|e| GremlinError::Tool(format!("Failed to load sprite sheet: {e}")))?;
        let sheet_rgba = sheet_img.to_rgba8();

        // Verify dimensions
        let (sheet_w, sheet_h) = sheet_rgba.dimensions();
        let expected_w = meta.frame_size * meta.total_frames as u32;
        if sheet_w != expected_w || sheet_h != meta.frame_size {
            return Err(GremlinError::Tool(format!(
                "Sprite sheet dimensions mismatch: got {}x{}, expected {}x{}",
                sheet_w, sheet_h, expected_w, meta.frame_size
            )));
        }

        // Validate every state up front so downstream code (tick/current_frame)
        // can rely on invariants instead of guarding against them everywhere:
        // an empty `frames` list would cause `frames.len() - 1` to underflow,
        // and `fps == 0` would divide-by-zero when computing one-shot duration.
        for state in &meta.states {
            if state.frames.is_empty() {
                return Err(GremlinError::Tool(format!(
                    "Sprite state '{}' has an empty frames list — invalid frame map",
                    state.name
                )));
            }
            if state.fps == 0 {
                return Err(GremlinError::Tool(format!(
                    "Sprite state '{}' has fps=0 — invalid frame map",
                    state.name
                )));
            }
            for &idx in &state.frames {
                if idx >= meta.total_frames {
                    return Err(GremlinError::Tool(format!(
                        "Sprite state '{}' references frame index {} but sheet only has {} frames",
                        state.name, idx, meta.total_frames
                    )));
                }
            }
        }

        // Extract frames
        let mut frames = Vec::with_capacity(meta.total_frames);
        for i in 0..meta.total_frames {
            let x = i as u32 * meta.frame_size;
            let frame = sheet_rgba.view(x, 0, meta.frame_size, meta.frame_size).to_image();
            frames.push(frame);
        }

        info!(
            "Loaded sprite sheet: {} frames @ {}x{} from {}",
            frames.len(), meta.frame_size, meta.frame_size, sheet_path.display()
        );

        let frame_size = meta.frame_size;
        Ok(Self { meta, frames, frame_size })
    }

    /// Get frame by global index
    pub fn frame(&self, index: usize) -> Option<&Frame> {
        self.frames.get(index)
    }

    /// Get frame by state name and local frame index
    pub fn frame_by_state(&self, state_name: &str, local_index: usize) -> Option<&Frame> {
        let state = self.meta.states.iter().find(|s| s.name == state_name)?;
        let global_idx = *state.frames.get(local_index)?;
        self.frame(global_idx)
    }

    /// Get state info
    pub fn state(&self, name: &str) -> Option<&AnimationState> {
        self.meta.states.iter().find(|s| s.name == name)
    }

    /// List all state names
    pub fn state_names(&self) -> Vec<&str> {
        self.meta.states.iter().map(|s| s.name.as_str()).collect()
    }
}

/// Animation controller — tracks current state, frame, timing.
/// All operations here are pure/synchronous (no I/O, no awaits) — protected
/// by a plain std::sync::Mutex so tool closures (which run on the async
/// runtime's worker threads but are NOT themselves async fns) can lock it
/// directly without needing block_on (which panics if called from within
/// a runtime thread — see Tokio's "Cannot start a runtime from within a
/// runtime" panic).
pub struct AnimationController {
    sheet: Arc<SpriteSheet>,
    current_state: String,
    frame_index: usize,     // local index within state
    last_update: Instant,
    paused: bool,
    override_until: Option<Instant>, // for one-shot states that auto-return
}

impl AnimationController {
    pub fn new(sheet: Arc<SpriteSheet>, initial_state: &str) -> Result<Self, GremlinError> {
        if sheet.state(initial_state).is_none() {
            return Err(GremlinError::Tool(format!(
                "Unknown initial state '{}'. Valid: {:?}",
                initial_state,
                sheet.state_names()
            )));
        }
        Ok(Self {
            sheet,
            current_state: initial_state.to_string(),
            frame_index: 0,
            last_update: Instant::now(),
            paused: false,
            override_until: None,
        })
    }

    /// Switch to a new state (resets frame index)
    pub fn set_state(&mut self, state_name: &str, one_shot: bool) -> Result<(), GremlinError> {
        if self.sheet.state(state_name).is_none() {
            return Err(GremlinError::Tool(format!(
                "Unknown state '{}'. Valid: {:?}",
                state_name,
                self.sheet.state_names()
            )));
        }
        self.current_state = state_name.to_string();
        self.frame_index = 0;
        self.paused = false;
        if one_shot {
            let state = self.sheet.state(state_name).unwrap();
            let duration_ms = (state.frames.len() as f32 / state.fps as f32 * 1000.0) as u64;
            self.override_until = Some(Instant::now() + Duration::from_millis(duration_ms));
        } else {
            self.override_until = None;
        }
        debug!("Animation state -> {} (one_shot={})", state_name, one_shot);
        Ok(())
    }

    /// Advance animation based on elapsed time
    pub fn tick(&mut self) -> &Frame {
        // Single lookup — with load-time validation guaranteeing every state
        // has fps > 0 and a non-empty frames list, `- 1` / division are safe.
        let mut state = self.sheet.state(&self.current_state)
            .expect("current_state is only ever set to a validated state name");
        let frame_duration = Duration::from_millis((1000.0 / state.fps as f32) as u64);
        let now = Instant::now();

        // Handle one-shot override expiry
        if let Some(until) = self.override_until {
            if now >= until && self.current_state != "idle" {
                // Only fall back to "idle" if the sheet actually has that state —
                // a custom frame map without an "idle" state would otherwise panic
                // on the next lookup.
                if self.sheet.state("idle").is_some() {
                    self.current_state = "idle".to_string();
                    self.frame_index = 0;
                    self.override_until = None;
                    debug!("One-shot complete, returned to idle");
                    state = self.sheet.state(&self.current_state).unwrap();
                } else {
                    self.override_until = None;
                }
            }
        }

        if !self.paused {
            while now.duration_since(self.last_update) >= frame_duration {
                self.last_update += frame_duration;
                self.frame_index += 1;
                if self.frame_index >= state.frames.len() {
                    if state.loop_anim {
                        self.frame_index = 0;
                    } else {
                        self.frame_index = state.frames.len() - 1;
                        self.paused = true; // freeze on last frame for non-looping
                    }
                }
            }
        }

        let global_idx = state.frames[self.frame_index];
        &self.sheet.frames[global_idx]
    }

    /// Get current frame without advancing time
    pub fn current_frame(&self) -> &Frame {
        let state = self.sheet.state(&self.current_state)
            .expect("current_state is only ever set to a validated state name");
        let idx = self.frame_index.min(state.frames.len() - 1);
        let global_idx = state.frames[idx];
        &self.sheet.frames[global_idx]
    }

    #[allow(dead_code)] // API for external callers / future interactive control
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn current_state(&self) -> &str {
        &self.current_state
    }

    pub fn current_frame_index(&self) -> usize {
        self.frame_index
    }

    /// Encode current frame as base64 PNG for Kitty graphics protocol
    pub fn current_frame_base64(&self) -> Result<String, GremlinError> {
        let frame = self.current_frame();
        let mut buf = Vec::new();
        frame.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .map_err(|e| GremlinError::Tool(format!("PNG encode failed: {e}")))?;
        Ok(BASE64_STANDARD.encode(&buf))
    }

    /// Encode specific frame as base64 PNG
    pub fn frame_base64(&self, state: &str, local_idx: usize) -> Option<String> {
        let frame = self.sheet.frame_by_state(state, local_idx)?;
        let mut buf = Vec::new();
        frame.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png).ok()?;
        Some(BASE64_STANDARD.encode(&buf))
    }

    /// Access to the underlying sheet metadata (for listing states etc.)
    pub fn sheet(&self) -> &SpriteSheet {
        &self.sheet
    }
}

/// Global sprite system holder.
/// Uses std::sync::Mutex (not tokio::sync::Mutex) because all controller
/// operations are synchronous — this lets tool closures (sync fns invoked
/// from ToolRegistry::execute) lock it directly with zero async ceremony,
/// and avoids the "block_on inside a runtime" panic entirely.
pub struct SpriteSystem {
    pub controller: Arc<Mutex<AnimationController>>,
}

impl SpriteSystem {
    pub fn new(sheet_dir: &str, initial_state: &str) -> Result<Self, GremlinError> {
        let sheet = Arc::new(SpriteSheet::load_from_dir(sheet_dir)?);
        let controller = Arc::new(Mutex::new(AnimationController::new(sheet, initial_state)?));
        Ok(Self { controller })
    }

    /// Spawn the animation tick loop (call once at startup).
    /// Uses spawn_blocking + std::thread-style sleep loop since the mutex
    /// is now a std::sync::Mutex; this keeps a dedicated background thread
    /// ticking the animation without holding up the tokio runtime.
    pub fn spawn_ticker(self: &Arc<Self>) {
        let controller = self.controller.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                std::thread::sleep(Duration::from_millis(16)); // ~60fps tick
                if let Ok(mut ctrl) = controller.lock() {
                    ctrl.tick();
                }
            }
        });
    }
}

/// Register sprite tools in the tool registry.
/// NOTE: these closures are plain sync fns (Box<dyn Fn(...) -> Result<...>>)
/// invoked synchronously by ToolRegistry::execute — they must NOT use
/// tokio::runtime::Handle::block_on, since tool execution can happen from
/// within an async task on the runtime's own worker threads, and block_on
/// from inside a runtime thread panics ("Cannot start a runtime from
/// within a runtime"). Because AnimationController uses a std::sync::Mutex
/// and does no I/O, we can just lock it directly here — no async needed.
pub fn register_sprite_tools(registry: &mut crate::tools::ToolRegistry, sprite_system: Arc<SpriteSystem>) {
    use crate::error::GremlinError;

    // sprite_state tool
    {
        let system = sprite_system.clone();
        registry.register(
            "sprite_state",
            "Set the sprite animation state. Use one_shot=true for transient states (error, wave, celebrate, yawn, wake) that auto-return to idle.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {"type": "string", "description": "Animation state name: sleep, wake, idle, think, typing, walk, coffee, error, yawn, wave, celebrate, sit"},
                    "one_shot": {"type": "boolean", "description": "If true, auto-return to idle after animation completes (default: false)"}
                },
                "required": ["state"]
            }),
            Box::new(move |args| {
                let state = args["state"].as_str().ok_or_else(|| GremlinError::Tool("missing 'state'".into()))?;
                let one_shot = args["one_shot"].as_bool().unwrap_or(false);
                let mut ctrl = system.controller.lock()
                    .map_err(|_| GremlinError::Tool("sprite controller lock poisoned".into()))?;
                ctrl.set_state(state, one_shot)?;
                Ok(format!("Sprite state set to '{}' (one_shot={})", state, one_shot))
            }),
        );
    }

    // sprite_status tool
    {
        let system = sprite_system.clone();
        registry.register(
            "sprite_status",
            "Get current sprite animation state and frame info.",
            serde_json::json!({"type": "object", "properties": {}}),
            Box::new(move |_args| {
                let ctrl = system.controller.lock()
                    .map_err(|_| GremlinError::Tool("sprite controller lock poisoned".into()))?;
                let state = ctrl.current_state();
                let frame_idx = ctrl.current_frame_index();
                Ok(format!("State: {}, Frame: {}", state, frame_idx))
            }),
        );
    }

    // sprite_frame tool — get base64 PNG of a specific frame
    {
        let system = sprite_system.clone();
        registry.register(
            "sprite_frame",
            "Get a specific frame as base64 PNG for terminal graphics (Kitty protocol).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {"type": "string", "description": "State name"},
                    "frame": {"type": "integer", "description": "Local frame index within state"}
                },
                "required": ["state", "frame"]
            }),
            Box::new(move |args| {
                let state = args["state"].as_str().ok_or_else(|| GremlinError::Tool("missing 'state'".into()))?;
                let frame = args["frame"].as_u64().ok_or_else(|| GremlinError::Tool("missing 'frame'".into()))? as usize;
                let ctrl = system.controller.lock()
                    .map_err(|_| GremlinError::Tool("sprite controller lock poisoned".into()))?;
                ctrl.frame_base64(state, frame)
                    .ok_or_else(|| GremlinError::Tool("invalid state/frame".into()))
                    .map(|b64| format!("data:image/png;base64,{}", b64))
            }),
        );
    }

    // sprite_current_frame tool — get current frame as base64
    {
        let system = sprite_system.clone();
        registry.register(
            "sprite_current_frame",
            "Get the current animation frame as base64 PNG for terminal graphics.",
            serde_json::json!({"type": "object", "properties": {}}),
            Box::new(move |_args| {
                let ctrl = system.controller.lock()
                    .map_err(|_| GremlinError::Tool("sprite controller lock poisoned".into()))?;
                let b64 = ctrl.current_frame_base64()?;
                Ok(format!("data:image/png;base64,{}", b64))
            }),
        );
    }

    // sprite_list_states tool
    {
        let system = sprite_system.clone();
        registry.register(
            "sprite_list_states",
            "List all available animation states with frame counts and FPS.",
            serde_json::json!({"type": "object", "properties": {}}),
            Box::new(move |_args| {
                let ctrl = system.controller.lock()
                    .map_err(|_| GremlinError::Tool("sprite controller lock poisoned".into()))?;
                let sheet = ctrl.sheet();
                let lines: Vec<String> = sheet.meta.states.iter().map(|s| {
                    format!("  {}: {} frames @ {}fps, loop={}, {}", s.name, s.frames.len(), s.fps, s.loop_anim, s.desc)
                }).collect();
                Ok(format!("Available states:\n{}", lines.join("\n")))
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sheet_dir() -> std::path::PathBuf {
        // Repo-relative assets dir used by the daemon at runtime.
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/sprites")
    }

    #[test]
    fn test_load_sheet_and_states() {
        let sheet = SpriteSheet::load_from_dir(test_sheet_dir()).expect("sheet should load");
        assert_eq!(sheet.frames.len(), sheet.meta.total_frames);
        assert!(sheet.state("idle").is_some(), "expected an 'idle' state");
    }

    #[test]
    fn test_set_state_and_tick_dont_panic() {
        let sheet = Arc::new(SpriteSheet::load_from_dir(test_sheet_dir()).expect("sheet should load"));
        let mut ctrl = AnimationController::new(sheet, "idle").expect("controller should init");
        ctrl.set_state("wave", true).expect("wave is a valid state");
        // Simulate several ticks — must not panic regardless of thread context.
        for _ in 0..5 {
            ctrl.tick();
        }
        let _ = ctrl.current_frame_base64();
    }

    /// Regression test for the "Cannot start a runtime from within a runtime"
    /// panic: tool closures must be able to lock the controller synchronously
    /// from inside a tokio worker thread (i.e. without block_on). This test
    /// runs the registered tool closures directly inside a tokio runtime to
    /// prove they don't rely on block_on/Handle::current, which previously
    /// caused a panic during real `ask`/`daemon` tool-call loops.
    #[tokio::test]
    async fn test_sprite_tools_work_inside_async_runtime() {
        let system = Arc::new(
            SpriteSystem::new(test_sheet_dir().to_str().unwrap(), "idle")
                .expect("sprite system should init"),
        );
        let mut registry = crate::tools::ToolRegistry::new().expect("ToolRegistry::new() should succeed");
        register_sprite_tools(&mut registry, system);

        // Directly exercises sprite_state's closure from within this async
        // test's tokio runtime context — this is exactly the situation that
        // previously panicked.
        let result = registry.execute("sprite_list_states", serde_json::json!({}));
        assert!(result.success, "sprite_list_states failed: {}", result.output);

        let result = registry.execute("sprite_state", serde_json::json!({"state": "wave", "one_shot": true}));
        assert!(result.success, "sprite_state failed: {}", result.output);

        let result = registry.execute("sprite_status", serde_json::json!({}));
        assert!(result.success, "sprite_status failed: {}", result.output);

        let result = registry.execute("sprite_current_frame", serde_json::json!({}));
        assert!(result.success, "sprite_current_frame failed: {}", result.output);
    }
}
