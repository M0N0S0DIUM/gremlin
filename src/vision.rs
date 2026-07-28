/// Vision model integration — screenshot capture + Ollama vision API.
///
/// Screenshot: uses `grim` on wlroots/Hyprland, falls back to `import` (ImageMagick).
/// Vision: sends base64-encoded image to Ollama's /api/generate endpoint.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Deserialize;
use std::process::Stdio;
use tracing::{debug, info};

use crate::error::GremlinError;

/// Capture a screenshot and return the raw PNG bytes.
pub fn capture_screenshot() -> Result<Vec<u8>, GremlinError> {
    // Try grim first (wlroots/Hyprland native)
    if let Ok(output) = std::process::Command::new("grim")
        .arg("-t")
        .arg("png")
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        if output.status.success() {
            debug!("Screenshot captured via grim ({} bytes)", output.stdout.len());
            return Ok(output.stdout);
        }
    }

    // Fall back to ImageMagick import
    if let Ok(output) = std::process::Command::new("import")
        .arg("-window")
        .arg("root")
        .arg("png:-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        if output.status.success() {
            debug!("Screenshot captured via import ({} bytes)", output.stdout.len());
            return Ok(output.stdout);
        }
    }

    Err(GremlinError::Tool(
        "No screenshot tool available. Install grim (wlroots) or ImageMagick.".into(),
    ))
}

/// Send an image to Ollama's vision model and get a description.
pub async fn describe_screenshot(
    ollama_url: &str,
    vision_model: &str,
    image_bytes: &[u8],
    prompt: &str,
) -> Result<String, GremlinError> {
    let base64_image = BASE64.encode(image_bytes);

    let body = serde_json::json!({
        "model": vision_model,
        "prompt": prompt,
        "images": [base64_image],
        "stream": false,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{ollama_url}/api/generate"))
        .json(&body)
        .send()
        .await
        .map_err(|e| GremlinError::Tool(format!("Vision model request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(GremlinError::Tool(format!(
            "Vision model returned {status}: {text}"
        )));
    }

    #[derive(Deserialize)]
    struct GenerateResponse {
        response: String,
    }

    let gen_resp: GenerateResponse = resp
        .json()
        .await
        .map_err(|e| GremlinError::Tool(format!("Vision model response parse error: {e}")))?;

    info!(
        response_len = gen_resp.response.len(),
        "Vision model responded"
    );

    Ok(gen_resp.response)
}

/// Quick check: is a vision model available in Ollama?
pub async fn vision_model_available(ollama_url: &str, model: &str) -> bool {
    let client = reqwest::Client::new();
    if let Ok(resp) = client
        .get(format!("{ollama_url}/api/tags"))
        .send()
        .await
    {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            if let Some(models) = json["models"].as_array() {
                return models
                    .iter()
                    .any(|m| m["name"].as_str() == Some(model));
            }
        }
    }
    false
}