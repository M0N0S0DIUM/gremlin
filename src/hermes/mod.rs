/// Hermes coding service integration.
/// Gremlin treats Hermes as an external service — it shells out to `hermes ask`
/// with a well-constructed prompt that includes automatic context.
///
/// Architecture:
///   Gremlin → hermes ask "..." → Hermes → Qwen Coder 80B (or configured model)
///
/// Gremlin never knows which model Hermes uses internally. It only needs
/// the Hermes CLI interface.

use std::process::Stdio;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::HermesConfig;
use crate::context::Context;
use crate::error::GremlinError;

/// Maximum time to wait for Hermes to respond (5 minutes for complex coding tasks).
const HERMES_TIMEOUT_SECS: u64 = 300;

/// Result of a Hermes invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HermesResult {
    pub success: bool,
    pub output: String,
    pub duration_secs: f64,
}

/// Build a coding prompt for Hermes. This bundles:
/// - The template/system prompt for the coding task
/// - Auto-gathered context (project, git branch, recent errors, etc.)
/// - The user's specific request
pub fn build_prompt(
    template: &PromptTemplate,
    context: &Context,
    user_request: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Template header
    parts.push(template.system_prompt.clone());

    // Context
    let ctx_str = context.to_prompt_string();
    if !ctx_str.is_empty() {
        parts.push(format!(
            "Project context (auto-gathered, do not ask the user for this):\n{ctx_str}"
        ));
    }

    // Template-specific instructions
    if let Some(ref instructions) = template.instructions {
        parts.push(instructions.clone());
    }

    // User's request
    parts.push(format!("Task:\n{user_request}"));

    // Output formatting hint from template
    if let Some(tag) = &template.output_tag {
        parts.push(format!(
            "Format your response as:\n```{tag}\n(your changes here)\n```"
        ));
    }

    parts.join("\n\n")
}

/// Launch Hermes with a prompt and wait for the response.
/// This is a blocking call — Hermes may take minutes for complex coding tasks.
pub async fn launch(
    config: &HermesConfig,
    template: &PromptTemplate,
    context: &Context,
    user_request: &str,
) -> Result<HermesResult, GremlinError> {
    let prompt = build_prompt(template, context, user_request);
    let start = std::time::Instant::now();

    info!(
        template = %template.name,
        prompt_len = prompt.len(),
        "Launching Hermes"
    );
    debug!(prompt = %prompt, "Hermes prompt");

    // Build the hermes command
    let binary = &config.binary;
    let mut cmd = std::process::Command::new(binary);

    cmd.arg("ask")
        .arg(&prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // If Hermes supports a model flag, pass the coding model
    // (Hermes CLI may or may not support this — we try gracefully)
    cmd.env("HERMES_MODEL", &config.coding_model);

    // Wait for Hermes to finish
    #[allow(unused_mut)]
    let mut child = cmd.spawn().map_err(|e| {
        GremlinError::Tool(format!("Failed to launch Hermes ({binary}): {e}"))
    })?;

    // Wait with timeout
    let output = tokio::task::spawn_blocking(move || {
        child.wait_with_output()
    });

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(HERMES_TIMEOUT_SECS),
        output,
    )
    .await
    .map_err(|_| GremlinError::Tool(format!(
        "Hermes timed out after {} seconds",
        HERMES_TIMEOUT_SECS
    )))?
    .map_err(|e| GremlinError::Tool(format!("Hermes task panicked: {e}")))?
    .map_err(|e| GremlinError::Tool(format!("Hermes process error: {e}")))?;

    let duration = start.elapsed().as_secs_f64();

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        info!(duration = %format!("{duration:.1}s"), "Hermes completed");

        // Truncate very long responses
        let stdout = if stdout.len() > 30_000 {
            let truncated: String = stdout.chars().take(30_000).collect();
            format!("{truncated}\n\n[... response truncated at 30KB ...]")
        } else {
            stdout
        };

        Ok(HermesResult {
            success: true,
            output: stdout,
            duration_secs: duration,
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let error_msg = if !stderr.is_empty() { stderr } else { stdout };

        warn!(duration = %format!("{duration:.1}s"), error = %error_msg, "Hermes failed");

        Ok(HermesResult {
            success: false,
            output: format!("Hermes exited with error:\n{error_msg}"),
            duration_secs: duration,
        })
    }
}

// ── Prompt Templates ──

/// A reusable prompt template. Gremlin fills these with context automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplate {
    /// Short name for the tool registry (e.g. "code_review", "bug_fix")
    pub name: String,
    /// Human-readable description shown in tool list
    pub description: String,
    /// System prompt / instructions prepended to the user's request
    pub system_prompt: String,
    /// Optional additional instructions (appended after context)
    #[serde(default)]
    pub instructions: Option<String>,
    /// Optional output format tag (e.g. "diff", "rust", "python")
    #[serde(default)]
    pub output_tag: Option<String>,
    /// When true, include a full git diff in the context
    #[serde(default)]
    pub include_diff: bool,
}

