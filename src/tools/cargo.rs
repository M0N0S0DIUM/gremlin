/// Cargo build/test tools — capture compiler output for context injection.
/// Gracefully handles: not in a Rust project, cargo not installed, build failures.

use crate::error::GremlinError;

/// Run `cargo build` and capture stdout+stderr.
/// Returns compiler output or a clear error if cargo isn't available.
pub fn build() -> Result<String, GremlinError> {
    run_cargo(&["build"], "build")
}

/// Run `cargo check` (faster than build, same errors).
pub fn check() -> Result<String, GremlinError> {
    run_cargo(&["check"], "check")
}

/// Run `cargo test` and capture output.
pub fn test() -> Result<String, GremlinError> {
    run_cargo(&["test"], "test")
}

/// Run `cargo build --release`.
pub fn build_release() -> Result<String, GremlinError> {
    run_cargo(&["build", "--release"], "build --release")
}

fn run_cargo(args: &[&str], label: &str) -> Result<String, GremlinError> {
    let output = std::process::Command::new("cargo")
        .args(args)
        .output()
        .map_err(|e| GremlinError::Tool(format!("cargo not available: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = String::new();

    if output.status.success() {
        result.push_str(&format!("cargo {label}: SUCCESS\n"));
    } else {
        result.push_str(&format!("cargo {label}: FAILED\n"));
    }

    // Extract the important bits: error lines, warning counts
    let combined = format!("{stdout}{stderr}");
    
    // Truncate but keep the most relevant parts (errors at the end)
    if combined.len() > 20_000 {
        // Split into lines, keep first 50 (warnings) and last 100 (errors)
        let lines: Vec<&str> = combined.lines().collect();
        let total = lines.len();
        
        if total <= 200 {
            result.push_str(&combined);
        } else {
            let head: Vec<&str> = lines.iter().take(50).copied().collect();
            let tail: Vec<&str> = lines.iter().rev().take(150).copied().collect::<Vec<_>>().into_iter().rev().collect();
            
            result.push_str(&head.join("\n"));
            result.push_str(&format!("\n\n... {} lines omitted ...\n\n", total - 200));
            result.push_str(&tail.join("\n"));
        }
    } else {
        result.push_str(&combined);
    }

    Ok(result)
}