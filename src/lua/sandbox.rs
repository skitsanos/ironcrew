use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mlua::{Lua, Result as LuaResult, StdLib, Value};
use std::path::{Component, Path, PathBuf};

use crate::lua::api::{json_value_to_lua, lua_table_to_json, lua_value_to_json};

/// Register utility global functions available in all Lua sandboxes.
pub fn register_lua_globals(lua: &Lua) -> LuaResult<()> {
    // env()
    let env_fn = lua.create_function(|_, name: String| Ok(std::env::var(&name).ok()))?;
    lua.globals().set("env", env_fn)?;

    // uuid4()
    let uuid_fn = lua.create_function(|_, ()| Ok(uuid::Uuid::new_v4().to_string()))?;
    lua.globals().set("uuid4", uuid_fn)?;

    // now_rfc3339()
    let now_rfc3339_fn = lua.create_function(|_, ()| Ok(chrono::Utc::now().to_rfc3339()))?;
    lua.globals().set("now_rfc3339", now_rfc3339_fn)?;

    // now_unix_ms()
    let now_unix_ms_fn = lua.create_function(|_, ()| {
        Ok(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64)
    })?;
    lua.globals().set("now_unix_ms", now_unix_ms_fn)?;

    // json_parse(str) -> Lua value
    let json_parse_fn = lua.create_function(|lua, s: String| {
        let value: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
        json_value_to_lua(lua, &value)
    })?;
    lua.globals().set("json_parse", json_parse_fn)?;

    // json_stringify(value) -> JSON string
    let json_stringify_fn = lua.create_function(|_, value: Value| {
        let json = lua_value_to_json(value)?;
        serde_json::to_string(&json).map_err(mlua::Error::external)
    })?;
    lua.globals().set("json_stringify", json_stringify_fn)?;

    // base64_encode(str)
    let b64_encode_fn = lua.create_function(|_, s: String| Ok(STANDARD.encode(s.as_bytes())))?;
    lua.globals().set("base64_encode", b64_encode_fn)?;

    // base64_decode(str)
    let b64_decode_fn = lua.create_function(|_, s: String| {
        let bytes = STANDARD.decode(&s).map_err(mlua::Error::external)?;
        String::from_utf8(bytes).map_err(mlua::Error::external)
    })?;
    lua.globals().set("base64_decode", b64_decode_fn)?;

    // log(level, msg...) — also emits to EventBus if available
    let log_fn = lua.create_function(|lua, args: mlua::Variadic<String>| {
        let args: Vec<String> = args.into_iter().collect();
        if args.is_empty() {
            return Ok(());
        }

        let (level, message) = if args.len() >= 2 {
            let lvl = args[0].as_str();
            let msg = args[1..].join(" ");
            (lvl.to_string(), msg)
        } else {
            ("info".to_string(), args[0].clone())
        };

        match level.as_str() {
            "trace" => tracing::trace!("<lua> {}", message),
            "debug" => tracing::debug!("<lua> {}", message),
            "info" => tracing::info!("<lua> {}", message),
            "warn" => tracing::warn!("<lua> {}", message),
            "error" => tracing::error!("<lua> {}", message),
            _ => tracing::info!("<lua> {}", message),
        }

        // Emit to EventBus if one is injected via app_data
        if let Some(eventbus) = lua.app_data_ref::<crate::engine::eventbus::EventBus>() {
            eventbus.emit(crate::engine::eventbus::CrewEvent::Log {
                level: level.clone(),
                message: message.clone(),
            });
        }

        Ok(())
    })?;
    lua.globals().set("log", log_fn)?;

    // Override print() to also emit to EventBus as a log event
    let print_fn = lua.create_function(|lua, args: mlua::Variadic<String>| {
        let message = args.into_iter().collect::<Vec<_>>().join("\t");
        println!("{}", message);

        if let Some(eventbus) = lua.app_data_ref::<crate::engine::eventbus::EventBus>() {
            eventbus.emit(crate::engine::eventbus::CrewEvent::Log {
                level: "info".into(),
                message,
            });
        }

        Ok(())
    })?;
    lua.globals().set("print", print_fn)?;

    // regex namespace — Rust regex engine exposed to Lua
    let regex_table = lua.create_table()?;

    // regex.match(pattern, text) -> bool
    let regex_match = lua.create_function(|_, (pattern, text): (String, String)| {
        let re = regex::Regex::new(&pattern).map_err(mlua::Error::external)?;
        Ok(re.is_match(&text))
    })?;
    regex_table.set("match", regex_match)?;

    // regex.find(pattern, text) -> string|nil (first match)
    let regex_find = lua.create_function(|_, (pattern, text): (String, String)| {
        let re = regex::Regex::new(&pattern).map_err(mlua::Error::external)?;
        Ok(re.find(&text).map(|m| m.as_str().to_string()))
    })?;
    regex_table.set("find", regex_find)?;

    // regex.find_all(pattern, text) -> table of strings (all matches)
    let regex_find_all = lua.create_function(|lua, (pattern, text): (String, String)| {
        let re = regex::Regex::new(&pattern).map_err(mlua::Error::external)?;
        let matches: Vec<String> = re
            .find_iter(&text)
            .map(|m| m.as_str().to_string())
            .collect();
        let table = lua.create_table()?;
        for (i, m) in matches.iter().enumerate() {
            table.set(i + 1, m.as_str())?;
        }
        Ok(table)
    })?;
    regex_table.set("find_all", regex_find_all)?;

    // regex.captures(pattern, text) -> table of capture groups|nil
    let regex_captures = lua.create_function(|lua, (pattern, text): (String, String)| {
        let re = regex::Regex::new(&pattern).map_err(mlua::Error::external)?;
        match re.captures(&text) {
            Some(caps) => {
                let table = lua.create_table()?;
                for (i, cap) in caps.iter().enumerate() {
                    if let Some(m) = cap {
                        table.set(i, m.as_str().to_string())?;
                    }
                }
                // Also set named captures
                for name in re.capture_names().flatten() {
                    if let Some(m) = caps.name(name) {
                        table.set(name, m.as_str().to_string())?;
                    }
                }
                Ok(mlua::Value::Table(table))
            }
            None => Ok(mlua::Value::Nil),
        }
    })?;
    regex_table.set("captures", regex_captures)?;

    // regex.replace(pattern, text, replacement) -> string (first match)
    let regex_replace = lua.create_function(
        |_, (pattern, text, replacement): (String, String, String)| {
            let re = regex::Regex::new(&pattern).map_err(mlua::Error::external)?;
            Ok(re.replace(&text, replacement.as_str()).into_owned())
        },
    )?;
    regex_table.set("replace", regex_replace)?;

    // regex.replace_all(pattern, text, replacement) -> string
    let regex_replace_all = lua.create_function(
        |_, (pattern, text, replacement): (String, String, String)| {
            let re = regex::Regex::new(&pattern).map_err(mlua::Error::external)?;
            Ok(re.replace_all(&text, replacement.as_str()).into_owned())
        },
    )?;
    regex_table.set("replace_all", regex_replace_all)?;

    // regex.split(pattern, text) -> table of strings
    let regex_split = lua.create_function(|lua, (pattern, text): (String, String)| {
        let re = regex::Regex::new(&pattern).map_err(mlua::Error::external)?;
        let parts: Vec<&str> = re.split(&text).collect();
        let table = lua.create_table()?;
        for (i, part) in parts.iter().enumerate() {
            table.set(i + 1, *part)?;
        }
        Ok(table)
    })?;
    regex_table.set("split", regex_split)?;

    lua.globals().set("regex", regex_table)?;

    // validate_json(json_string, schema_table) -> {valid=bool, errors=table}
    let validate_json_fn =
        lua.create_function(|lua, (data_str, schema_table): (String, mlua::Table)| {
            let data: serde_json::Value =
                serde_json::from_str(&data_str).map_err(mlua::Error::external)?;
            let schema = lua_table_to_json(&schema_table)?;

            let compiled = jsonschema::draft7::new(&schema)
                .map_err(|e| mlua::Error::external(format!("Invalid schema: {}", e)))?;

            let result_table = lua.create_table()?;

            match compiled.validate(&data) {
                Ok(()) => {
                    result_table.set("valid", true)?;
                    result_table.set("errors", lua.create_table()?)?;
                }
                Err(first_error) => {
                    result_table.set("valid", false)?;
                    let errors_table = lua.create_table()?;
                    let err = lua.create_table()?;
                    err.set("path", first_error.instance_path().to_string())?;
                    err.set("message", first_error.to_string())?;
                    errors_table.set(1, err)?;

                    for (i, error) in compiled.iter_errors(&data).skip(1).enumerate() {
                        let err = lua.create_table()?;
                        err.set("path", error.instance_path().to_string())?;
                        err.set("message", error.to_string())?;
                        errors_table.set(i + 2, err)?;
                    }
                    result_table.set("errors", errors_table)?;
                }
            }

            Ok(result_table)
        })?;
    lua.globals().set("validate_json", validate_json_fn)?;

    // http namespace — async HTTP client for Lua scripts
    let http_table = lua.create_table()?;

    // Shared reqwest client for all http.* calls
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("IronCrew/Lua")
        .build()
        .map_err(|e| mlua::Error::external(format!("Failed to create HTTP client: {}", e)))?;

    // http.get(url, options?) -> {status, headers, body}
    let client_get = client.clone();
    let http_get =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_get.clone();
            async move {
                let mut req = client.get(&url);
                req = apply_http_options(req, &options)?;
                execute_http_request(lua, req).await
            }
        })?;
    http_table.set("get", http_get)?;

    // http.post(url, options?) -> {status, headers, body}
    let client_post = client.clone();
    let http_post =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_post.clone();
            async move {
                let mut req = client.post(&url);
                req = apply_http_options(req, &options)?;
                if let Some(ref opts) = options {
                    req = apply_http_body(req, opts)?;
                }
                execute_http_request(lua, req).await
            }
        })?;
    http_table.set("post", http_post)?;

    // http.put(url, options?) -> {status, headers, body}
    let client_put = client.clone();
    let http_put =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_put.clone();
            async move {
                let mut req = client.put(&url);
                req = apply_http_options(req, &options)?;
                if let Some(ref opts) = options {
                    req = apply_http_body(req, opts)?;
                }
                execute_http_request(lua, req).await
            }
        })?;
    http_table.set("put", http_put)?;

    // http.delete(url, options?) -> {status, headers, body}
    let client_delete = client.clone();
    let http_delete =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_delete.clone();
            async move {
                let mut req = client.delete(&url);
                req = apply_http_options(req, &options)?;
                execute_http_request(lua, req).await
            }
        })?;
    http_table.set("delete", http_delete)?;

    // http.request(method, url, options?) -> {status, headers, body}
    let client_any = client;
    let http_request = lua.create_async_function(
        move |lua, (method, url, options): (String, String, Option<mlua::Table>)| {
            let client = client_any.clone();
            async move {
                let mut req = match method.to_uppercase().as_str() {
                    "GET" => client.get(&url),
                    "POST" => client.post(&url),
                    "PUT" => client.put(&url),
                    "DELETE" => client.delete(&url),
                    "PATCH" => client.patch(&url),
                    "HEAD" => client.head(&url),
                    other => {
                        return Err(mlua::Error::external(format!(
                            "Unsupported method: {}",
                            other
                        )));
                    }
                };
                req = apply_http_options(req, &options)?;
                if let Some(ref opts) = options {
                    req = apply_http_body(req, opts)?;
                }
                execute_http_request(lua, req).await
            }
        },
    )?;
    http_table.set("request", http_request)?;

    lua.globals().set("http", http_table)?;

    Ok(())
}

