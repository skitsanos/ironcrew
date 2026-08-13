//! Transactional registry for MCP 2026 HTTP tool-header plans.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use rmcp::model::{
    ClientJsonRpcMessage, ClientRequest, ListToolsResult, ServerJsonRpcMessage, ServerResult, Tool,
};
use serde_json::{Map, Value};

use super::http_tool_schema::HeaderPlan;

#[path = "http_tool_listing.rs"]
mod listing;

pub(super) const HTTP_HEADER_POLICY_VERSION: &str = "mcp-2026-http-tool-headers-v1";

#[derive(Clone, Debug, Default)]
pub(super) struct HttpToolHeaderRegistry(Arc<RwLock<RegistryState>>);

#[derive(Debug, Default)]
struct RegistryState {
    generation: u64,
    next_id: u64,
    active: BTreeMap<String, CachedTool>,
    pending: Option<PendingListing>,
}

#[derive(Clone, Debug)]
struct CachedTool {
    original_schema: Arc<Map<String, Value>>,
    semantic_definition: Value,
    plan: HeaderPlan,
}

#[derive(Debug)]
struct PendingListing {
    id: ListingId,
    base_generation: u64,
    tools: BTreeMap<String, CachedTool>,
    pages: usize,
    terminal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ListingId(u64);

#[derive(Debug)]
#[must_use = "a listing transaction must be validated and committed"]
pub(super) struct ListingTransaction {
    id: ListingId,
    registry: HttpToolHeaderRegistry,
    armed: bool,
}

#[derive(Debug)]
#[must_use = "a validated listing must be committed or deliberately dropped"]
pub(super) struct ValidatedListing(ListingTransaction);

#[derive(Debug, thiserror::Error)]
pub(super) enum HeaderPolicyError {
    #[error("tool `{0}` has no current HTTP parameter-header plan")]
    MissingTool(String),
    #[error("invalid x-mcp-header annotation: {0}")]
    InvalidSchema(String),
    #[error("MCP tool argument `{0}` does not match its annotated primitive type")]
    InvalidArgument(String),
    #[error("MCP tool argument `{0}` is outside the JavaScript safe integer range")]
    UnsafeInteger(String),
    #[error("failed to construct MCP parameter header `{0}`")]
    InvalidHeader(String),
    #[error("a tools/list transaction is already active")]
    ConcurrentListing,
    #[error("no tools/list transaction is active")]
    NoPendingListing,
    #[error("tools/list transaction {0} is no longer active")]
    MismatchedListing(u64),
    #[error("tools/list transaction {0} received an invalid page sequence")]
    InvalidListingSequence(u64),
    #[error("tools/list transaction {0} contains duplicate tool `{1}`")]
    DuplicateTool(u64, String),
    #[error("tools/list transaction {0} is not terminal")]
    ListingIncomplete(u64),
    #[error("tools/list transaction {0} is stale")]
    StaleListing(u64),
    #[error("tools/list refresh changed the non-header definition of `{0}`")]
    RefreshSemanticDrift(String),
}

impl HttpToolHeaderRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn begin_listing(&self) -> Result<ListingTransaction, HeaderPolicyError> {
        let mut state = self.0.write().expect("MCP header registry lock");
        if state.pending.is_some() {
            return Err(HeaderPolicyError::ConcurrentListing);
        }
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let id = ListingId(state.next_id);
        state.pending = Some(PendingListing {
            id,
            base_generation: state.generation,
            tools: BTreeMap::new(),
            pages: 0,
            terminal: false,
        });
        Ok(ListingTransaction {
            id,
            registry: self.clone(),
            armed: true,
        })
    }

    /// Sanitize a backend response before rmcp can inspect or cache it.
    pub(super) fn stage_pending_server_message(
        &self,
        cursor: Option<&str>,
        message: &mut ServerJsonRpcMessage,
    ) -> Result<Option<bool>, HeaderPolicyError> {
        let ServerJsonRpcMessage::Response(response) = message else {
            return Ok(None);
        };
        let ServerResult::ListToolsResult(result) = &mut response.result else {
            return Ok(None);
        };
        self.stage_page(None, cursor, result).map(Some)
    }

