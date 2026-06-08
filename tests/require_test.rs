//! End-to-end tests for sandboxed `require` from a `_lib` directory (#34).

use ironcrew::lua::sandbox::{create_crew_lua, create_crew_lua_with_lib_dirs};
use std::fs;
use tempfile::TempDir;

/// A temp project whose `_lib/` holds the given `(filename, body)` modules.
fn project_with_lib(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let lib = dir.path().join("_lib");
    fs::create_dir_all(&lib).unwrap();
    for (name, body) in files {
        fs::write(lib.join(name), body).unwrap();
    }
    dir
}

fn crew_vm(dir: &TempDir) -> mlua::Lua {
    create_crew_lua_with_lib_dirs(vec![dir.path().join("_lib")]).unwrap()
}

#[test]
fn bare_crew_vm_has_no_require() {
    let lua = create_crew_lua().unwrap();
    let is_nil: bool = lua.load("return require == nil").eval().unwrap();
    assert!(is_nil, "bare create_crew_lua must not expose require");
}

#[test]
fn package_stdlib_is_unavailable() {
    let dir = project_with_lib(&[]);
    let lua = crew_vm(&dir);
    let is_nil: bool = lua.load("return package == nil").eval().unwrap();
    assert!(is_nil, "the Lua package stdlib must never be enabled");
}

#[test]
fn flow_can_require_module_and_call_it() {
    let dir = project_with_lib(&[(
        "credentials.lua",
        "local M = {}\nfunction M.resolve(id) return 'key-' .. id end\nreturn M",
    )]);
    let lua = crew_vm(&dir);
    let out: String = lua
        .load("local c = require('credentials')\nreturn c.resolve('42')")
        .eval()
        .unwrap();
    assert_eq!(out, "key-42");
}

#[test]
fn require_twice_executes_once() {
    let dir = project_with_lib(&[(
        "counter.lua",
        "_G.exec_count = (_G.exec_count or 0) + 1\nreturn { n = _G.exec_count }",
    )]);
    let lua = crew_vm(&dir);
    let out: i64 = lua
        .load("require('counter')\nrequire('counter')\nreturn _G.exec_count")
        .eval()
        .unwrap();
    assert_eq!(out, 1);
}

#[test]
fn require_rejects_traversal_and_unknown() {
    let dir = project_with_lib(&[]);
    let lua = crew_vm(&dir);
    for (name, needle) in [
        ("..", "invalid module name"),
        ("../secret", "invalid module name"),
        ("/etc/passwd", "invalid module name"),
        ("nope", "not found in _lib"),
    ] {
        let err = lua
            .load(format!("return require('{name}')"))
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains(needle), "name {name:?}: got {err}");
    }
}

#[test]
fn module_shares_flow_sandbox_globals() {
    // A module uses base64_encode (a sandbox global) — proves modules run in
    // the same VM with the same globals as the flow.
    let dir = project_with_lib(&[(
        "enc.lua",
        "local M = {}\nfunction M.b64(s) return base64_encode(s) end\nreturn M",
    )]);
    let lua = crew_vm(&dir);
    let out: String = lua.load("return require('enc').b64('hi')").eval().unwrap();
    assert_eq!(out, "aGk=");
}
