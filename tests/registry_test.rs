use ironcrew::tools::file_read::FileReadTool;
use ironcrew::tools::file_write::FileWriteTool;
use ironcrew::tools::registry::ToolRegistry;

#[test]
fn test_register_and_get_tool() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FileReadTool::new(None)));
    assert!(registry.get("file_read").is_some());
    assert!(registry.get("nonexistent").is_none());
}

#[test]
fn test_list_tools() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FileReadTool::new(None)));
    registry.register(Box::new(FileWriteTool::new(None, None)));
    let names = registry.list();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"file_read".to_string()));
    assert!(names.contains(&"file_write".to_string()));
}

#[test]
fn test_schemas() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FileReadTool::new(None)));
    let schemas = registry.schemas();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "file_read");
}
