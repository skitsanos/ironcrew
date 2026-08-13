//! IronCrew's dependency-proof strict MCP 2026 message surface.

use rmcp::model::{ServerJsonRpcMessage, ServerNotification};

pub(super) fn inbound_is_allowed(message: &ServerJsonRpcMessage) -> bool {
    match message {
        ServerJsonRpcMessage::Response(_) | ServerJsonRpcMessage::Error(_) => true,
        ServerJsonRpcMessage::Request(_) => false,
        ServerJsonRpcMessage::Notification(notification) => matches!(
            notification.notification,
            ServerNotification::CancelledNotification(_)
                | ServerNotification::ProgressNotification(_)
                | ServerNotification::LoggingMessageNotification(_)
                | ServerNotification::ToolListChangedNotification(_)
        ),
    }
}