/// Built-in prompt templates. Users can add more in ~/.config/gremlin/prompts/*.toml
pub fn builtin_templates() -> Vec<PromptTemplate> {
    vec![
        PromptTemplate {
            name: "code_review".into(),
            description: "Review code for bugs, security issues, performance problems, and style violations".into(),
            system_prompt: "You are performing a thorough code review. Examine the code for:\n\
                - Bugs and logic errors\n\
                - Security vulnerabilities\n\
                - Performance issues\n\
                - Adherence to the project's existing style and patterns\n\
                - Unnecessary complexity\n\n\
                Be specific. Reference exact lines. Suggest concrete fixes, not vague advice.\n\
                If the code is good, say so — don't invent issues.".into(),
            instructions: None,
            output_tag: None,
            include_diff: true,
        },
        PromptTemplate {
            name: "bug_fix".into(),
            description: "Fix a specific bug. Include error messages and reproduction steps in your request.".into(),
            system_prompt: "You are fixing a bug. Your job is to produce the minimal correct fix.\n\n\
                Rules:\n\
                - Change ONLY what's necessary to fix the bug\n\
                - Do not refactor unrelated code\n\
                - Do not add features\n\
                - Preserve existing code style and patterns\n\
                - If you're unsure, explain what additional information you'd need\n\n\
                Provide the fix as a unified diff or specific file edits.".into(),
            instructions: Some("If there's a compiler error or stack trace available, it will be in the context above. Use it to locate the exact problem.".into()),
            output_tag: Some("diff".into()),
            include_diff: true,
        },
        PromptTemplate {
            name: "architecture".into(),
            description: "Evaluate architecture decisions, design patterns, or system structure".into(),
            system_prompt: "You are evaluating the architecture of a software project.\n\n\
                Consider:\n\
                - Separation of concerns\n\
                - Data flow and coupling\n\
                - Testability and maintainability\n\
                - Performance implications of the current design\n\
                - How the architecture supports or hinders the project's goals\n\n\
                Be constructive, not critical for its own sake. If the architecture is appropriate, say why.\n\
                If you recommend changes, explain the tradeoffs.".into(),
            instructions: None,
            output_tag: None,
            include_diff: false,
        },
        PromptTemplate {
            name: "refactor".into(),
            description: "Refactor code for clarity, performance, or maintainability without changing behavior".into(),
            system_prompt: "You are refactoring existing code. Your changes must NOT alter behavior.\n\n\
                Goals (in priority order):\n\
                1. Correctness — the refactored code must behave identically\n\
                2. Clarity — make the code easier to understand\n\
                3. Performance — improve efficiency where it matters\n\
                4. Consistency — match the project's existing patterns\n\n\
                Provide the refactored code as a diff or complete file replacement.\n\
                Explain what you changed and why.".into(),
            instructions: Some("The existing code is in the context. Only suggest changes that preserve all existing tests and behavior.".into()),
            output_tag: Some("diff".into()),
            include_diff: true,
        },
        PromptTemplate {
            name: "explain".into(),
            description: "Explain how a piece of code or system works".into(),
            system_prompt: "You are explaining how code works to an experienced developer.\n\n\
                Be concise but thorough. Assume the reader understands the language and basic patterns.\n\
                Focus on:\n\
                - The high-level flow\n\
                - Non-obvious design decisions\n\
                - Edge cases and gotchas\n\
                - How this code fits into the larger system\n\n\
                Do not explain basic syntax. Do not write a line-by-line commentary.".into(),
            instructions: None,
            output_tag: None,
            include_diff: false,
        },
        PromptTemplate {
            name: "documentation".into(),
            description: "Generate or improve documentation for code".into(),
            system_prompt: "You are writing documentation for developers.\n\n\
                Produce documentation that is:\n\
                - Clear and concise\n\
                - Focused on usage, not implementation details (unless relevant)\n\
                - Well-structured with examples where appropriate\n\
                - Consistent with the project's existing documentation style\n\n\
                Include: purpose, usage examples, API reference, and any important caveats.".into(),
            instructions: None,
            output_tag: Some("markdown".into()),
            include_diff: false,
        },
    ]
}

/// Load a specific template by name from builtins. Falls back to generic template.
pub fn find_template(name: &str) -> Option<PromptTemplate> {
    builtin_templates()
        .into_iter()
        .find(|t| t.name == name)
}

/// List available template names for the tool description.
pub fn template_names() -> Vec<String> {
    builtin_templates()
        .iter()
        .map(|t| t.name.clone())
        .collect()
}