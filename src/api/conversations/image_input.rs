//! Bounded image preparation for HTTP conversation turns.

use std::path::Path;

use axum::{Json, http::StatusCode};

use super::{ConversationHandle, map_err_to_response};
use crate::api::{ErrorResponse, error_response};
use crate::llm::provider::ImageInput;

fn decoded_base64_len(data: &str) -> usize {
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    data.len()
        .saturating_div(4)
        .saturating_mul(3)
        .saturating_sub(padding)
}

pub(super) async fn load_message_images(
    handle: &ConversationHandle,
    flow_path: &Path,
    paths: Option<Vec<String>>,
    shared_store: bool,
) -> Result<Option<Vec<ImageInput>>, (StatusCode, Json<ErrorResponse>)> {
    let Some(paths) = paths.filter(|paths| !paths.is_empty()) else {
        return Ok(None);
    };
    let policy = handle.conv.tool_registry.conversation_policy();
    let (history_image_count, history_image_bytes) = {
        let history = handle.conv.messages.lock().await;
        history
            .iter()
            .filter_map(|message| message.images.as_ref())
            .flatten()
            .fold((0usize, 0usize), |(count, bytes), image| {
                (
                    count.saturating_add(1),
                    bytes.saturating_add(decoded_base64_len(&image.data)),
                )
            })
    };
    let max_per_message = policy.images_per_message();
    let max_per_conversation = policy.images_per_conversation();
    if paths.len() > max_per_message
        || history_image_count.saturating_add(paths.len()) > max_per_conversation
    {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "Image count exceeds limits ({max_per_message} per message, {max_per_conversation} per conversation)"
            ),
        ));
    }
    validate_image_locators(&paths, policy.image_locator_bytes(), shared_store)?;

    let conversation_remaining = policy
        .image_bytes_per_conversation()
        .saturating_sub(history_image_bytes);
    let mut remaining = policy.image_bytes_per_message().min(conversation_remaining);
    if remaining == 0 {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Conversation image-byte limit reached".into(),
        ));
    }

    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let image = crate::llm::image::load_image_with_policy(
            &path,
            flow_path,
            remaining,
            policy.image_bytes(),
            policy.image_error_bytes(),
        )
        .await
        .map_err(|error| map_err_to_response(&error))?;
        let decoded_bytes = decoded_base64_len(&image.data);
        if decoded_bytes > remaining {
            return Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Aggregate image-byte limit exceeded".into(),
            ));
        }
        remaining -= decoded_bytes;
        loaded.push(image);
    }
    Ok(Some(loaded))
}

fn validate_image_locators(
    paths: &[String],
    max_locator_bytes: usize,
    shared_store: bool,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if let Some(path) = paths
        .iter()
        .find(|path| path.is_empty() || path.len() > max_locator_bytes)
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "Image locator must be 1..={max_locator_bytes} bytes (received {})",
                path.len()
            ),
        ));
    }
    if shared_store
        && paths
            .iter()
            .any(|path| !(path.starts_with("http://") || path.starts_with("https://")))
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "PostgreSQL conversation images require public HTTP(S) locators; project-local paths are process-local"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_store_rejects_process_local_paths() {
        let local = vec!["images/chart.png".to_string()];
        assert!(validate_image_locators(&local, 2_048, false).is_ok());
        let error = validate_image_locators(&local, 2_048, true).unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.0.error.contains("public HTTP(S)"));

        let remote = vec!["https://example.test/chart.png".to_string()];
        assert!(validate_image_locators(&remote, 2_048, true).is_ok());
    }
}
