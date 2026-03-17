use ironcrew::lua::loader::ProjectLoader;
use std::fs;

#[test]
fn test_discover_project_directory() {
    let dir = tempfile::tempdir().unwrap();
    let agents_dir = dir.path().join("agents");
    let tools_dir = dir.path().join("tools");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::create_dir_all(&tools_dir).unwrap();
    fs::write(agents_dir.join("researcher.lua"), "return {name='researcher', goal='research'}").unwrap();
    fs::write(agents_dir.join("writer.lua"), "return {name='writer', goal='write'}").unwrap();
    fs::write(tools_dir.join("summarize.lua"), "return {name='summarize'}").unwrap();
    fs::write(dir.path().join("crew.lua"), "-- entrypoint").unwrap();

    let loader = ProjectLoader::from_directory(dir.path()).unwrap();
    assert_eq!(loader.agent_files().len(), 2);
    assert_eq!(loader.tool_files().len(), 1);
    assert!(loader.entrypoint().is_some());
}

#[test]
fn test_discover_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let crew_file = dir.path().join("crew.lua");
    fs::write(&crew_file, "-- single file mode").unwrap();

    let loader = ProjectLoader::from_file(&crew_file).unwrap();
    assert!(loader.agent_files().is_empty());
    assert!(loader.tool_files().is_empty());
    assert!(loader.entrypoint().is_some());
}

#[test]
fn test_missing_entrypoint() {
    let dir = tempfile::tempdir().unwrap();
    let result = ProjectLoader::from_directory(dir.path());
    assert!(result.is_err());
}
