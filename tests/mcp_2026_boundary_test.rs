#![cfg(feature = "mcp")]

#[path = "mcp_2026_protocol/boundary_test_support.rs"]
mod boundary_test_support;
#[path = "mcp_2026_protocol/capability_negotiation_tests.rs"]
mod capability_negotiation_tests;
#[path = "mcp_2026_protocol/header_http_fixture.rs"]
mod header_http_fixture;
#[path = "mcp_2026_protocol/header_mismatch_cases.rs"]
mod header_mismatch_cases;
#[path = "mcp_2026_protocol/http_boundary_tests.rs"]
mod http_boundary_tests;
#[path = "mcp_2026_protocol/http_fixture.rs"]
mod http_fixture;
#[path = "mcp_2026_protocol/http_param_header_contract_tests.rs"]
mod http_param_header_contract_tests;
#[path = "mcp_2026_protocol/http_param_header_identity_tests.rs"]
mod http_param_header_identity_tests;
#[path = "mcp_2026_protocol/http_param_header_pagination_tests.rs"]
mod http_param_header_pagination_tests;
#[path = "mcp_2026_protocol/http_param_header_tests.rs"]
mod http_param_header_tests;
#[path = "mcp_2026_protocol/http_param_header_value_tests.rs"]
mod http_param_header_value_tests;
#[path = "mcp_2026_protocol/param_header_schemas.rs"]
mod param_header_schemas;
#[path = "mcp_2026_protocol/raw_http_fixture.rs"]
mod raw_http_fixture;
#[path = "mcp_2026_protocol/raw_http_response.rs"]
mod raw_http_response;
#[path = "mcp_2026_protocol/stdio_boundary_tests.rs"]
#[cfg(unix)]
mod stdio_boundary_tests;
#[path = "mcp_2026_protocol/stdio_test_support.rs"]
#[cfg(unix)]
mod stdio_test_support;
