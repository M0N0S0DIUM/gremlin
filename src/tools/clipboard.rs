/// Clipboard tools — read/write the Wayland/X11 clipboard.
/// Uses `wl-paste` / `wl-copy` on Wayland, `xclip` on X11.

use crate::error::GremlinError;

/// Get the current clipboard contents as text.
pub fn read() -> Result<String, GremlinError> {
    // Try wl-paste first (Wayland), fall back to xclip (X11)
    let cmd = if command_exists("wl-paste") {
        "wl-paste"
    } else if command_exists("xclip") {
        "xclip -selection clipboard -o"
    } else {
        return Err(GremlinError::Tool(
            "No clipboard tool available. Install wl-clipboard (Wayland) or xclip (X11).".into(),
        ));
    };

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| GremlinError::Tool(format!("clipboard read failed: {e}")))?;

    if !output.status.success() {
        return Err(GremlinError::Tool("clipboard read returned non-zero".into()));
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        Ok("(clipboard is empty)".to_string())
    } else {
        // Truncate very large clipboard contents
        if text.len() > 10_000 {
            let truncated: String = text.chars().take(10_000).collect();
            return Ok(format!("{truncated}\n\n[... clipboard truncated at 10KB ...]"));
        }
        Ok(text)
    }
}

/// Write text to the clipboard.
pub fn write(text: &str) -> Result<(), GremlinError> {
    let cmd = if command_exists("wl-copy") {
        "wl-copy".to_string()
    } else if command_exists("xclip") {
        "xclip -selection clipboard".to_string()
    } else {
        return Err(GremlinError::Tool(
            "No clipboard tool available. Install wl-clipboard (Wayland) or xclip (X11).".into(),
        ));
    };

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| GremlinError::Tool(format!("clipboard write failed: {e}")))?;

    use std::io::Write;
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(text.as_bytes())?;
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(GremlinError::Tool("clipboard write returned non-zero".into()));
    }

    Ok(())
}

fn command_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}