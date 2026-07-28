/// Desktop awareness module — Hyprland IPC via hyprctl subprocess.
/// Falls back gracefully when not on Hyprland or when hyprctl is unavailable.

use serde::Deserialize;

use crate::error::GremlinError;

/// Run `hyprctl -j <args...>` and parse JSON output.
fn hyprctl_json<T: for<'de> Deserialize<'de>>(args: &[&str]) -> Result<T, GremlinError> {
    let output = std::process::Command::new("hyprctl")
        .arg("-j")
        .args(args)
        .output()
        .map_err(|e| GremlinError::Tool(format!("hyprctl not available: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GremlinError::Tool(format!("hyprctl error: {stderr}")));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|e| GremlinError::Tool(format!("hyprctl JSON parse error: {e}")))
}

/// Run `hyprctl <args...>` and return raw stdout (no -j flag).
fn hyprctl_raw(args: &[&str]) -> Result<String, GremlinError> {
    let output = std::process::Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|e| GremlinError::Tool(format!("hyprctl not available: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GremlinError::Tool(format!("hyprctl error: {stderr}")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ── JSON types for hyprctl output ──

#[derive(Debug, Deserialize)]
pub struct WindowInfo {
    pub title: String,
    pub class: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub workspace: WorkspaceRef,
    #[serde(default)]
    pub pid: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct WorkspaceRef {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceInfo {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub windows: i64,
    #[serde(default)]
    pub monitor: String,
}

#[derive(Debug, Deserialize)]
pub struct MonitorInfo {
    pub id: i64,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub refresh_rate: f64,
    #[serde(default)]
    pub x: i64,
    #[serde(default)]
    pub y: i64,
    #[serde(default)]
    pub active_workspace: WorkspaceRef,
}

// ── Public API ──

/// Get info about the currently focused window.
pub fn active_window() -> Result<WindowInfo, GremlinError> {
    hyprctl_json(&["activewindow"])
}

/// Get the active workspace ID and name.
pub fn active_workspace() -> Result<(i64, String), GremlinError> {
    let ws: WorkspaceInfo = hyprctl_json(&["activeworkspace"])?;
    Ok((ws.id, ws.name))
}

/// List all workspaces with window counts.
pub fn list_workspaces() -> Result<Vec<WorkspaceInfo>, GremlinError> {
    hyprctl_json(&["workspaces"])
}

/// List all monitors.
pub fn list_monitors() -> Result<Vec<MonitorInfo>, GremlinError> {
    hyprctl_json(&["monitors"])
}

/// Get the focused window title as a plain string (for tool output).
pub fn focused_title() -> Result<String, GremlinError> {
    let win = active_window()?;
    Ok(format!("{} — {}", win.class, win.title))
}

/// Get a summary of the current desktop state (for context injection).
pub fn desktop_summary() -> Result<String, GremlinError> {
    let mut parts = Vec::new();

    // Active workspace
    match active_workspace() {
        Ok((id, name)) => parts.push(format!("Workspace: {name} (id={id})")),
        Err(_) => {} // Not on Hyprland — skip
    }

    // Focused window
    match active_window() {
        Ok(win) => parts.push(format!("Focused: {} — {}", win.class, win.title)),
        Err(_) => {}
    }

    // Monitors
    match list_monitors() {
        Ok(monitors) if !monitors.is_empty() => {
            let descs: Vec<String> = monitors
                .iter()
                .map(|m| format!("  {}: {} {}x{}@{}Hz", m.name, m.description, m.width, m.height, m.refresh_rate))
                .collect();
            parts.push(format!("Monitors:\n{}", descs.join("\n")));
        }
        _ => {}
    }

    if parts.is_empty() {
        Ok(String::new())
    } else {
        Ok(parts.join("\n"))
    }
}