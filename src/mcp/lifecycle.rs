//! MCP 2026-07-28 client identity and strict discovery lifecycle.
#![expect(deprecated, reason = "strictly reject deprecated roots requests")]

use rmcp::{
    ClientHandler, ClientLifecycleMode,
    model::{
        ClientCapabilities, ClientInfo, ElicitRequestParams, ElicitResult, ErrorData,
        Implementation, ListRootsRequestMethod, ListRootsResult, ProtocolVersion,
    },
    service::{RequestContext, RoleClient},
};

#[derive(Clone, Copy)]
pub(super) struct StrictClientHandler;

impl ClientHandler for StrictClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("ironcrew", env!("CARGO_PKG_VERSION")),
        )
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ListRootsResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(ErrorData::method_not_found::<ListRootsRequestMethod>()))
    }

    fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ElicitResult, ErrorData>> + Send + '_ {
        std::future::ready(Err(ErrorData::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "server-initiated elicitation is outside IronCrew's capabilities",
            None,
        )))
    }
}

pub(super) fn discovery_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}
