use super::{
    resolve_custom_provider_key, strict_string_list, trusted_provider_key_env_name,
    validate_api_key_value, validate_config_string,
};
use mlua::Lua;

#[test]
fn strict_string_list_rejects_sparse_mixed_and_duplicate_values() {
    let lua = Lua::new();

    let sparse = lua.create_table().unwrap();
    sparse.raw_set(1, "one").unwrap();
    sparse.raw_set(3, "three").unwrap();
    assert!(strict_string_list(&sparse, "items", 8, 32).is_err());

    let mixed = lua.create_table().unwrap();
    mixed.raw_set(1, "one").unwrap();
    mixed.set("extra", "two").unwrap();
    assert!(strict_string_list(&mixed, "items", 8, 32).is_err());

    let duplicate = lua.create_sequence_from(["one", "one"]).unwrap();
    assert!(strict_string_list(&duplicate, "items", 8, 32).is_err());
}

#[test]
fn strict_string_list_preserves_valid_dense_order() {
    let lua = Lua::new();
    let table = lua.create_sequence_from(["one", "two"]).unwrap();
    assert_eq!(
        strict_string_list(&table, "items", 2, 8).unwrap(),
        vec!["one", "two"]
    );
}

#[test]
fn config_strings_and_api_keys_are_bounded() {
    assert!(validate_config_string("goal", "", 10).is_err());
    assert!(validate_config_string("goal", "eleven bytes", 10).is_err());
    assert!(validate_config_string("goal", "valid", 10).is_ok());
    assert!(validate_api_key_value(" padded").is_err());
    assert!(validate_api_key_value("valid-key").is_ok());
}

#[test]
fn custom_provider_url_never_inherits_a_process_secret_for_untrusted_hosts() {
    assert!(resolve_custom_provider_key(Some("https://attacker.example/v1"), None).is_err());
    assert!(
        resolve_custom_provider_key(
            Some("https://attacker.example/v1"),
            Some("caller-owned-key")
        )
        .is_ok()
    );
    assert!(resolve_custom_provider_key(None, None).is_ok());
    assert_eq!(
        trusted_provider_key_env_name("https://generativelanguage.googleapis.com/v1beta/openai"),
        Some("GEMINI_API_KEY")
    );
    assert_eq!(
        trusted_provider_key_env_name("https://api.openai.com.attacker.example/v1"),
        None
    );
    assert_eq!(
        trusted_provider_key_env_name("http://api.openai.com/v1"),
        None
    );
}
