use crate::error::GremlinError;

/// Get the current working directory
pub fn pwd() -> Result<String, GremlinError> {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .map_err(|e| GremlinError::Tool(format!("pwd failed: {e}")))
}

/// List files in a directory
pub fn list_dir(path: Option<&str>) -> Result<String, GremlinError> {
    let dir_path = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_dir()?,
    };

    let entries: Vec<String> = std::fs::read_dir(&dir_path)?
        .filter_map(|entry| {
            entry.ok().map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    format!("{name}/")
                } else {
                    name
                }
            })
        })
        .collect();

    if entries.is_empty() {
        Ok("(empty directory)".to_string())
    } else {
        Ok(entries.join("\n"))
    }
}