use crate::error::GremlinError;

/// Run a git command and return stdout
fn git(args: &[&str]) -> Result<String, GremlinError> {
    let output = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|e| GremlinError::Tool(format!("git not available: {e}")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(GremlinError::Tool(format!("git error: {stderr}")))
    }
}

/// Get `git status --short`
pub fn status() -> Result<String, GremlinError> {
    let out = git(&["status", "--short"])?;
    if out.is_empty() {
        Ok("(clean working tree)".to_string())
    } else {
        Ok(out)
    }
}

/// Get `git diff`
pub fn diff(staged: bool) -> Result<String, GremlinError> {
    let mut args = vec!["diff"];
    if staged {
        args.push("--staged");
    }
    let out = git(&args)?;
    if out.is_empty() {
        Ok("(no changes)".to_string())
    } else {
        // Truncate large diffs
        if out.len() > 30_000 {
            let truncated: String = out.chars().take(30_000).collect();
            return Ok(format!("{truncated}\n\n[... diff truncated at 30KB ...]"));
        }
        Ok(out)
    }
}

/// Get current branch name
pub fn current_branch() -> Result<String, GremlinError> {
    git(&["rev-parse", "--abbrev-ref", "HEAD"])
}