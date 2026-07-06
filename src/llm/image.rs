use base64::Engine;

use crate::utils::error::{IronCrewError, Result};

use super::provider::ImageInput;

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Load an image from a local file path or URL, returning base64-encoded
/// data with the detected MIME type.
pub async fn load_image(
    path_or_url: &str,
    project_dir: &std::path::Path,
    client: &reqwest::Client,
) -> Result<ImageInput> {
    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        load_image_from_url(path_or_url, client).await
    } else {
        load_image_from_file(path_or_url, project_dir)
    }
}

fn load_image_from_file(path: &str, project_dir: &std::path::Path) -> Result<ImageInput> {
    // Containment: resolve `..`/symlinks and require the result to stay within
    // the project directory. Without this, `project_dir.join(path)` with an
    // absolute path replaces the base entirely, and `../` walks out — letting a
    // request read any image-extension file on the host and exfiltrate it.
    let full_path = project_dir.join(path);
    let canonical_base = project_dir
        .canonicalize()
        .map_err(|e| IronCrewError::Validation(format!("Invalid project directory: {}", e)))?;
    let canonical_path = full_path.canonicalize().map_err(|_| {
        IronCrewError::Validation(format!("Image file not found: {}", full_path.display()))
    })?;
    if !canonical_path.starts_with(&canonical_base) {
        return Err(IronCrewError::Validation(format!(
            "Image path escapes project directory: {}",
            path
        )));
    }

    let bytes = std::fs::read(&canonical_path)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(IronCrewError::Validation(format!(
            "Image too large: {} bytes (max {})",
            bytes.len(),
            MAX_IMAGE_BYTES
        )));
    }

    let mime_type = mime_from_extension(&canonical_path)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ImageInput { mime_type, data })
}

async fn load_image_from_url(url: &str, client: &reqwest::Client) -> Result<ImageInput> {
    // SSRF protection: a remote image URL is attacker-influenced input, so it
    // must not be able to reach cloud metadata or internal services.
    crate::utils::network::validate_url_not_private(url).map_err(IronCrewError::Validation)?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to download image: {}", e)))?;

    let mime_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .split(';')
        .next()
        .unwrap_or("image/jpeg")
        .to_string();

    let bytes = response
        .bytes()
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to read image body: {}", e)))?;

    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(IronCrewError::Validation(format!(
            "Image too large: {} bytes (max {})",
            bytes.len(),
            MAX_IMAGE_BYTES
        )));
    }

    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ImageInput { mime_type, data })
}

fn mime_from_extension(path: &std::path::Path) -> Result<String> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => Ok("image/jpeg".into()),
        Some("png") => Ok("image/png".into()),
        Some("gif") => Ok("image/gif".into()),
        Some("webp") => Ok("image/webp".into()),
        Some(ext) => Err(IronCrewError::Validation(format!(
            "Unsupported image format: .{} (supported: jpg, png, gif, webp)",
            ext
        ))),
        None => Err(IronCrewError::Validation(format!(
            "Cannot detect image format: {} (no file extension)",
            path.display()
        ))),
    }
}