    fn stage_page(
        &self,
        expected: Option<ListingId>,
        cursor: Option<&str>,
        result: &mut ListToolsResult,
    ) -> Result<bool, HeaderPolicyError> {
        let mut state = self.0.write().expect("MCP header registry lock");
        let pending = state
            .pending
            .as_mut()
            .ok_or(HeaderPolicyError::NoPendingListing)?;
        if expected.is_some_and(|id| id != pending.id) {
            return Err(HeaderPolicyError::MismatchedListing(expected.unwrap().0));
        }
        listing::stage_page(pending, cursor, result)
    }

    pub(super) fn headers_for_message(
        &self,
        message: &ClientJsonRpcMessage,
    ) -> Result<Vec<(axum::http::HeaderName, axum::http::HeaderValue)>, HeaderPolicyError> {
        let ClientJsonRpcMessage::Request(request) = message else {
            return Ok(Vec::new());
        };
        let ClientRequest::CallToolRequest(call) = &request.request else {
            return Ok(Vec::new());
        };
        self.headers_for_call(&call.params.name, call.params.arguments.as_ref())
    }

    pub(super) fn headers_for_call(
        &self,
        name: &str,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Vec<(axum::http::HeaderName, axum::http::HeaderValue)>, HeaderPolicyError> {
        self.0
            .read()
            .expect("MCP header registry lock")
            .active
            .get(name)
            .ok_or_else(|| HeaderPolicyError::MissingTool(name.to_owned()))?
            .plan
            .headers(arguments)
    }

    pub(super) fn plan_definition(&self, name: &str) -> Option<Value> {
        self.0
            .read()
            .expect("MCP header registry lock")
            .active
            .get(name)
            .map(|tool| tool.plan.definition())
    }
}

impl ListingTransaction {
    pub(super) fn restore_page(&self, tools: &mut [Tool]) -> Result<(), HeaderPolicyError> {
        let state = self.registry.0.read().expect("MCP header registry lock");
        let pending = matching_pending(&state, self.id)?;
        for tool in tools {
            let cached = pending
                .tools
                .get(tool.name.as_ref())
                .ok_or_else(|| HeaderPolicyError::MissingTool(tool.name.to_string()))?;
            tool.input_schema = Arc::clone(&cached.original_schema);
        }
        Ok(())
    }

    /// Call only after validating aggregate caps and the complete cursor chain.
    pub(super) fn into_validated(self) -> Result<ValidatedListing, HeaderPolicyError> {
        let terminal = {
            let state = self.registry.0.read().expect("MCP header registry lock");
            matching_pending(&state, self.id)?.terminal
        };
        if !terminal {
            return Err(HeaderPolicyError::ListingIncomplete(self.id.0));
        }
        Ok(ValidatedListing(self))
    }
}

impl Drop for ListingTransaction {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.registry.0.write().expect("MCP header registry lock");
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == self.id)
        {
            state.pending = None;
        }
    }
}

impl ValidatedListing {
    pub(super) fn commit(mut self) -> Result<u64, HeaderPolicyError> {
        self.finish(false)
    }

    pub(super) fn commit_refresh(mut self) -> Result<u64, HeaderPolicyError> {
        self.finish(true)
    }

    fn finish(&mut self, refresh: bool) -> Result<u64, HeaderPolicyError> {
        let mut state = self.0.registry.0.write().expect("MCP header registry lock");
        let pending = matching_pending(&state, self.0.id)?;
        if pending.base_generation != state.generation {
            return Err(HeaderPolicyError::StaleListing(self.0.id.0));
        }
        if refresh {
            listing::ensure_same_semantics(&state.active, &pending.tools)?;
        }
        let tools = state
            .pending
            .take()
            .expect("matching pending listing")
            .tools;
        state.generation = state.generation.wrapping_add(1).max(1);
        state.active = tools;
        self.0.armed = false;
        Ok(state.generation)
    }
}

fn matching_pending(
    state: &RegistryState,
    id: ListingId,
) -> Result<&PendingListing, HeaderPolicyError> {
    state
        .pending
        .as_ref()
        .filter(|pending| pending.id == id)
        .ok_or(HeaderPolicyError::MismatchedListing(id.0))
}
