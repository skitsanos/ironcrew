//! Sandboxed `require` for flow VMs.
//!
//! Resolves Lua-source modules ONLY from configured library directories
//! (`<flow-dir>/_lib`). The Lua `package` stdlib is never enabled, so
//! `package.cpath` / `package.loadlib` / C-module loaders do not exist in the
//! VM — the sandbox is tight by construction, not by removal.

use mlua::{Lua, Result as LuaResult, Table, Value};
use std::path::{Path, PathBuf};

const LOADED_KEY: &str = "__ic_modules_loaded";
const LOADING_KEY: &str = "__ic_modules_loading";

/// Map a module name to a relative `.lua` path with strict validation.
///
/// Splits on `.`; each segment must be non-empty and consist only of
/// `[A-Za-z0-9_-]`. This rejects `..`, `/`, `\`, `:`, absolute paths, and
/// leading/trailing/double dots at the name level. `"credentials"` ->
/// `credentials.lua`; `"auth.jwt"` -> `auth/jwt.lua`.
fn module_name_to_relpath(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err(format!("invalid module name '{name}'"));
    }
    let mut path = PathBuf::new();
    for segment in name.split('.') {
        let valid = !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(format!("invalid module name '{name}'"));
        }
        path.push(segment);
    }
    path.set_extension("lua");
    Ok(path)
}

/// Resolve `relpath` against `roots`, returning the first existing match that
/// passes a canonicalize + containment check (defeats symlink/`..` escape).
/// Mirrors the path-safety approach of `validate_tool_fs_path` in sandbox.rs.
fn resolve_module_path(roots: &[PathBuf], relpath: &Path) -> Option<PathBuf> {
    for root in roots {
        let candidate = root.join(relpath);
        if !candidate.is_file() {
            continue;
        }
        let canonical = match candidate.canonicalize() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        if canonical.starts_with(&root_canonical) {
            return Some(canonical);
        }
    }
    None
}

/// Register a sandboxed `require` global resolving Lua-source modules only from
/// `roots`. No-op when `roots` is empty (preserves status quo for no-project
/// callers such as graph_extract). Caches results and detects circular requires.
pub fn install_require(lua: &Lua, roots: Vec<PathBuf>) -> LuaResult<()> {
    if roots.is_empty() {
        return Ok(());
    }
    lua.set_named_registry_value(LOADED_KEY, lua.create_table()?)?;
    lua.set_named_registry_value(LOADING_KEY, lua.create_table()?)?;

    let require = lua.create_function(move |lua, name: String| {
        let relpath = module_name_to_relpath(&name).map_err(mlua::Error::external)?;

        let loaded: Table = lua.named_registry_value(LOADED_KEY)?;
        let cached: Value = loaded.get(name.as_str())?;
        if cached != Value::Nil {
            return Ok(cached);
        }

        let loading: Table = lua.named_registry_value(LOADING_KEY)?;
        let in_progress: Value = loading.get(name.as_str())?;
        if in_progress != Value::Nil {
            return Err(mlua::Error::external(format!(
                "circular require detected: '{name}'"
            )));
        }

        let path = resolve_module_path(&roots, &relpath)
            .ok_or_else(|| mlua::Error::external(format!("module '{name}' not found in _lib")))?;
        let source = std::fs::read_to_string(&path).map_err(mlua::Error::external)?;

        loading.set(name.as_str(), true)?;
        let outcome = lua
            .load(&source)
            .set_name(format!("@{}", path.display()))
            .eval::<Value>();
        loading.set(name.as_str(), Value::Nil)?; // clear regardless of outcome

        let value = outcome?;
        // Mirror stock Lua: a module that returns nothing caches as `true`.
        let to_cache = if value == Value::Nil {
            Value::Boolean(true)
        } else {
            value
        };
        loaded.set(name.as_str(), to_cache.clone())?;
        Ok(to_cache)
    })?;

    lua.globals().set("require", require)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn maps_simple_name() {
        assert_eq!(
            module_name_to_relpath("credentials").unwrap(),
            PathBuf::from("credentials.lua")
        );
    }

    #[test]
    fn maps_dotted_name_to_subdir() {
        assert_eq!(
            module_name_to_relpath("auth.jwt").unwrap(),
            PathBuf::from("auth").join("jwt.lua")
        );
    }

    #[test]
    fn rejects_traversal_and_separators() {
        for bad in [
            "..",
            "../secret",
            "/etc/passwd",
            "a/b",
            "a\\b",
            "a:b",
            "",
            ".",
            "a.",
            ".a",
            "a..b",
        ] {
            assert!(
                module_name_to_relpath(bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn resolves_existing_module() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("m.lua"), "return {}").unwrap();
        let got = resolve_module_path(&[dir.path().to_path_buf()], Path::new("m.lua"));
        assert_eq!(
            got.unwrap(),
            dir.path().join("m.lua").canonicalize().unwrap()
        );
    }

    #[test]
    fn returns_none_for_missing_module() {
        let dir = TempDir::new().unwrap();
        assert!(resolve_module_path(&[dir.path().to_path_buf()], Path::new("nope.lua")).is_none());
    }

    fn lib_vm(files: &[(&str, &str)]) -> (Lua, TempDir) {
        let dir = TempDir::new().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let lua = Lua::new();
        install_require(&lua, vec![dir.path().to_path_buf()]).unwrap();
        (lua, dir)
    }

    #[test]
    fn caches_module_executed_once() {
        let (lua, _d) = lib_vm(&[(
            "counter.lua",
            "_G.exec_count = (_G.exec_count or 0) + 1\nreturn { n = _G.exec_count }",
        )]);
        let out: i64 = lua
            .load(
                "local a = require('counter')\nlocal b = require('counter')\nreturn a.n + b.n + _G.exec_count",
            )
            .eval()
            .unwrap();
        // executed once: a.n=1, b.n=1, exec_count=1 -> 3
        assert_eq!(out, 3);
    }

    #[test]
    fn circular_require_errors_cleanly() {
        let (lua, _d) = lib_vm(&[
            ("a.lua", "require('b')\nreturn {}"),
            ("b.lua", "require('a')\nreturn {}"),
        ]);
        let err = lua
            .load("return require('a')")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("circular require"), "got: {err}");
    }

    #[test]
    fn unknown_module_errors() {
        let (lua, _d) = lib_vm(&[]);
        let err = lua
            .load("return require('nope')")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found in _lib"), "got: {err}");
    }

    #[test]
    fn invalid_name_errors_from_lua() {
        let (lua, _d) = lib_vm(&[]);
        let err = lua
            .load("return require('../secret')")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid module name"), "got: {err}");
    }
}