/// Apply headers and timeout from an options table to a request builder.
fn apply_http_options(
    mut req: reqwest::RequestBuilder,
    options: &Option<mlua::Table>,
) -> mlua::Result<reqwest::RequestBuilder> {
    if let Some(opts) = options {
        // Headers
        if let Ok(headers) = opts.get::<mlua::Table>("headers") {
            for pair in headers.pairs::<String, String>() {
                let (key, value) = pair?;
                req = req.header(key.as_str(), value.as_str());
            }
        }
        // Timeout override
        if let Ok(timeout_secs) = opts.get::<f64>("timeout") {
            req = req.timeout(std::time::Duration::from_secs_f64(timeout_secs));
        }
    }
    Ok(req)
}

/// Apply body from options table.
fn apply_http_body(
    mut req: reqwest::RequestBuilder,
    opts: &mlua::Table,
) -> mlua::Result<reqwest::RequestBuilder> {
    if let Ok(body) = opts.get::<String>("body") {
        // Auto-detect JSON
        if body.starts_with('{') || body.starts_with('[') {
            req = req.header("Content-Type", "application/json");
        }
        req = req.body(body);
    } else if let Ok(json_table) = opts.get::<mlua::Table>("json") {
        // Serialize Lua table as JSON body
        let json_value = lua_table_to_json(&json_table)?;
        let json_str = serde_json::to_string(&json_value)
            .map_err(|e| mlua::Error::external(format!("JSON serialize error: {}", e)))?;
        req = req
            .header("Content-Type", "application/json")
            .body(json_str);
    }
    Ok(req)
}

