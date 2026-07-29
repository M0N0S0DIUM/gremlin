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
    pub desktop_summary: Option<String>,
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

        // Git diff (for context — key for coding workflows)
        let result = tools.execute("git_diff", serde_json::json!({"staged": false}));
        if result.success && !result.output.is_empty() {
            ctx.git_diff = Some(result.output);
        }

        // Desktop awareness — rich summary (monitors, workspaces, focused window)
        match crate::desktop::hyprland::desktop_summary() {
            Ok(summary) if !summary.is_empty() => {
                ctx.desktop_summary = Some(summary);
            }
            _ => {}
        }

        // Desktop awareness — active window (individual, for targeted context)
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
        if let Some(ref diff) = self.git_diff {
            // Truncate very large diffs in the prompt
            let truncated = if diff.len() > 4000 {
                format!("{}...\n(truncated, {} chars total)", &diff[..4000], diff.len())
            } else {
                diff.clone()
            };
            parts.push(format!("Git diff (unstaged):\n{truncated}"));
        }
        if let Some(ref project) = self.current_project {
            parts.push(format!("Current project: {project}"));
        }
        // Desktop summary — richest context first
        if let Some(ref summary) = self.desktop_summary {
            parts.push(format!("Desktop:\n{summary}"));
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