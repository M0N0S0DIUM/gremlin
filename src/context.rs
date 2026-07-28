use crate::tools::ToolRegistry;

/// Context automatically gathered before dispatching a prompt.
/// Gremlin collects this without the user having to copy-paste anything.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub working_directory: Option<String>,
    pub git_branch: Option<String>,
    pub git_status: Option<String>,
    pub git_diff: Option<String>,
    pub current_project: Option<String>,

    // Desktop awareness (Hyprland — gracefully absent when not available)
    pub active_window: Option<String>,
    pub active_workspace: Option<String>,
}

impl Context {
    /// Collect all available context from the current environment
    pub fn collect(tools: &ToolRegistry) -> Self {
        let mut ctx = Context::default();

        // Working directory
        let result = tools.execute("pwd", serde_json::json!({}));
        if result.success {
            ctx.working_directory = Some(result.output);
        }

        // Git branch
        let result = tools.execute("git_branch", serde_json::json!({}));
        if result.success {
            ctx.git_branch = Some(result.output);
        }

        // Git status (short)
        let result = tools.execute("git_status", serde_json::json!({}));
        if result.success && result.output != "(clean working tree)" {
            ctx.git_status = Some(result.output);
        }

        // Desktop awareness — active window
        let result = tools.execute("active_window", serde_json::json!({}));
        if result.success {
            ctx.active_window = Some(result.output);
        }

        // Desktop awareness — active workspace
        let result = tools.execute("active_workspace", serde_json::json!({}));
        if result.success {
            ctx.active_workspace = Some(result.output);
        }

        ctx
    }

    /// Build a context string to prepend to the user's prompt
    pub fn to_prompt_string(&self) -> String {
        let mut parts = Vec::new();

        if let Some(ref dir) = self.working_directory {
            parts.push(format!("Working directory: {dir}"));
        }
        if let Some(ref branch) = self.git_branch {
            parts.push(format!("Git branch: {branch}"));
        }
        if let Some(ref status) = self.git_status {
            parts.push(format!("Git status:\n{status}"));
        }
        if let Some(ref project) = self.current_project {
            parts.push(format!("Current project: {project}"));
        }
        if let Some(ref win) = self.active_window {
            parts.push(format!("Active window: {win}"));
        }
        if let Some(ref ws) = self.active_workspace {
            parts.push(format!("{ws}"));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!("System context:\n{}\n\n", parts.join("\n"))
        }
    }
}