/// Execute an HTTP request and return the result as a Lua table.
async fn execute_http_request(lua: Lua, req: reqwest::RequestBuilder) -> mlua::Result<mlua::Table> {
    let resp = req.send().await.map_err(mlua::Error::external)?;

    let status = resp.status().as_u16();
    let headers_table = lua.create_table()?;
    for (key, value) in resp.headers() {
        if let Ok(v) = value.to_str() {
            headers_table.set(key.as_str(), v.to_string())?;
        }
    }

    let body_text = resp.text().await.map_err(mlua::Error::external)?;

    let result = lua.create_table()?;
    result.set("status", status)?;
    result.set("headers", headers_table)?;
    result.set("body", body_text.clone())?;

    // Try to parse as JSON for convenience
    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(&body_text) {
        let lua_value = json_value_to_lua(&lua, &json_value)?;
        result.set("json", lua_value)?;
    }

    result.set("ok", (200..300).contains(&status))?;

    Ok(result)
}

/// Create a Lua VM for crew.lua (full access context).
pub fn create_crew_lua() -> LuaResult<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::COROUTINE | StdLib::OS,
        mlua::LuaOptions::default(),
    )?;

    // Block dangerous os functions, keep os.clock and os.time
    lua.load(
        r#"
        local _os = os
        os = {
            clock = _os.clock,
            time = _os.time,
            date = _os.date,
            difftime = _os.difftime,
        }
        "#,
    )
    .exec()?;

    // Remove dangerous globals
    lua.load(
        r#"
        loadfile = nil
        dofile = nil
        "#,
    )
    .exec()?;

    register_lua_globals(&lua)?;

    Ok(lua)
}

