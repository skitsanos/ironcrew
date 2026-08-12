//! Captured limits for one HTTP conversation runtime.

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConversationTurnPolicy {
    turn_timeout_secs: u64,
    message_bytes: usize,
    images_per_message: usize,
    images_per_conversation: usize,
    image_bytes_per_message: usize,
    image_bytes_per_conversation: usize,
    image_locator_bytes: usize,
    image_bytes: usize,
    image_error_bytes: usize,
}

impl ConversationTurnPolicy {
    pub(crate) fn capture() -> Self {
        Self {
            turn_timeout_secs: bounded("IRONCREW_MAX_CONVERSATION_TURN_SECS", 300, 3_600) as u64,
            message_bytes: bounded(
                "IRONCREW_API_MESSAGE_MAX_BYTES",
                256 * 1024,
                4 * 1024 * 1024,
            ),
            images_per_message: bounded("IRONCREW_API_MAX_IMAGES_PER_MESSAGE", 4, 32),
            images_per_conversation: bounded("IRONCREW_API_MAX_IMAGES_PER_CONVERSATION", 16, 256),
            image_bytes_per_message: bounded(
                "IRONCREW_API_MAX_IMAGE_BYTES_PER_MESSAGE",
                20 * 1024 * 1024,
                100 * 1024 * 1024,
            ),
            image_bytes_per_conversation: bounded(
                "IRONCREW_API_MAX_IMAGE_BYTES_PER_CONVERSATION",
                32 * 1024 * 1024,
                512 * 1024 * 1024,
            ),
            image_locator_bytes: bounded("IRONCREW_API_MAX_IMAGE_LOCATOR_BYTES", 2_048, 16 * 1024),
            image_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_MAX_IMAGE_BYTES",
                crate::utils::http::DEFAULT_IMAGE_BYTES,
            ),
            image_error_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_PROVIDER_MAX_ERROR_BYTES",
                crate::utils::http::DEFAULT_PROVIDER_ERROR_BYTES,
            ),
        }
    }

    pub(crate) fn definition(self) -> Value {
        json!({
            "turn_timeout_secs": self.turn_timeout_secs,
            "message_bytes": self.message_bytes,
            "images_per_message": self.images_per_message,
            "images_per_conversation": self.images_per_conversation,
            "image_bytes_per_message": self.image_bytes_per_message,
            "image_bytes_per_conversation": self.image_bytes_per_conversation,
            "image_locator_bytes": self.image_locator_bytes,
            "image_bytes": self.image_bytes,
            "image_error_bytes": self.image_error_bytes,
            "remote_image_network": "public_only",
        })
    }

    pub(crate) fn turn_timeout_secs(self) -> u64 {
        self.turn_timeout_secs
    }
    pub(crate) fn message_bytes(self) -> usize {
        self.message_bytes
    }
    pub(crate) fn images_per_message(self) -> usize {
        self.images_per_message
    }
    pub(crate) fn images_per_conversation(self) -> usize {
        self.images_per_conversation
    }
    pub(crate) fn image_bytes_per_message(self) -> usize {
        self.image_bytes_per_message
    }
    pub(crate) fn image_bytes_per_conversation(self) -> usize {
        self.image_bytes_per_conversation
    }
    pub(crate) fn image_locator_bytes(self) -> usize {
        self.image_locator_bytes
    }
    pub(crate) fn image_bytes(self) -> usize {
        self.image_bytes
    }
    pub(crate) fn image_error_bytes(self) -> usize {
        self.image_error_bytes
    }

    #[cfg(test)]
    pub(crate) fn from_marker(marker: usize) -> Self {
        Self {
            turn_timeout_secs: marker as u64,
            message_bytes: marker,
            images_per_message: marker,
            images_per_conversation: marker,
            image_bytes_per_message: marker,
            image_bytes_per_conversation: marker,
            image_locator_bytes: marker,
            image_bytes: marker,
            image_error_bytes: marker,
        }
    }
}

fn bounded(name: &str, default: usize, upper: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(upper))
        .unwrap_or(default.min(upper))
}
