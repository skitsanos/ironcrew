//! Immutable, capability-scoped snapshots of executable Lua sources.

#[cfg(not(unix))]
use std::path::Path;

#[cfg(not(unix))]
use crate::utils::error::IronCrewError;
use crate::utils::error::Result;

mod snapshot;
pub use snapshot::{
    ConversationSourceContext, FlowSourceRoles, FlowSourceSnapshot, SnapshotLuaSource,
};

#[cfg(unix)]
mod support;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::capture_flow_source;

#[cfg(not(unix))]
pub fn capture_flow_source(_flow_root: &Path) -> Result<FlowSourceSnapshot> {
    Err(IronCrewError::Validation(
        "secure no-follow flow-source traversal is unavailable on this platform".into(),
    ))
}

/// Compatibility wrapper for callers that only need the immutable tree hash.
pub fn flow_source_fingerprint(flow_root: &std::path::Path) -> Result<String> {
    Ok(capture_flow_source(flow_root)?.fingerprint().to_owned())
}

#[cfg(test)]
mod tests;
