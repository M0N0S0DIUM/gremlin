pub mod cargo;
pub mod clipboard;
pub mod filesystem;
pub mod git;
pub mod shell;
pub mod system;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::GremlinError;

/// A tool that Gremlin can invoke — the LLM picks the tool, we execute it
#[derive(Debug, Clone)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

/// Result of executing a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: String,
    pub success: bool,
    pub output: String,
}

/// Registry of all available tools
pub struct ToolRegistry {
    tools: Vec<Tool>,
    /// Map tool name -> handler function
    handlers: HashMap<String, Box<dyn Fn(serde_json::Value) -> Result<String, GremlinError> + Send + Sync>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            tools: Vec::new(),
            handlers: HashMap::new(),
        };

        // Register built-in tools
        registry.register(
            "read_file",
            "Read the contents of a file at the given path. Returns the file content or an error.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or relative path to the file"}
                },
                "required": ["path"]
            }),
            Box::new(|args| {
                let path = args["path"].as_str().ok_or_else(|| {
                    GremlinError::Tool("missing 'path' argument".into())
                })?;
                filesystem::read_file(path)
            }),
        );

        registry.register(
            "write_file",
            "Write content to a file. Overwrites existing files. Creates parent directories.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or relative path to the file"},
                    "content": {"type": "string", "description": "Content to write"}
                },
                "required": ["path", "content"]
            }),
            Box::new(|args| {
                let path = args["path"].as_str().ok_or_else(|| {
                    GremlinError::Tool("missing 'path' argument".into())
                })?;
                let content = args["content"].as_str().ok_or_else(|| {
                    GremlinError::Tool("missing 'content' argument".into())
                })?;
                filesystem::write_file(path, content)?;
                Ok(format!("Wrote {}", path))
            }),
        );

        registry.register(
            "git_status",
            "Get the current git status of the repository in the current directory.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| git::status()),
        );

        registry.register(
            "git_diff",
            "Get the current git diff (unstaged changes) in the repository.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "staged": {"type": "boolean", "description": "If true, show staged changes instead"}
                },
                "required": []
            }),
            Box::new(|args| {
                let staged = args.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
                git::diff(staged)
            }),
        );

        registry.register(
            "git_branch",
            "Get the current git branch name.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| git::current_branch()),
        );

        registry.register(
            "pwd",
            "Get the current working directory.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| system::pwd()),
        );

        registry.register(
            "list_dir",
            "List files and directories in the given path.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory path (defaults to current directory)"}
                },
                "required": []
            }),
            Box::new(|args| {
                let path = args.get("path").and_then(|v| v.as_str());
                system::list_dir(path)
            }),
        );

        // ── Desktop awareness tools (Hyprland) ──

        registry.register(
            "active_window",
            "Get the title and class of the currently focused window. Requires Hyprland.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| crate::desktop::hyprland::focused_title()),
        );

        registry.register(
            "active_workspace",
            "Get the currently active Hyprland workspace ID and name.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| {
                let (id, name) = crate::desktop::hyprland::active_workspace()?;
                Ok(format!("Workspace {id}: {name}"))
            }),
        );

        registry.register(
            "list_workspaces",
            "List all Hyprland workspaces with their window counts.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| {
                let workspaces = crate::desktop::hyprland::list_workspaces()?;
                let lines: Vec<String> = workspaces
                    .iter()
                    .map(|w| format!("Workspace {}: {} ({} windows on {})", w.id, w.name, w.windows, w.monitor))
                    .collect();
                Ok(lines.join("\n"))
            }),
        );

        registry.register(
            "list_monitors",
            "List all monitors with their resolutions and refresh rates. Requires Hyprland.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| {
                let monitors = crate::desktop::hyprland::list_monitors()?;
                let lines: Vec<String> = monitors
                    .iter()
                    .map(|m| format!("{}: {} {}x{}@{}Hz", m.name, m.description, m.width, m.height, m.refresh_rate))
                    .collect();
                Ok(lines.join("\n"))
            }),
        );

        // ── Clipboard ──

        registry.register(
            "clipboard",
            "Read the current clipboard text contents.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| clipboard::read()),
        );

        registry.register(
            "clipboard_write",
            "Write text to the system clipboard.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Text to write to the clipboard"}
                },
                "required": ["text"]
            }),
            Box::new(|args| {
                let text = args["text"].as_str().unwrap_or("");
                clipboard::write(text)?;
                Ok("Written to clipboard".to_string())
            }),
        );

        // ── Cargo tools ──

        registry.register(
            "cargo_build",
            "Run `cargo build` and capture compiler output (errors and warnings). Fails gracefully if not in a Rust project.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "release": {"type": "boolean", "description": "Build in release mode (default: false)"}
                },
                "required": []
            }),
            Box::new(|args| {
                if args.get("release").and_then(|v| v.as_bool()).unwrap_or(false) {
                    cargo::build_release()
                } else {
                    cargo::build()
                }
            }),
        );

        registry.register(
            "cargo_check",
            "Run `cargo check` (faster than build, same errors). Captures compiler output.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| cargo::check()),
        );

        registry.register(
            "cargo_test",
            "Run `cargo test` and capture test output.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| cargo::test()),
        );

        // ── Shell tools ──

        registry.register(
            "recent_commands",
            "Get recent commands from fish shell history. Useful for understanding what the user was doing.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "Number of commands to return (default: 20, max: 100)"}
                },
                "required": []
            }),
            Box::new(|args| {
                let limit = args.get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20) as usize;
                shell::recent_commands(limit)
            }),
        );

        registry.register(
            "kitty_cwd",
            "Get the current working directory of the focused Kitty terminal tab. Falls back to process cwd.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| shell::kitty_cwd()),
        );

        registry.register(
            "last_exit_code",
            "Get the exit code of the last command run in fish shell. Useful for understanding errors.",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            Box::new(|_args| shell::last_exit_code()),
        );

        // ── Vision / Screenshot ──

        registry.register(
            "screenshot",
            "Capture a screenshot and describe what's on screen using the vision model. \
             Useful when the user asks 'what do you see?' or 'look at this error'.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "What to look for in the screenshot (e.g. 'Describe the error on screen')"
                    }
                },
                "required": []
            }),
            Box::new(|args| {
                let prompt = args.get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Describe what you see on the screen. Focus on code, errors, and UI elements.")
                    .to_string();

                let image_bytes = crate::vision::capture_screenshot()?;

                let config = crate::config::Config::load()
                    .map_err(|e| GremlinError::Tool(format!("Config error: {e}")))?;

                let vision_model = config.vision
                    .as_ref()
                    .map(|v| v.model.clone())
                    .unwrap_or_else(|| "llama3.2-vision:11b".to_string());

                let ollama_url = config.ollama.url.clone();

                // NOTE: this closure runs synchronously from inside ToolRegistry::execute(),
                // which itself is called from within an async fn running on a tokio worker
                // thread (daemon.rs's query() loop). Calling Handle::current().block_on(...)
                // here would panic ("Cannot start a runtime from within a runtime"). Instead
                // we spawn a plain OS thread with its own throwaway single-threaded runtime
                // and block on THAT thread, which has no tokio context of its own.
                let img_len = image_bytes.len();
                let vision_model_for_result = vision_model.clone();
                let description = std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| GremlinError::Tool(format!("Failed to create runtime: {e}")))?;
                    rt.block_on(crate::vision::describe_screenshot(
                        &ollama_url,
                        &vision_model,
                        &image_bytes,
                        &prompt,
                    ))
                })
                .join()
                .map_err(|_| GremlinError::Tool("Screenshot worker thread panicked".into()))??;

                Ok(format!(
                    "Screenshot ({:.1}KB) — {vision_model_for_result}:\n{description}",
                    img_len as f64 / 1024.0
                ))
            }),
        );

        // ── Hermes coding service ──

        registry.register(
            "launch_hermes",
            "Delegate a coding task to the Hermes coding service. Use this for code review, \
             bug fixes, refactoring, architecture questions, documentation, or any non-trivial \
             code work. Hermes has access to a larger coding model. \
             Available templates: code_review, bug_fix, architecture, refactor, explain, documentation.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "template": {
                        "type": "string",
                        "description": "Template name: code_review, bug_fix, architecture, refactor, explain, documentation"
                    },
                    "request": {
                        "type": "string",
                        "description": "The specific coding task or question. Be detailed."
                    }
                },
                "required": ["template", "request"]
            }),
            Box::new(|args| {
                let template_name = args["template"].as_str().unwrap_or("code_review").to_string();
                let request = args["request"].as_str().unwrap_or("Review the code").to_string();

                let template = crate::hermes::find_template(&template_name)
                    .ok_or_else(|| GremlinError::Tool(format!(
                        "Unknown template '{}'. Available: {}",
                        template_name,
                        crate::hermes::template_names().join(", ")
                    )))?;

                let hermes_config = crate::config::Config::load()
                    .map_err(|e| GremlinError::Tool(format!("Failed to load config: {e}")))?
                    .hermes
                    .ok_or_else(|| GremlinError::Tool(
                        "Hermes is not configured. Add [hermes] section to ~/.config/gremlin/config.toml".into()
                    ))?;

                let context = crate::context::Context::collect(&crate::tools::ToolRegistry::new());

                // Same rationale as the screenshot tool above: run the async launch on a
                // dedicated OS thread with its own runtime instead of block_on-ing the
                // caller's (already-async) tokio worker thread.
                let result = std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| GremlinError::Tool(format!("Failed to create runtime: {e}")))?;
                    rt.block_on(crate::hermes::launch(&hermes_config, &template, &context, &request))
                })
                .join()
                .map_err(|_| GremlinError::Tool("Hermes worker thread panicked".into()))??;

                Ok(format!(
                    "Hermes {} (took {:.1}s):\n{}",
                    if result.success { "completed" } else { "failed" },
                    result.duration_secs,
                    result.output
                ))
            }),
        );

        registry
    }

    /// Register a new tool
    pub fn register(
        &mut self,
        name: &'static str,
        description: &'static str,
        parameters: serde_json::Value,
        handler: Box<dyn Fn(serde_json::Value) -> Result<String, GremlinError> + Send + Sync>,
    ) {
        self.tools.push(Tool {
            name,
            description,
            parameters,
        });
        self.handlers.insert(name.to_string(), handler);
    }

    /// Get the tools schema for the LLM (for function calling / tool-use prompt)
    #[allow(dead_code)] // reserved for native function calling support
    pub fn tools_schema(&self) -> &[Tool] {
        &self.tools
    }

    /// Build a tools description string for prompting models that don't support native function calling
    pub fn tools_prompt(&self) -> String {
        let mut prompt = String::from("Available tools:\n\n");
        for tool in &self.tools {
            prompt.push_str(&format!(
                "- {}: {}\n  Parameters: {}\n\n",
                tool.name,
                tool.description,
                serde_json::to_string_pretty(&tool.parameters).unwrap_or_default()
            ));
        }
        prompt.push_str(
            "To use a tool, respond with a JSON block:\n```tool\n{\"tool\": \"<name>\", \"args\": {...}}\n```\n"
        );
        prompt
    }

    /// Execute a tool by name with the given arguments
    pub fn execute(&self, name: &str, args: serde_json::Value) -> ToolResult {
        match self.handlers.get(name) {
            Some(handler) => match handler(args) {
                Ok(output) => ToolResult {
                    tool: name.to_string(),
                    success: true,
                    output,
                },
                Err(e) => ToolResult {
                    tool: name.to_string(),
                    success: false,
                    output: e.to_string(),
                },
            },
            None => ToolResult {
                tool: name.to_string(),
                success: false,
                output: format!("Unknown tool: {name}"),
            },
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_has_tools() {
        let registry = ToolRegistry::new();
        assert!(registry.tools.len() >= 20, "expected >=20 tools, got {}", registry.tools.len());
    }

    #[test]
    fn test_pwd_tool() {
        let registry = ToolRegistry::new();
        let result = registry.execute("pwd", serde_json::json!({}));
        assert!(result.success, "pwd failed: {}", result.output);
    }

    #[test]
    fn test_unknown_tool() {
        let registry = ToolRegistry::new();
        let result = registry.execute("nope_nope", serde_json::json!({}));
        assert!(!result.success);
        assert!(result.output.contains("Unknown tool"));
    }

    #[test]
    fn test_all_tools_have_descriptions() {
        let registry = ToolRegistry::new();
        for tool in &registry.tools {
            assert!(!tool.description.is_empty(), "tool '{}' has no description", tool.name);
        }
    }

    #[test]
    fn test_git_branch_handles_no_repo() {
        let registry = ToolRegistry::new();
        let result = registry.execute("git_branch", serde_json::json!({}));
        // Either succeeds (in a repo) or fails with git error
        if !result.success {
            assert!(result.output.contains("git"), "expected git error, got: {}", result.output);
        }
    }

    #[test]
    fn test_list_dir_works() {
        let registry = ToolRegistry::new();
        let result = registry.execute("list_dir", serde_json::json!({}));
        assert!(result.success, "list_dir failed: {}", result.output);
    }
}