use std::sync::Once;

pub(super) fn isolate_environment() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        std::env::set_var("IRONCREW_MCP_ALLOW_LOCALHOST", "1");
        std::env::set_var("IRONCREW_MCP_MAX_INBOUND_MESSAGE_BYTES", "1048576");
        std::env::set_var("IRONCREW_MCP_MAX_MRTR_ROUNDS", "4");
        std::env::set_var("IRONCREW_MCP_MAX_REQUEST_STATE_BYTES", "65536");
        std::env::set_var("IRONCREW_MCP_CALL_TIMEOUT_SECS", "1");
        std::env::set_var("IRONCREW_MCP_DISCOVERY_TIMEOUT_SECS", "1");
        std::env::set_var("IRONCREW_MCP_LIST_TIMEOUT_SECS", "1");
        std::env::set_var("IRONCREW_MCP_SHUTDOWN_TIMEOUT_SECS", "5");
    });
}
