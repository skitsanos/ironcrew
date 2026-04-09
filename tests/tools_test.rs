use ironcrew::tools::Tool;
use ironcrew::tools::file_read_glob::FileReadGlobTool;
use ironcrew::tools::hash::HashTool;
use ironcrew::tools::template_render::TemplateRenderTool;
use ironcrew::tools::validate_schema::ValidateSchemaTool;
use serde_json::json;

#[tokio::test]
async fn test_hash_sha256() {
    let tool = HashTool::new();
    let result = tool
        .execute(json!({"text": "hello", "algorithm": "sha256"}))
        .await
        .unwrap();
    assert_eq!(
        result,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[tokio::test]
async fn test_hash_md5() {
    let tool = HashTool::new();
    let result = tool
        .execute(json!({"text": "hello", "algorithm": "md5"}))
        .await
        .unwrap();
    assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
}

#[tokio::test]
async fn test_template_render_basic() {
    let tool = TemplateRenderTool::new();
    let result = tool
        .execute(json!({
            "template": "Hello {{ name }}! You are {{ age }} years old.",
            "data": {"name": "Alice", "age": 30}
        }))
        .await
        .unwrap();
    assert_eq!(result, "Hello Alice! You are 30 years old.");
}

#[tokio::test]
async fn test_template_render_with_loop() {
    let tool = TemplateRenderTool::new();
    let result = tool
        .execute(json!({
            "template": "{% for item in items %}{{ item }},{% endfor %}",
            "data": {"items": ["a", "b", "c"]}
        }))
        .await
        .unwrap();
    assert_eq!(result, "a,b,c,");
}

#[tokio::test]
async fn test_template_render_with_conditional() {
    let tool = TemplateRenderTool::new();
    let result = tool
        .execute(json!({
            "template": "{% if active %}Active{% else %}Inactive{% endif %}",
            "data": {"active": true}
        }))
        .await
        .unwrap();
    assert_eq!(result, "Active");
}

#[tokio::test]
async fn test_file_read_glob_basic() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "content a").unwrap();
    std::fs::write(dir.path().join("b.txt"), "content b").unwrap();
    std::fs::write(dir.path().join("c.md"), "content c").unwrap();

    let tool = FileReadGlobTool::new(Some(dir.path().to_path_buf()));
    let result = tool.execute(json!({"pattern": "*.txt"})).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    // v2.6.0: output is an object, not a bare array
    let files = parsed["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["content"], "content a");
    assert_eq!(files[1]["content"], "content b");
    assert_eq!(parsed["file_count"], 2);
    assert_eq!(parsed["truncated"], false);
    // total_bytes = len("content a") + len("content b") = 9 + 9 = 18
    assert_eq!(parsed["total_bytes"], 18);
}

#[tokio::test]
async fn test_file_read_glob_file_count_cap() {
    // Temporarily force a tight file-count cap via env var
    unsafe {
        std::env::set_var("IRONCREW_GLOB_MAX_FILES", "2");
    }

    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("f{}.txt", i)), format!("file{}", i)).unwrap();
    }

    let tool = FileReadGlobTool::new(Some(dir.path().to_path_buf()));
    let result = tool.execute(json!({"pattern": "*.txt"})).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["file_count"], 2);
    assert_eq!(parsed["truncated"], true);

    unsafe {
        std::env::remove_var("IRONCREW_GLOB_MAX_FILES");
    }
}

#[tokio::test]
async fn test_file_read_glob_traversal_blocked() {
    let tool = FileReadGlobTool::new(None);
    let result = tool.execute(json!({"pattern": "../etc/*.conf"})).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_validate_schema_valid() {
    let tool = ValidateSchemaTool::new();
    let result = tool
        .execute(json!({
            "data": r#"{"name": "Alice", "age": 30}"#,
            "schema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "age": {"type": "integer"}
                },
                "required": ["name", "age"]
            }
        }))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["valid"], true);
}

#[tokio::test]
async fn test_validate_schema_invalid() {
    let tool = ValidateSchemaTool::new();
    let result = tool
        .execute(json!({
            "data": r#"{"name": 123}"#,
            "schema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        }))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["valid"], false);
    assert!(parsed["error_count"].as_u64().unwrap() > 0);
}
