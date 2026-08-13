use serde_json::{Value, json};

#[derive(Clone, Copy, Debug)]
pub(super) enum HeaderMismatchCase {
    RetrySucceeds,
    Repeated,
    WrongCode,
    MissingRefreshTarget,
    InvalidRefreshTarget,
    PaginatedRetrySucceeds,
    DuplicateRefreshPage,
}

pub(super) struct HeaderListPage {
    pub(super) tools: Value,
    pub(super) next_cursor: Option<&'static str>,
}

impl HeaderMismatchCase {
    pub(super) fn page(self, list_index: usize, cursor: Option<&str>) -> HeaderListPage {
        if matches!(self, Self::PaginatedRetrySucceeds) && list_index == 0 {
            return HeaderListPage {
                tools: refresh_tool("Stale"),
                next_cursor: Some("initial-page-2"),
            };
        }
        if matches!(self, Self::PaginatedRetrySucceeds)
            && list_index == 1
            && cursor == Some("initial-page-2")
        {
            return HeaderListPage {
                tools: sibling_tool(),
                next_cursor: None,
            };
        }
        if list_index == 0 {
            return HeaderListPage {
                tools: refresh_tool("Stale"),
                next_cursor: None,
            };
        }
        let tools = match self {
            Self::MissingRefreshTarget => json!([]),
            Self::InvalidRefreshTarget => invalid_refresh_tool(),
            Self::PaginatedRetrySucceeds | Self::DuplicateRefreshPage if cursor.is_none() => {
                return HeaderListPage {
                    tools: refresh_tool("Current"),
                    next_cursor: Some("refresh-page-2"),
                };
            }
            Self::PaginatedRetrySucceeds if cursor == Some("refresh-page-2") => sibling_tool(),
            Self::DuplicateRefreshPage if cursor == Some("refresh-page-2") => {
                refresh_tool("Duplicate")
            }
            Self::RetrySucceeds | Self::Repeated | Self::WrongCode => refresh_tool("Current"),
            Self::PaginatedRetrySucceeds | Self::DuplicateRefreshPage => json!([]),
        };
        HeaderListPage {
            tools,
            next_cursor: None,
        }
    }

    pub(super) fn rejects_call(self, call_index: usize) -> bool {
        call_index == 0 || matches!(self, Self::Repeated)
    }

    pub(super) fn error_code(self) -> i64 {
        if matches!(self, Self::WrongCode) {
            -32021
        } else {
            -32020
        }
    }
}

fn refresh_tool(header: &str) -> Value {
    json!([{
        "name": "refresh",
        "description": "Refresh the cached parameter-header plan.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tenant": {
                    "type": "object",
                    "properties": {
                        "region": {"type": "string", "x-mcp-header": header}
                    }
                }
            }
        }
    }])
}

fn invalid_refresh_tool() -> Value {
    json!([{
        "name": "refresh",
        "description": "Invalid refreshed parameter-header plan.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tenant": {
                    "type": "array",
                    "items": {"type": "string", "x-mcp-header": "Invalid"}
                }
            }
        }
    }])
}

fn sibling_tool() -> Value {
    json!([{
        "name": "refresh-page-two",
        "description": "Terminal page marker.",
        "inputSchema": {"type": "object", "properties": {}}
    }])
}