/// Create a restricted Lua VM for tool execute functions.
/// Registers sandbox API: env(), and placeholders for llm, http, fs
/// (full llm/http/fs sandbox APIs will be wired when the tool is executed
/// with a provider context — see LuaScriptTool::execute).
pub fn create_tool_lua() -> LuaResult<Lua> {
    create_tool_lua_with_base_dir(None)
}

pub fn create_tool_lua_with_base_dir(base_dir: Option<PathBuf>) -> LuaResult<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH,
        mlua::LuaOptions::default(),
    )?;

    // Remove any potentially dangerous globals
    lua.load(
        r#"
        loadfile = nil
        dofile = nil
        require = nil
        os = nil
        io = nil
        "#,
    )
    .exec()?;

    register_lua_globals(&lua)?;

    if let Some(base_dir) = base_dir {
        let fs_table = lua.create_table()?;
        let read_base = base_dir.clone();
        let write_base = base_dir;

        let fs_read = lua.create_function(move |_, path: String| {
            let validated = validate_tool_fs_path(&read_base, &path)?;
            std::fs::read_to_string(&validated).map_err(mlua::Error::external)
        })?;
        let fs_write = lua.create_function(move |_, (path, content): (String, String)| {
            let validated = validate_tool_fs_path(&write_base, &path)?;
            std::fs::write(&validated, &content).map_err(mlua::Error::external)
        })?;

        fs_table.set("read", fs_read)?;
        fs_table.set("write", fs_write)?;
        lua.globals().set("fs", fs_table)?;
    }

    // Note: llm and http namespaces require async and a provider/client reference.
    // These are registered per-execution in LuaScriptTool::execute when a provider
    // is available. For v1, Lua tools that need llm:chat() or http:get() will need
    // to be wired in a future iteration with the async tool sandbox.

    Ok(lua)
}

fn validate_tool_fs_path(base_dir: &Path, path: &str) -> LuaResult<PathBuf> {
    let candidate = Path::new(path);
    if candidate.as_os_str().is_empty()
        || candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
                    | Component::CurDir
            )
        })
    {
        return Err(mlua::Error::external("invalid fs path"));
    }

    let joined = base_dir.join(candidate);

    // Canonicalize and verify containment to prevent symlink escapes
    if joined.exists() {
        let canonical = joined
            .canonicalize()
            .map_err(|e| mlua::Error::external(format!("failed to resolve path: {}", e)))?;
        let base_canonical = base_dir
            .canonicalize()
            .unwrap_or_else(|_| base_dir.to_path_buf());
        if !canonical.starts_with(&base_canonical) {
            return Err(mlua::Error::external("path escapes project directory"));
        }
        Ok(canonical)
    } else {
        // File doesn't exist yet (write case) — verify the parent stays in bounds
        if let Some(parent) = joined.parent()
            && parent.exists()
        {
            let parent_canonical = parent
                .canonicalize()
                .map_err(|e| mlua::Error::external(format!("failed to resolve path: {}", e)))?;
            let base_canonical = base_dir
                .canonicalize()
                .unwrap_or_else(|_| base_dir.to_path_buf());
            if !parent_canonical.starts_with(&base_canonical) {
                return Err(mlua::Error::external("path escapes project directory"));
            }
        }
        Ok(joined)
    }
}
