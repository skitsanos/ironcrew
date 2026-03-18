use ironcrew::tools::hash::HashTool;
use ironcrew::tools::template_render::TemplateRenderTool;
use ironcrew::tools::Tool;
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
