use crate::error::GremlinError;

/// Read a file and return its contents as a string
pub fn read_file(path: &str) -> Result<String, GremlinError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        GremlinError::Tool(format!("Failed to read {path}: {e}"))
    })?;

    // Truncate large files to avoid flooding context
    if content.len() > 50_000 {
        let truncated: String = content.chars().take(50_000).collect();
        return Ok(format!("{truncated}\n\n[... truncated at 50KB ...]"));
    }

    Ok(content)
}

/// Write content to a file, creating parent directories if needed
pub fn write_file(path: &str, content: &str) -> Result<(), GremlinError> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}