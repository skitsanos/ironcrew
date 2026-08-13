//! Compilation helpers for a pending HTTP tool listing.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use rmcp::model::ListToolsResult;

use super::{CachedTool, HeaderPolicyError, PendingListing};
use crate::mcp::http_tool_schema::compile_tool;

pub(super) fn stage_page(
    pending: &mut PendingListing,
    cursor: Option<&str>,
    result: &mut ListToolsResult,
) -> Result<bool, HeaderPolicyError> {
    if pending.terminal || ((pending.pages == 0) != cursor.is_none()) {
        return Err(HeaderPolicyError::InvalidListingSequence(pending.id.0));
    }
    let mut page_names = BTreeSet::new();
    let mut compiled = Vec::with_capacity(result.tools.len());
    for tool in &result.tools {
        match compile_tool(tool) {
            Ok((plan, sanitized)) => {
                let name = tool.name.to_string();
                if pending.tools.contains_key(&name) || !page_names.insert(name.clone()) {
                    return Err(HeaderPolicyError::DuplicateTool(pending.id.0, name));
                }
                let mut semantic = tool.clone();
                semantic.input_schema = Arc::new(sanitized.clone());
                let definition = serde_json::to_value(semantic).map_err(|error| {
                    HeaderPolicyError::InvalidSchema(format!("tool definition: {error}"))
                })?;
                compiled.push(Some((plan, sanitized, definition)));
            }
            Err(error) => {
                tracing::warn!(tool = %tool.name, %error, "excluding invalid MCP HTTP tool");
                compiled.push(None);
            }
        }
    }
    let source = std::mem::take(&mut result.tools);
    for (mut tool, compiled) in source.into_iter().zip(compiled) {
        let Some((plan, sanitized, semantic_definition)) = compiled else {
            continue;
        };
        pending.tools.insert(
            tool.name.to_string(),
            CachedTool {
                original_schema: Arc::clone(&tool.input_schema),
                semantic_definition,
                plan,
            },
        );
        tool.input_schema = Arc::new(sanitized);
        result.tools.push(tool);
    }
    pending.pages += 1;
    pending.terminal = result.next_cursor.is_none();
    Ok(pending.terminal)
}

pub(super) fn ensure_same_semantics(
    active: &BTreeMap<String, CachedTool>,
    pending: &BTreeMap<String, CachedTool>,
) -> Result<(), HeaderPolicyError> {
    let changed = active.iter().find_map(|(name, active_tool)| {
        let unchanged = pending
            .get(name)
            .is_some_and(|tool| tool.semantic_definition == active_tool.semantic_definition);
        (!unchanged).then_some(name.clone())
    });
    if let Some(name) = changed {
        return Err(HeaderPolicyError::RefreshSemanticDrift(name));
    }
    if active.len() != pending.len() {
        let name = pending
            .keys()
            .find(|name| !active.contains_key(*name))
            .cloned()
            .unwrap_or_else(|| "<tool-set>".to_owned());
        return Err(HeaderPolicyError::RefreshSemanticDrift(name));
    }
    Ok(())
}
