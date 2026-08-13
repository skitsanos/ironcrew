//! MCP 2026-07-28 client identity and strict discovery lifecycle.

use rmcp::{
    ClientLifecycleMode,
    model::{ClientCapabilities, ClientInfo, Implementation, ProtocolVersion},
};

pub(super) fn client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("ironcrew", env!("CARGO_PKG_VERSION")),
    )
}

pub(super) fn discovery_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}
