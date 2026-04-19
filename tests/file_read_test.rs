use ironcrew::tools::Tool;
use ironcrew::tools::ToolCallContext;
use ironcrew::tools::file_read::FileReadTool;
use ironcrew::tools::file_write::FileWriteTool;
use serde_json::json;

#[tokio::test]
async fn test_file_read_success() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    let tool = FileReadTool::new(Some(dir.path().to_path_buf()));
    let ctx = ToolCallContext::default();
    let result = tool
        .execute(json!({"path": "test.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(result, "hello world");
}

#[tokio::test]
async fn test_file_read_traversal_blocked() {
    let tool = FileReadTool::new(None);
    let ctx = ToolCallContext::default();
    let result = tool.execute(json!({"path": "../etc/passwd"}), &ctx).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("traversal"));
}

#[tokio::test]
async fn test_file_write_and_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolCallContext::default();

    let write_tool = FileWriteTool::new(Some(dir.path().to_path_buf()), None);
    write_tool
        .execute(
            json!({"path": "output.txt", "content": "test content"}),
            &ctx,
        )
        .await
        .unwrap();

    let read_tool = FileReadTool::new(Some(dir.path().to_path_buf()));
    let result = read_tool
        .execute(json!({"path": "output.txt"}), &ctx)
        .await
        .unwrap();
    assert_eq!(result, "test content");
}

#[tokio::test]
async fn test_file_write_blocked_extension() {
    let tool = FileWriteTool::new(None, None);
    let ctx = ToolCallContext::default();
    let result = tool
        .execute(json!({"path": "evil.exe", "content": "bad"}), &ctx)
        .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not allowed"));
}
