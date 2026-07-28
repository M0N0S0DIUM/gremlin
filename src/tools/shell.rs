/// Shell tools — fish history, terminal cwd detection.
/// Gracefully handles: fish not installed, not in a terminal, etc.

use crate::error::GremlinError;

/// Get recent commands from fish history.
/// Returns the last N commands (default 20, max 100).
pub fn recent_commands(limit: usize) -> Result<String, GremlinError> {
    let limit = limit.min(100).max(1);
    
    let output = std::process::Command::new("fish")
        .args(["-c", &format!("history | tail -n {limit}")])
        .output()
        .map_err(|e| GremlinError::Tool(format!("fish not available: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GremlinError::Tool(format!("fish error: {stderr}")));
    }

    let history = String::from_utf8_lossy(&output.stdout).trim().to_string();
    
    if history.is_empty() {
        Ok("(no fish history available)".to_string())
    } else {
        Ok(history)
    }
}

/// Detect the current working directory of the active Kitty terminal.
/// Uses `kitty @ ls` to find the focused tab's cwd.
/// Falls back to the current process cwd if Kitty isn't available.
pub fn kitty_cwd() -> Result<String, GremlinError> {
    // Try kitty remote control
    let output = std::process::Command::new("kitty")
        .args(["@", "ls"])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let json: serde_json::Value = serde_json::from_slice(&out.stdout)
                .map_err(|e| GremlinError::Tool(format!("kitty JSON parse error: {e}")))?;
            
            // Find the focused tab and extract cwd
            if let Some(tabs) = json.as_array() {
                for os_window in tabs {
                    if let Some(tabs_list) = os_window["tabs"].as_array() {
                        for tab in tabs_list {
                            if tab["is_focused"].as_bool() == Some(true) {
                                if let Some(cwd) = tab["cwd"].as_str() {
                                    return Ok(cwd.to_string());
                                }
                            }
                        }
                    }
                }
            }
            
            Err(GremlinError::Tool("Could not find focused Kitty tab cwd".into()))
        }
        _ => {
            // Fall back to process cwd
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .map_err(|e| GremlinError::Tool(format!("cwd detection failed: {e}")))
        }
    }
}

/// Get the last command's exit code and any stderr output.
/// This is a snapshot tool — call it after a command fails to capture context.
pub fn last_exit_code() -> Result<String, GremlinError> {
    let output = std::process::Command::new("fish")
        .args(["-c", "echo $status"])
        .output()
        .map_err(|_| GremlinError::Tool("fish not available".into()))?;

    if output.status.success() {
        let code = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(format!("Last exit code: {code}"))
    } else {
        Ok("(could not determine last exit code)".to_string())
    }
}