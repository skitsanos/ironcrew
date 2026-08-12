use base64::Engine;
use std::sync::LazyLock;
use std::time::Duration;

use crate::utils::error::{IronCrewError, Result};

use super::provider::ImageInput;

static REMOTE_IMAGE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    crate::utils::network::secure_client_builder(
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
    )
    .timeout(Duration::from_secs(30))
    .pool_max_idle_per_host(2)
    .user_agent(format!("IronCrew/{}", env!("CARGO_PKG_VERSION")))
    .build()
    .expect("failed to build remote image HTTP client")
});

/// Load an image from a local file path or URL, returning base64-encoded
/// data with the detected MIME type.
pub async fn load_image(
    path_or_url: &str,
    project_dir: &std::path::Path,
    client: &reqwest::Client,
) -> Result<ImageInput> {
    let limit = crate::utils::http::byte_limit_from_env(
        "IRONCREW_MAX_IMAGE_BYTES",
        crate::utils::http::DEFAULT_IMAGE_BYTES,
    );
    load_image_with_limit(path_or_url, project_dir, client, limit).await
}

/// Load an image with a caller-supplied remaining byte budget. The configured
/// per-image cap still applies, so this can safely enforce an aggregate budget
/// without allowing one image to exceed the process policy.
pub async fn load_image_with_limit(
    path_or_url: &str,
    project_dir: &std::path::Path,
    _client: &reqwest::Client,
    max_bytes: usize,
) -> Result<ImageInput> {
    let configured_limit = crate::utils::http::byte_limit_from_env(
        "IRONCREW_MAX_IMAGE_BYTES",
        crate::utils::http::DEFAULT_IMAGE_BYTES,
    );
    let error_limit = crate::utils::http::byte_limit_from_env(
        "IRONCREW_PROVIDER_MAX_ERROR_BYTES",
        crate::utils::http::DEFAULT_PROVIDER_ERROR_BYTES,
    );
    load_image_with_policy(
        path_or_url,
        project_dir,
        max_bytes,
        configured_limit,
        error_limit,
    )
    .await
}

pub(crate) async fn load_image_with_policy(
    path_or_url: &str,
    project_dir: &std::path::Path,
    max_bytes: usize,
    configured_limit: usize,
    error_limit: usize,
) -> Result<ImageInput> {
    let limit = max_bytes.min(configured_limit);
    if limit == 0 {
        return Err(IronCrewError::Validation(
            "Image byte budget is exhausted".into(),
        ));
    }

    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        load_image_from_url(path_or_url, limit, error_limit).await
    } else {
        load_image_from_file(path_or_url, project_dir, limit)
    }
}

fn load_image_from_file(
    path: &str,
    project_dir: &std::path::Path,
    max_bytes: usize,
) -> Result<ImageInput> {
    // Resolve and open relative to an already-open capability directory. This
    // removes the canonicalize-then-open symlink swap window and rejects
    // absolute paths, traversal, non-regular files, and growth past the cap.
    let relative_path = std::path::Path::new(path);
    let root = crate::tools::project_fs::open_root(Some(project_dir)).map_err(|error| {
        IronCrewError::Validation(format!("Invalid project directory: {error}"))
    })?;
    let bytes = crate::tools::project_fs::read_bytes_bounded(&root, relative_path, max_bytes)
        .map_err(|error| {
            IronCrewError::Validation(format!("Failed to read image '{path}': {error}"))
        })?;

    let mime_type = mime_from_extension(relative_path)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(ImageInput { mime_type, data })
}

async fn load_image_from_url(
    url: &str,
    max_bytes: usize,
    error_limit: usize,
) -> Result<ImageInput> {
    // SSRF protection: a remote image URL is attacker-influenced input, so it
    // must not be able to reach cloud metadata or internal services.
    crate::utils::network::validate_url_not_private(url).map_err(IronCrewError::Validation)?;

    // Always use the resolver-pinned client here. Accepting an arbitrary
    // reqwest client would reintroduce a resolve-then-connect SSRF race.
    let response = REMOTE_IMAGE_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|e| IronCrewError::Validation(format!("Failed to download image: {}", e)))?;

    decode_remote_image_response(response, max_bytes, error_limit).await
}

async fn decode_remote_image_response(
    response: reqwest::Response,
    max_bytes: usize,
    error_limit: usize,
) -> Result<ImageInput> {
    let status = response.status();
    if !status.is_success() {
        let body = crate::utils::http::read_response_bytes(
            response,
            error_limit,
            "remote image error response",
        )
        .await
        .map_err(|error| IronCrewError::Validation(error.to_string()))?;
        let message = String::from_utf8_lossy(&body);
        return Err(IronCrewError::Validation(format!(
            "Image download returned HTTP {status}: {}",
            crate::utils::http::utf8_prefix(message.trim(), 512)
        )));
    }

    let mime_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .ok_or_else(|| IronCrewError::Validation("Image response is missing Content-Type".into()))?
        .to_str()
        .map_err(|_| IronCrewError::Validation("Image Content-Type is not valid ASCII".into()))?
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        mime_type.as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return Err(IronCrewError::Validation(format!(
            "Unsupported remote image Content-Type '{mime_type}' (supported: image/jpeg, image/png, image/gif, image/webp)"
        )));
    }

    let bytes = crate::utils::http::read_response_bytes(response, max_bytes, "remote image")
        .await
        .map_err(|error| IronCrewError::Validation(error.to_string()))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response_from(server_response: &'static [u8]) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind image test server");
        let address = listener.local_addr().expect("image test address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept image request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(server_response)
                .await
                .expect("write image response");
        });
        reqwest::get(format!("http://{address}/"))
            .await
            .expect("fetch image test response")
    }

    #[tokio::test]
    async fn remote_images_require_success_status() {
        let response = response_from(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: image/png\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing",
        )
        .await;
        let error = decode_remote_image_response(response, 1024, 1024)
            .await
            .expect_err("error status must fail");
        assert!(error.to_string().contains("HTTP 404"));
    }

    #[tokio::test]
    async fn remote_images_require_supported_mime_type() {
        let response = response_from(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops",
        )
        .await;
        let error = decode_remote_image_response(response, 1024, 1024)
            .await
            .expect_err("unsupported MIME must fail");
        assert!(
            error
                .to_string()
                .contains("Unsupported remote image Content-Type")
        );
    }

    #[tokio::test]
    async fn remote_images_enforce_chunked_limit_before_base64() {
        let response = response_from(
            b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .await;
        let error = decode_remote_image_response(response, 8, 1024)
            .await
            .expect_err("oversized chunked image must fail");
        assert!(error.to_string().contains("8-byte limit"));
    }
}
