use ironcrew::tools::Tool;
use ironcrew::tools::ToolCallContext;
use ironcrew::tools::file_read::FileReadTool;
use ironcrew::tools::file_read_glob::FileReadGlobTool;
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

#[tokio::test]
async fn file_write_never_modifies_flow_source_or_extensionless_control_files() {
    let dir = tempfile::tempdir().unwrap();
    let tool = FileWriteTool::new(
        Some(dir.path().to_path_buf()),
        Some(vec!["lua".into(), "sh".into(), "txt".into()]),
    );
    let ctx = ToolCallContext::default();

    for path in ["crew.lua", "config.lua", "hook.sh", ".env", "Dockerfile"] {
        let result = tool
            .execute(json!({"path": path, "content": "malicious"}), &ctx)
            .await;
        assert!(result.is_err(), "unexpectedly wrote {path}");
        assert!(!dir.path().join(path).exists());
    }
}

#[tokio::test]
async fn agent_file_reads_never_expose_flow_credentials_or_state() {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in [
        (".env", "OPENAI_API_KEY=secret"),
        ("credentials.json", "{\"token\":\"secret\"}"),
        ("private.key", "secret"),
    ] {
        std::fs::write(dir.path().join(path), content).unwrap();
    }
    std::fs::create_dir_all(dir.path().join(".ironcrew")).unwrap();
    std::fs::write(dir.path().join(".ironcrew/run.json"), "secret").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        symlink(".env", dir.path().join("env-alias.txt")).unwrap();
        symlink(".ironcrew", dir.path().join("state-alias")).unwrap();
    }

    let reader = FileReadTool::new(Some(dir.path().to_path_buf()));
    let ctx = ToolCallContext::default();
    for path in [
        ".env",
        "credentials.json",
        "private.key",
        ".ironcrew/run.json",
    ] {
        assert!(
            reader.execute(json!({"path": path}), &ctx).await.is_err(),
            "unexpectedly read {path}"
        );
    }
    #[cfg(unix)]
    for path in ["env-alias.txt", "state-alias/run.json"] {
        assert!(
            reader.execute(json!({"path": path}), &ctx).await.is_err(),
            "unexpectedly read sensitive symlink alias {path}"
        );
    }

    let glob = FileReadGlobTool::new(Some(dir.path().to_path_buf()));
    let result = glob.execute(json!({"pattern": "*"}), &ctx).await.unwrap();
    assert!(!result.contains("OPENAI_API_KEY"));
    assert!(!result.contains("secret"));
}

#[tokio::test]
async fn file_read_and_write_enforce_default_byte_caps() {
    let dir = tempfile::tempdir().unwrap();
    let oversized = "x".repeat(10 * 1024 * 1024 + 1);
    std::fs::write(dir.path().join("oversized.txt"), &oversized).unwrap();
    let ctx = ToolCallContext::default();

    let reader = FileReadTool::new(Some(dir.path().to_path_buf()));
    let read_error = reader
        .execute(json!({"path": "oversized.txt"}), &ctx)
        .await
        .unwrap_err();
    assert!(read_error.to_string().contains("limit"));

    let writer = FileWriteTool::new(Some(dir.path().to_path_buf()), None);
    let write_error = writer
        .execute(json!({"path": "new.txt", "content": oversized}), &ctx)
        .await
        .unwrap_err();
    assert!(write_error.to_string().contains("MAX_BYTES"));
    assert!(!dir.path().join("new.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn project_file_tools_reject_symlink_escapes_and_special_files() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, "secret").unwrap();
    symlink(&outside_file, project.path().join("read-link.txt")).unwrap();
    symlink(outside.path(), project.path().join("outside-dir")).unwrap();

    let fifo = project.path().join("pipe.txt");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());

    let ctx = ToolCallContext::default();
    let reader = FileReadTool::new(Some(project.path().to_path_buf()));
    for path in ["read-link.txt", "outside-dir/secret.txt", "pipe.txt"] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.execute(json!({"path": path}), &ctx),
        )
        .await
        .expect("special-file read must not block");
        assert!(result.is_err(), "unexpectedly read {path}");
    }

    let writer = FileWriteTool::new(Some(project.path().to_path_buf()), None);
    assert!(
        writer
            .execute(
                json!({"path": "read-link.txt", "content": "overwritten"}),
                &ctx,
            )
            .await
            .is_err()
    );
    assert!(
        writer
            .execute(
                json!({"path": "outside-dir/new.txt", "content": "escaped"}),
                &ctx,
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "secret");
    assert!(!outside.path().join("new.txt").exists());
}

#[tokio::test]
async fn file_write_atomically_replaces_regular_file_without_temp_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("result.txt"), "old").unwrap();
    let tool = FileWriteTool::new(Some(dir.path().to_path_buf()), None);
    tool.execute(
        json!({"path": "result.txt", "content": "new"}),
        &ToolCallContext::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "new"
    );
    let temporary_count = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".ironcrew-write-")
        })
        .count();
    assert_eq!(temporary_count, 0);
}
