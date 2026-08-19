use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mlua::{Lua, LuaString, Result as LuaResult, StdLib, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::lua::json::{
    json_value_to_lua_with_limits, lua_table_to_json_with_limits, lua_value_to_json_with_limits,
};
use crate::lua::json_policy::JsonLimits;
use crate::lua::limits::install_lua_limits;
use crate::tools::runtime_policy::LuaVmPolicy;

mod eval_vm;
pub(crate) use eval_vm::{create_eval_lua, fresh_eval_environment};

// Regexes are cached per worker thread. Bound both the number and compiled
// program size so a flow cannot turn distinct patterns into persistent pod RSS.
const REGEX_CACHE_MAX: usize = 16;
const MAX_REGEX_PATTERN_BYTES: usize = 4 * 1024;
const MAX_REGEX_COMPILED_BYTES: usize = 512 * 1024;
const MAX_REGEX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_REGEX_REPLACEMENT_BYTES: usize = 64 * 1024;
const MAX_REGEX_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_REGEX_RESULT_ITEMS: usize = 10_000;
const MAX_REGEX_CAPTURE_GROUPS: usize = 128;
const MAX_LUA_JSON_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_LUA_JSON_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_LUA_BASE64_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_LUA_LOG_ARGS: usize = 64;
const MAX_LUA_LOG_BYTES: usize = 64 * 1024;
const MAX_LUA_TEMPLATE_BYTES: usize = 256 * 1024;
const MAX_LUA_TEMPLATE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_JSON_VALIDATION_ERRORS: usize = 100;
const MAX_JSON_VALIDATION_ERROR_BYTES: usize = 256 * 1024;
const MAX_JSON_VALIDATION_ERROR_FIELD_BYTES: usize = 8 * 1024;
const DEFAULT_LUA_FS_MAX_BYTES: usize = 1024 * 1024;
const HARD_LUA_FS_MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_LUA_HTTP_URL_BYTES: usize = 8 * 1024;
const MAX_LUA_HTTP_REQUEST_HEADERS: usize = 128;

thread_local! {
    static REGEX_CACHE: RefCell<HashMap<String, regex::Regex>> = RefCell::new(HashMap::new());
}

/// Get a cached compiled regex or compile and cache it.
fn get_or_compile_regex(pattern: &str) -> mlua::Result<regex::Regex> {
    if pattern.len() > MAX_REGEX_PATTERN_BYTES {
        return Err(mlua::Error::external(format!(
            "regex pattern exceeds the {MAX_REGEX_PATTERN_BYTES}-byte hard limit"
        )));
    }
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(re) = cache.get(pattern) {
            return Ok(re.clone());
        }
        let re = regex::RegexBuilder::new(pattern)
            .size_limit(MAX_REGEX_COMPILED_BYTES)
            .dfa_size_limit(MAX_REGEX_COMPILED_BYTES)
            .build()
            .map_err(mlua::Error::external)?;
        let capture_groups = re.captures_len().saturating_sub(1);
        if capture_groups > MAX_REGEX_CAPTURE_GROUPS {
            return Err(mlua::Error::external(format!(
                "regex contains {} capture groups; hard limit is {MAX_REGEX_CAPTURE_GROUPS}",
                capture_groups
            )));
        }
        if cache.len() >= REGEX_CACHE_MAX {
            // Evict all when full. This simple policy keeps the persistent
            // compiled-program footprint deterministic without another cache.
            cache.clear();
        }
        cache.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

fn ensure_input_limit(value: &str, limit: usize, context: &'static str) -> LuaResult<()> {
    if value.len() > limit {
        return Err(mlua::Error::external(format!(
            "{context} is {} bytes; hard limit is {limit} bytes",
            value.len()
        )));
    }
    Ok(())
}

fn add_bounded_len(
    used: &mut usize,
    addition: usize,
    limit: usize,
    context: &'static str,
) -> LuaResult<()> {
    *used = used.checked_add(addition).ok_or_else(|| {
        mlua::Error::external(format!("{context} length overflowed the platform size"))
    })?;
    if *used > limit {
        return Err(mlua::Error::external(format!(
            "{context} exceeds the {limit}-byte hard limit"
        )));
    }
    Ok(())
}

fn push_bounded(
    output: &mut String,
    value: &str,
    limit: usize,
    context: &'static str,
) -> LuaResult<()> {
    let mut next = output.len();
    add_bounded_len(&mut next, value.len(), limit, context)?;
    output.push_str(value);
    Ok(())
}

#[derive(Debug)]
struct LimitedOutputWriter {
    bytes: Vec<u8>,
    limit: usize,
    context: &'static str,
    exceeded: bool,
}

impl LimitedOutputWriter {
    fn new(limit: usize, capacity_hint: usize, context: &'static str) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity_hint.min(limit)),
            limit,
            context,
            exceeded: false,
        }
    }

    fn into_string(self) -> String {
        String::from_utf8(self.bytes).expect("JSON and Tera writers emit valid UTF-8")
    }
}

impl Write for LimitedOutputWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other(format!("{} length overflow", self.context)))?;
        if next > self.limit {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "{} exceeds the {}-byte hard limit",
                    self.context, self.limit
                ),
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_json_limited(value: &serde_json::Value) -> LuaResult<String> {
    let mut writer =
        LimitedOutputWriter::new(MAX_LUA_JSON_OUTPUT_BYTES, 1024, "json_stringify output");
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        mlua::Error::external(format!("Failed to serialize bounded JSON: {error}"))
    })?;
    Ok(writer.into_string())
}

fn join_lua_args(
    args: &[LuaString],
    start: usize,
    separator: &str,
    context: &'static str,
) -> LuaResult<String> {
    if args.len() > MAX_LUA_LOG_ARGS {
        return Err(mlua::Error::external(format!(
            "{context} accepts at most {MAX_LUA_LOG_ARGS} arguments"
        )));
    }
    let mut output = String::new();
    for (index, value) in args.iter().enumerate().skip(start) {
        let value = value.to_str()?;
        if index > start {
            push_bounded(&mut output, separator, MAX_LUA_LOG_BYTES, context)?;
        }
        push_bounded(&mut output, &value, MAX_LUA_LOG_BYTES, context)?;
    }
    Ok(output)
}

fn ensure_regex_text(text: &str) -> LuaResult<()> {
    ensure_input_limit(text, MAX_REGEX_TEXT_BYTES, "regex text")
}

fn ensure_regex_replacement(replacement: &str) -> LuaResult<()> {
    ensure_input_limit(
        replacement,
        MAX_REGEX_REPLACEMENT_BYTES,
        "regex replacement",
    )
}

#[derive(Clone, Copy, Debug)]
enum ReplacementCapture<'a> {
    Index(usize),
    Name(&'a str),
}

fn replacement_capture(value: &str) -> Option<(ReplacementCapture<'_>, usize)> {
    let bytes = value.as_bytes();
    if bytes.first() != Some(&b'$') || bytes.len() <= 1 {
        return None;
    }
    if bytes[1] == b'{' {
        let close = value[2..].find('}')? + 2;
        let name = &value[2..close];
        let capture = name
            .parse::<usize>()
            .map(ReplacementCapture::Index)
            .unwrap_or(ReplacementCapture::Name(name));
        return Some((capture, close + 1));
    }

    let end = bytes[1..]
        .iter()
        .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
        .map(|offset| offset + 1)
        .unwrap_or(bytes.len());
    if end == 1 {
        return None;
    }
    let name = &value[1..end];
    let capture = name
        .parse::<usize>()
        .map(ReplacementCapture::Index)
        .unwrap_or(ReplacementCapture::Name(name));
    Some((capture, end))
}

fn expanded_replacement_len(captures: &regex::Captures<'_>, replacement: &str) -> LuaResult<usize> {
    let mut remaining = replacement;
    let mut output_len = 0usize;
    while let Some(offset) = remaining.find('$') {
        add_bounded_len(
            &mut output_len,
            offset,
            MAX_REGEX_RESULT_BYTES,
            "regex replacement result",
        )?;
        remaining = &remaining[offset..];
        if remaining.as_bytes().get(1) == Some(&b'$') {
            add_bounded_len(
                &mut output_len,
                1,
                MAX_REGEX_RESULT_BYTES,
                "regex replacement result",
            )?;
            remaining = &remaining[2..];
            continue;
        }
        let Some((capture, consumed)) = replacement_capture(remaining) else {
            add_bounded_len(
                &mut output_len,
                1,
                MAX_REGEX_RESULT_BYTES,
                "regex replacement result",
            )?;
            remaining = &remaining[1..];
            continue;
        };
        let matched = match capture {
            ReplacementCapture::Index(index) => captures.get(index),
            ReplacementCapture::Name(name) => captures.name(name),
        };
        if let Some(matched) = matched {
            add_bounded_len(
                &mut output_len,
                matched.as_str().len(),
                MAX_REGEX_RESULT_BYTES,
                "regex replacement result",
            )?;
        }
        remaining = &remaining[consumed..];
    }
    add_bounded_len(
        &mut output_len,
        remaining.len(),
        MAX_REGEX_RESULT_BYTES,
        "regex replacement result",
    )?;
    Ok(output_len)
}

fn replace_regex_bounded(
    regex: &regex::Regex,
    text: &str,
    replacement: &str,
    replace_all: bool,
) -> LuaResult<String> {
    ensure_regex_text(text)?;
    ensure_regex_replacement(replacement)?;

    let mut output = String::with_capacity(text.len().min(MAX_REGEX_RESULT_BYTES));
    let mut previous_end = 0usize;
    let mut matches = 0usize;
    for captures in regex.captures_iter(text) {
        matches = matches.saturating_add(1);
        if matches > MAX_REGEX_RESULT_ITEMS {
            return Err(mlua::Error::external(format!(
                "regex replacement exceeds the {MAX_REGEX_RESULT_ITEMS}-match hard limit"
            )));
        }
        let matched = captures
            .get(0)
            .expect("regex capture iterator always includes the complete match");
        push_bounded(
            &mut output,
            &text[previous_end..matched.start()],
            MAX_REGEX_RESULT_BYTES,
            "regex replacement result",
        )?;
        let expansion_len = expanded_replacement_len(&captures, replacement)?;
        let mut projected = output.len();
        add_bounded_len(
            &mut projected,
            expansion_len,
            MAX_REGEX_RESULT_BYTES,
            "regex replacement result",
        )?;
        let before = output.len();
        captures.expand(replacement, &mut output);
        debug_assert_eq!(output.len().saturating_sub(before), expansion_len);
        previous_end = matched.end();
        if !replace_all {
            break;
        }
    }
    push_bounded(
        &mut output,
        &text[previous_end..],
        MAX_REGEX_RESULT_BYTES,
        "regex replacement result",
    )?;
    Ok(output)
}

fn format_display_limited(value: &impl std::fmt::Display) -> String {
    struct Writer {
        value: String,
        truncated: bool,
    }
    impl std::fmt::Write for Writer {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            let remaining = MAX_JSON_VALIDATION_ERROR_FIELD_BYTES.saturating_sub(self.value.len());
            if value.len() > remaining {
                self.value
                    .push_str(crate::utils::http::utf8_prefix(value, remaining));
                self.truncated = true;
                return Err(std::fmt::Error);
            }
            self.value.push_str(value);
            Ok(())
        }
    }

    let mut writer = Writer {
        value: String::new(),
        truncated: false,
    };
    let _ = std::fmt::write(&mut writer, format_args!("{value}"));
    if writer.truncated {
        const MARKER: &str = "...";
        let keep = MAX_JSON_VALIDATION_ERROR_FIELD_BYTES.saturating_sub(MARKER.len());
        writer
            .value
            .truncate(crate::utils::http::utf8_prefix(&writer.value, keep).len());
        writer.value.push_str(MARKER);
    }
    writer.value
}

fn lua_fs_limit(name: &str) -> LuaResult<usize> {
    let value = match std::env::var(name) {
        Ok(raw) => raw.parse::<usize>().map_err(|_| {
            mlua::Error::external(format!(
                "{name} must be an integer from 1 to {HARD_LUA_FS_MAX_BYTES}"
            ))
        })?,
        Err(_) => DEFAULT_LUA_FS_MAX_BYTES,
    };
    if !(1..=HARD_LUA_FS_MAX_BYTES).contains(&value) {
        return Err(mlua::Error::external(format!(
            "{name} must be from 1 to {HARD_LUA_FS_MAX_BYTES}"
        )));
    }
    Ok(value)
}

/// Register utility global functions available in all Lua sandboxes.
#[cfg(test)]
pub fn register_lua_globals(lua: &Lua) -> LuaResult<()> {
    let policy = crate::tools::runtime_policy::RuntimeExecutionPolicy::capture()
        .lua_vm_policy()
        .map_err(mlua::Error::external)?;
    register_lua_globals_with_policy(lua, policy)
}

fn register_lua_globals_with_policy(lua: &Lua, vm_policy: LuaVmPolicy) -> LuaResult<()> {
    let http_policy = vm_policy.http();
    let json_limits = vm_policy.json_limits();
    let env_policy = vm_policy.env();
    // env() — fail-closed allowlist. Lua can read ONLY the environment
    // variables whose exact names appear in IRONCREW_ENV_ALLOWLIST
    // (comma-separated, case-insensitive). Every other name returns nil.
    //
    // This replaces the earlier suffix denylist, which was fail-open and
    // silently leaked credentials it didn't anticipate — e.g.
    // AWS_SECRET_ACCESS_KEY (ends `_ACCESS_KEY`, not `_API_KEY`),
    // AWS_ACCESS_KEY_ID, GOOGLE_APPLICATION_CREDENTIALS, and anything ending
    // `_KEY`/`_CREDENTIALS`. An allowlist matches the fail-closed posture of
    // the rest of the sandbox: operators opt specific, non-secret vars in per
    // deployment (e.g. `IRONCREW_ENV_ALLOWLIST=APP_REGION,FEATURE_FLAGS`).
    let env_fn = lua.create_function(move |_, name: LuaString| {
        if env_policy == crate::tools::runtime_policy::LuaEnvPolicy::PersistentConversationBlocked {
            return Err(mlua::Error::external(
                "env() is unavailable in persistent conversation tool and subflow execution",
            ));
        }
        let name = name.to_str()?;
        ensure_input_limit(&name, 256, "environment variable name")?;
        let value = crate::utils::env::read_allowlisted(name.as_ref());
        if value.is_none() {
            tracing::warn!("Lua environment access blocked by IRONCREW_ENV_ALLOWLIST");
        }
        Ok(value)
    })?;
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
    let json_parse_fn = lua.create_function(move |lua, s: LuaString| {
        let s = s.to_str()?;
        ensure_input_limit(&s, MAX_LUA_JSON_INPUT_BYTES, "json_parse input")?;
        let value: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
        json_value_to_lua_with_limits(lua, &value, json_limits)
    })?;
    lua.globals().set("json_parse", json_parse_fn)?;

    // json_stringify(value) -> JSON string
    let json_stringify_fn = lua.create_function(move |_, value: Value| {
        let json = lua_value_to_json_with_limits(value, json_limits)?;
        serialize_json_limited(&json)
    })?;
    lua.globals().set("json_stringify", json_stringify_fn)?;

    // base64_encode(str)
    let b64_encode_fn = lua.create_function(|_, s: LuaString| {
        let bytes = s.as_bytes();
        if bytes.len() > MAX_LUA_BASE64_INPUT_BYTES {
            return Err(mlua::Error::external(format!(
                "base64_encode input is {} bytes; hard limit is {MAX_LUA_BASE64_INPUT_BYTES} bytes",
                bytes.len()
            )));
        }
        Ok(STANDARD.encode(bytes))
    })?;
    lua.globals().set("base64_encode", b64_encode_fn)?;

    // base64_decode(str)
    let b64_decode_fn = lua.create_function(|_, s: LuaString| {
        let bytes = s.as_bytes();
        if bytes.len() > MAX_LUA_BASE64_INPUT_BYTES {
            return Err(mlua::Error::external(format!(
                "base64_decode input is {} bytes; hard limit is {MAX_LUA_BASE64_INPUT_BYTES} bytes",
                bytes.len()
            )));
        }
        let bytes = STANDARD.decode(bytes).map_err(mlua::Error::external)?;
        String::from_utf8(bytes).map_err(mlua::Error::external)
    })?;
    lua.globals().set("base64_decode", b64_decode_fn)?;

    // base64_decode_bytes(str) — like base64_decode but returns the raw
    // decoded bytes as a Lua string without UTF-8 validation. Needed to parse
    // binary layouts (e.g. salt||iv||ciphertext blobs) when using the
    // low-level crypto primitives below.
    let b64_decode_bytes_fn = lua.create_function(|lua, s: LuaString| {
        let s = s.to_str()?;
        ensure_input_limit(&s, MAX_LUA_BASE64_INPUT_BYTES, "base64_decode_bytes input")?;
        let bytes = STANDARD.decode(s.trim()).map_err(mlua::Error::external)?;
        lua.create_string(&bytes)
    })?;
    lua.globals()
        .set("base64_decode_bytes", b64_decode_bytes_fn)?;

    // Symmetric-crypto primitives (PBKDF2-HMAC-SHA256 + AES-256-GCM decrypt).
    // Lets flows decrypt secrets read from a store at runtime without the
    // caller passing plaintext keys through the run input.
    crate::lua::crypto::register_crypto_globals(lua)?;

    // log(level, msg...) — also emits to EventBus if available
    let log_fn = lua.create_function(|lua, args: mlua::Variadic<LuaString>| {
        if args.is_empty() {
            return Ok(());
        }
        if args.len() > MAX_LUA_LOG_ARGS {
            return Err(mlua::Error::external(format!(
                "log accepts at most {MAX_LUA_LOG_ARGS} arguments"
            )));
        }

        let (level, message) = if args.len() >= 2 {
            let lvl = args[0].to_str()?;
            let msg = join_lua_args(&args, 1, " ", "Lua log message")?;
            (lvl.to_string(), msg)
        } else {
            let message = args[0].to_str()?;
            ensure_input_limit(&message, MAX_LUA_LOG_BYTES, "Lua log message")?;
            ("info".to_string(), message.to_string())
        };
        ensure_input_limit(&level, 16, "Lua log level")?;

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
    let print_fn = lua.create_function(|lua, args: mlua::Variadic<LuaString>| {
        let message = join_lua_args(&args, 0, "\t", "Lua print message")?;

        if let Some(eventbus) = lua.app_data_ref::<crate::engine::eventbus::EventBus>() {
            // API mode: send to SSE only, don't pollute server stdout
            eventbus.emit(crate::engine::eventbus::CrewEvent::Log {
                level: "info".into(),
                message,
            });
        } else if lua
            .app_data_ref::<crate::cli::commands::JsonOutputMode>()
            .is_some()
        {
            // --json mode: suppress print, structured output comes from run record
        } else {
            // CLI mode: print to stdout
            println!("{}", message);
        }

        Ok(())
    })?;
    lua.globals().set("print", print_fn)?;

    // regex namespace — Rust regex engine exposed to Lua (with compiled pattern caching)
    let regex_table = lua.create_table()?;

    // regex.match(pattern, text) -> bool
    let regex_match = lua.create_function(|_, (pattern, text): (LuaString, LuaString)| {
        let pattern = pattern.to_str()?;
        let text = text.to_str()?;
        ensure_regex_text(&text)?;
        let re = get_or_compile_regex(&pattern)?;
        Ok(re.is_match(&text))
    })?;
    regex_table.set("match", regex_match)?;

    // regex.find(pattern, text) -> string|nil (first match)
    let regex_find = lua.create_function(|_, (pattern, text): (LuaString, LuaString)| {
        let pattern = pattern.to_str()?;
        let text = text.to_str()?;
        ensure_regex_text(&text)?;
        let re = get_or_compile_regex(&pattern)?;
        Ok(re.find(&text).map(|matched| matched.as_str().to_string()))
    })?;
    regex_table.set("find", regex_find)?;

    // regex.find_all(pattern, text) -> table of strings (all matches)
    let regex_find_all = lua.create_function(|lua, (pattern, text): (LuaString, LuaString)| {
        let pattern = pattern.to_str()?;
        let text = text.to_str()?;
        ensure_regex_text(&text)?;
        let re = get_or_compile_regex(&pattern)?;
        let table = lua.create_table_with_capacity(MAX_REGEX_RESULT_ITEMS.min(64), 0)?;
        let mut result_bytes = 0usize;
        for (index, matched) in re.find_iter(&text).enumerate() {
            if index >= MAX_REGEX_RESULT_ITEMS {
                return Err(mlua::Error::external(format!(
                    "regex.find_all exceeds the {MAX_REGEX_RESULT_ITEMS}-item hard limit"
                )));
            }
            add_bounded_len(
                &mut result_bytes,
                matched.as_str().len(),
                MAX_REGEX_RESULT_BYTES,
                "regex.find_all result",
            )?;
            table.set(index + 1, matched.as_str())?;
        }
        Ok(table)
    })?;
    regex_table.set("find_all", regex_find_all)?;

    // regex.captures(pattern, text) -> table of capture groups|nil
    let regex_captures = lua.create_function(|lua, (pattern, text): (LuaString, LuaString)| {
        let pattern = pattern.to_str()?;
        let text = text.to_str()?;
        ensure_regex_text(&text)?;
        let re = get_or_compile_regex(&pattern)?;
        match re.captures(&text) {
            Some(caps) => {
                let table = lua.create_table_with_capacity(
                    re.captures_len(),
                    re.capture_names().flatten().count(),
                )?;
                let mut result_bytes = 0usize;
                for (i, cap) in caps.iter().enumerate() {
                    if let Some(m) = cap {
                        add_bounded_len(
                            &mut result_bytes,
                            m.as_str().len(),
                            MAX_REGEX_RESULT_BYTES,
                            "regex.captures result",
                        )?;
                        table.set(i, m.as_str())?;
                    }
                }
                // Also set named captures
                for name in re.capture_names().flatten() {
                    if let Some(m) = caps.name(name) {
                        add_bounded_len(
                            &mut result_bytes,
                            name.len().saturating_add(m.as_str().len()),
                            MAX_REGEX_RESULT_BYTES,
                            "regex.captures result",
                        )?;
                        table.set(name, m.as_str())?;
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
        |_, (pattern, text, replacement): (LuaString, LuaString, LuaString)| {
            let pattern = pattern.to_str()?;
            let text = text.to_str()?;
            let replacement = replacement.to_str()?;
            let re = get_or_compile_regex(&pattern)?;
            replace_regex_bounded(&re, &text, &replacement, false)
        },
    )?;
    regex_table.set("replace", regex_replace)?;

    // regex.replace_all(pattern, text, replacement) -> string
    let regex_replace_all = lua.create_function(
        |_, (pattern, text, replacement): (LuaString, LuaString, LuaString)| {
            let pattern = pattern.to_str()?;
            let text = text.to_str()?;
            let replacement = replacement.to_str()?;
            let re = get_or_compile_regex(&pattern)?;
            replace_regex_bounded(&re, &text, &replacement, true)
        },
    )?;
    regex_table.set("replace_all", regex_replace_all)?;

    // regex.split(pattern, text) -> table of strings
    let regex_split = lua.create_function(|lua, (pattern, text): (LuaString, LuaString)| {
        let pattern = pattern.to_str()?;
        let text = text.to_str()?;
        ensure_regex_text(&text)?;
        let re = get_or_compile_regex(&pattern)?;
        let table = lua.create_table_with_capacity(MAX_REGEX_RESULT_ITEMS.min(64), 0)?;
        let mut result_bytes = 0usize;
        for (index, part) in re.split(&text).enumerate() {
            if index >= MAX_REGEX_RESULT_ITEMS {
                return Err(mlua::Error::external(format!(
                    "regex.split exceeds the {MAX_REGEX_RESULT_ITEMS}-item hard limit"
                )));
            }
            add_bounded_len(
                &mut result_bytes,
                part.len(),
                MAX_REGEX_RESULT_BYTES,
                "regex.split result",
            )?;
            table.set(index + 1, part)?;
        }
        Ok(table)
    })?;
    regex_table.set("split", regex_split)?;

    lua.globals().set("regex", regex_table)?;

    // validate_json(json_string, schema_table) -> {valid=bool, errors=table}
    let validate_json_fn = lua.create_function(
        move |lua, (data_str, schema_table): (LuaString, mlua::Table)| {
            let data_str = data_str.to_str()?;
            ensure_input_limit(&data_str, MAX_LUA_JSON_INPUT_BYTES, "validate_json input")?;
            let data: serde_json::Value =
                serde_json::from_str(&data_str).map_err(mlua::Error::external)?;
            let schema = lua_table_to_json_with_limits(&schema_table, json_limits)?;

            let compiled = crate::tools::validate_schema::compile_local_draft7(&schema)
                .map_err(|e| mlua::Error::external(format!("Invalid schema: {}", e)))?;

            let result_table = lua.create_table()?;

            if compiled.validate(&data).is_ok() {
                result_table.set("valid", true)?;
                result_table.set("errors", lua.create_table()?)?;
            } else {
                const OMITTED_MESSAGE: &str =
                    "additional validation errors omitted by sandbox resource limits";
                result_table.set("valid", false)?;
                let errors_table = lua.create_table_with_capacity(MAX_JSON_VALIDATION_ERRORS, 0)?;
                let mut output_bytes = 0usize;
                let mut written = 0usize;
                let mut omitted = false;

                for error in compiled.iter_errors(&data) {
                    if written >= MAX_JSON_VALIDATION_ERRORS {
                        omitted = true;
                        break;
                    }
                    let path = format_display_limited(error.instance_path());
                    let message = format_display_limited(&error);
                    let entry_bytes = path.len().saturating_add(message.len());
                    let budget_without_marker =
                        MAX_JSON_VALIDATION_ERROR_BYTES.saturating_sub(OMITTED_MESSAGE.len());
                    if output_bytes.saturating_add(entry_bytes) > budget_without_marker {
                        omitted = true;
                        break;
                    }
                    output_bytes += entry_bytes;
                    written += 1;
                    let entry = lua.create_table()?;
                    entry.set("path", path)?;
                    entry.set("message", message)?;
                    errors_table.set(written, entry)?;
                }

                if omitted {
                    let entry = lua.create_table()?;
                    entry.set("path", "")?;
                    entry.set("message", OMITTED_MESSAGE)?;
                    errors_table.set(written + 1, entry)?;
                }
                result_table.set("errors", errors_table)?;
            }

            Ok(result_table)
        },
    )?;
    lua.globals().set("validate_json", validate_json_fn)?;

    // template(template_string, data_table) -> rendered string
    let template_fn = lua.create_function(move |_, (tpl, data): (LuaString, mlua::Table)| {
        let tpl = tpl.to_str()?;
        ensure_input_limit(&tpl, MAX_LUA_TEMPLATE_BYTES, "template source")?;
        let json_data = lua_table_to_json_with_limits(&data, json_limits)?;
        let mut tera = tera::Tera::default();
        tera.add_raw_template("inline", &tpl)
            .map_err(|e| mlua::Error::external(format!("Template error: {}", e)))?;
        let context = tera::Context::from_serialize(&json_data)
            .map_err(|e| mlua::Error::external(format!("Template context error: {}", e)))?;
        let mut writer =
            LimitedOutputWriter::new(MAX_LUA_TEMPLATE_OUTPUT_BYTES, tpl.len(), "template output");
        let render_result = tera.render_to("inline", &context, &mut writer);
        if writer.exceeded {
            return Err(mlua::Error::external(format!(
                "template output exceeds the {MAX_LUA_TEMPLATE_OUTPUT_BYTES}-byte hard limit"
            )));
        }
        render_result.map_err(|e| mlua::Error::external(format!("Template render error: {e}")))?;
        Ok(writer.into_string())
    })?;
    lua.globals().set("template", template_fn)?;

    // http namespace — async HTTP client for Lua scripts
    let http_table = lua.create_table()?;

    let client = crate::tools::http_request::client_for_policy(&http_policy);

    // http.get(url, options?) -> {status, headers, body}
    let client_get = client.clone();
    let policy_get = http_policy.clone();
    let http_get =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_get.clone();
            let policy = policy_get.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "http.get")?;
                let mut req = client.get(&url);
                req = apply_http_options(req, &options, &policy)?;
                validate_lua_url(&url, &policy)?;
                execute_http_request(lua, req, &policy, json_limits).await
            }
        })?;
    http_table.set("get", http_get)?;

    // http.post(url, options?) -> {status, headers, body}
    let client_post = client.clone();
    let policy_post = http_policy.clone();
    let http_post =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_post.clone();
            let policy = policy_post.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "http.post")?;
                let mut req = client.post(&url);
                req = apply_http_options(req, &options, &policy)?;
                if let Some(ref opts) = options {
                    req = apply_http_body(req, opts, &policy, json_limits)?;
                }
                validate_lua_url(&url, &policy)?;
                execute_http_request(lua, req, &policy, json_limits).await
            }
        })?;
    http_table.set("post", http_post)?;

    // http.put(url, options?) -> {status, headers, body}
    let client_put = client.clone();
    let policy_put = http_policy.clone();
    let http_put =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_put.clone();
            let policy = policy_put.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "http.put")?;
                let mut req = client.put(&url);
                req = apply_http_options(req, &options, &policy)?;
                if let Some(ref opts) = options {
                    req = apply_http_body(req, opts, &policy, json_limits)?;
                }
                validate_lua_url(&url, &policy)?;
                execute_http_request(lua, req, &policy, json_limits).await
            }
        })?;
    http_table.set("put", http_put)?;

    // http.delete(url, options?) -> {status, headers, body}
    let client_delete = client.clone();
    let policy_delete = http_policy.clone();
    let http_delete =
        lua.create_async_function(move |lua, (url, options): (String, Option<mlua::Table>)| {
            let client = client_delete.clone();
            let policy = policy_delete.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "http.delete")?;
                let mut req = client.delete(&url);
                req = apply_http_options(req, &options, &policy)?;
                validate_lua_url(&url, &policy)?;
                execute_http_request(lua, req, &policy, json_limits).await
            }
        })?;
    http_table.set("delete", http_delete)?;

    // http.request(method, url, options?) -> {status, headers, body}
    let client_any = client;
    let policy_any = http_policy;
    let http_request = lua.create_async_function(
        move |lua, (method, url, options): (String, String, Option<mlua::Table>)| {
            let client = client_any.clone();
            let policy = policy_any.clone();
            async move {
                crate::lua::bootstrap::reject_effect(&lua, "http.request")?;
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
                req = apply_http_options(req, &options, &policy)?;
                if let Some(ref opts) = options {
                    req = apply_http_body(req, opts, &policy, json_limits)?;
                }
                validate_lua_url(&url, &policy)?;
                execute_http_request(lua, req, &policy, json_limits).await
            }
        },
    )?;
    http_table.set("request", http_request)?;

    lua.globals().set("http", http_table)?;

    // Sandbox-level `run_flow(path, input)` — lets any Lua VM (crew.lua,
    // custom tools, conversation tool-call handlers) delegate to a sub-flow.
    // Registration is unconditional; the function itself errors out at call
    // time if the VM lacks the runtime/project_dir app-data (parse-time VMs).
    crate::lua::subflow::register_run_flow(lua)?;

    Ok(())
}

/// Apply headers and timeout from an options table to a request builder.
fn apply_http_options(
    mut req: reqwest::RequestBuilder,
    options: &Option<mlua::Table>,
    policy: &crate::tools::http_request::HttpToolPolicy,
) -> mlua::Result<reqwest::RequestBuilder> {
    if let Some(opts) = options {
        // Headers
        let headers_value = opts.raw_get::<mlua::Value>("headers")?;
        if !matches!(headers_value, mlua::Value::Nil) {
            let mlua::Value::Table(headers) = headers_value else {
                return Err(mlua::Error::external("HTTP headers must be a table"));
            };
            let byte_limit = policy.request_header_bytes();
            let mut count = 0usize;
            let mut bytes = 0usize;
            for pair in headers.pairs::<mlua::Value, mlua::Value>() {
                let (key, value) = pair?;
                count = count.saturating_add(1);
                if count > MAX_LUA_HTTP_REQUEST_HEADERS {
                    return Err(mlua::Error::external(format!(
                        "HTTP headers exceed the {MAX_LUA_HTTP_REQUEST_HEADERS}-entry limit"
                    )));
                }
                let mlua::Value::String(key) = key else {
                    return Err(mlua::Error::external("HTTP header names must be strings"));
                };
                let mlua::Value::String(value) = value else {
                    return Err(mlua::Error::external("HTTP header values must be strings"));
                };
                let key = key.to_str()?;
                let value = value.to_str()?;
                bytes = bytes
                    .checked_add(key.len())
                    .and_then(|total| total.checked_add(value.len()))
                    .ok_or_else(|| mlua::Error::external("HTTP headers are too large"))?;
                if bytes > byte_limit {
                    return Err(mlua::Error::external(format!(
                        "HTTP headers exceed IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES ({byte_limit})"
                    )));
                }
                let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .map_err(mlua::Error::external)?;
                let value = reqwest::header::HeaderValue::from_str(value.as_ref())
                    .map_err(mlua::Error::external)?;
                req = req.header(name, value);
            }
        }
        // Timeout override
        let timeout_value = opts.get::<mlua::Value>("timeout")?;
        if !matches!(timeout_value, mlua::Value::Nil) {
            let timeout_secs = match timeout_value {
                mlua::Value::Integer(value) => value as f64,
                mlua::Value::Number(value) => value,
                _ => {
                    return Err(mlua::Error::external(
                        "HTTP timeout must be a number of seconds",
                    ));
                }
            };
            const MAX_HTTP_TIMEOUT_SECS: f64 = 300.0;
            if !timeout_secs.is_finite()
                || timeout_secs <= 0.0
                || timeout_secs > MAX_HTTP_TIMEOUT_SECS
            {
                return Err(mlua::Error::external(format!(
                    "HTTP timeout must be finite and greater than 0, up to {MAX_HTTP_TIMEOUT_SECS} seconds"
                )));
            }
            req = req.timeout(std::time::Duration::from_secs_f64(timeout_secs));
        }
    }
    Ok(req)
}

/// Apply body from options table.
fn apply_http_body(
    mut req: reqwest::RequestBuilder,
    opts: &mlua::Table,
    policy: &crate::tools::http_request::HttpToolPolicy,
    json_limits: JsonLimits,
) -> mlua::Result<reqwest::RequestBuilder> {
    let body_value = opts.raw_get::<mlua::Value>("body")?;
    let json_value = opts.raw_get::<mlua::Value>("json")?;
    if !matches!(body_value, mlua::Value::Nil) && !matches!(json_value, mlua::Value::Nil) {
        return Err(mlua::Error::external(
            "HTTP options must provide only one of body or json",
        ));
    }
    let body_limit = policy.request_body_bytes();
    match body_value {
        mlua::Value::Nil => {}
        mlua::Value::String(body) => {
            let body = body.to_str()?;
            if body.len() > body_limit {
                return Err(mlua::Error::external(format!(
                    "HTTP body exceeds IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES ({body_limit})"
                )));
            }
            if body.starts_with('{') || body.starts_with('[') {
                req = req.header("Content-Type", "application/json");
            }
            req = req.body(body.to_owned());
        }
        _ => return Err(mlua::Error::external("HTTP body must be a string")),
    }
    match json_value {
        mlua::Value::Nil => {}
        mlua::Value::Table(json_table) => {
            let json_value = lua_table_to_json_with_limits(&json_table, json_limits)?;
            let json_string = crate::utils::http::to_json_pretty_limited(&json_value, body_limit)
                .map_err(|error| {
                mlua::Error::external(format!(
                    "JSON body exceeds IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES ({body_limit}): {error}"
                ))
            })?;
            req = req
                .header("Content-Type", "application/json")
                .body(json_string);
        }
        _ => return Err(mlua::Error::external("HTTP json must be a table")),
    }
    Ok(req)
}

/// Validate a URL for SSRF before making a request from Lua.
fn validate_lua_url(
    url: &str,
    policy: &crate::tools::http_request::HttpToolPolicy,
) -> mlua::Result<()> {
    if url.len() > MAX_LUA_HTTP_URL_BYTES {
        return Err(mlua::Error::external(format!(
            "HTTP URL exceeds the {MAX_LUA_HTTP_URL_BYTES}-byte hard limit"
        )));
    }
    crate::utils::network::validate_url_with_private_access(
        url,
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
        policy.allow_private(),
    )
    .map_err(|e| mlua::Error::external(format!("SSRF blocked: {}", e)))
}

/// Execute an HTTP request and return the result as a Lua table.
async fn execute_http_request(
    lua: Lua,
    req: reqwest::RequestBuilder,
    policy: &crate::tools::http_request::HttpToolPolicy,
    json_limits: JsonLimits,
) -> mlua::Result<mlua::Table> {
    let resp = req.send().await.map_err(mlua::Error::external)?;

    let status = resp.status().as_u16();
    let max_header_bytes = policy.response_header_bytes();
    let headers = crate::utils::http::collect_response_headers(
        resp.headers(),
        max_header_bytes,
        "Lua HTTP response headers",
    )
    .map_err(mlua::Error::external)?;
    let headers_table = lua.create_table()?;
    for (key, value) in headers {
        headers_table.set(key, value)?;
    }

    let max_response_size = policy.response_bytes();
    let body_bytes =
        crate::utils::http::read_response_bytes(resp, max_response_size, "Lua HTTP response")
            .await
            .map_err(mlua::Error::external)?;
    let body_text = match String::from_utf8(body_bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    };

    // Converting JSON into both serde and Lua trees amplifies memory beyond the
    // raw body. Keep the convenience field for bounded responses only; callers
    // always retain the body string.
    let max_json_bytes = policy.json_bytes();
    let json_value = if body_text.len() <= max_json_bytes {
        serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .map(|value| json_value_to_lua_with_limits(&lua, &value, json_limits))
            .transpose()?
    } else {
        None
    };

    let result = lua.create_table()?;
    result.set("status", status)?;
    result.set("headers", headers_table)?;
    result.set("body", body_text)?;

    // Try to parse as JSON for convenience
    if let Some(json_value) = json_value {
        result.set("json", json_value)?;
    }

    result.set("ok", (200..300).contains(&status))?;

    Ok(result)
}

/// Create a Lua VM for crew.lua (full access context), with no `require`
/// library directories. `require` is unavailable in this variant.
pub fn create_crew_lua() -> LuaResult<Lua> {
    create_crew_lua_with_lib_dirs(Vec::new())
}

/// Create a crew VM whose `require` resolves Lua-source modules from `lib_dirs`
/// (typically `<flow-dir>/_lib`). When `lib_dirs` is empty, `require` is not
/// installed.
pub fn create_crew_lua_with_lib_dirs(lib_dirs: Vec<PathBuf>) -> LuaResult<Lua> {
    let policy = crate::tools::runtime_policy::RuntimeExecutionPolicy::capture()
        .lua_vm_policy()
        .map_err(mlua::Error::external)?;
    create_crew_lua_with_policy(lib_dirs, policy)
}

pub(crate) fn create_crew_lua_with_policy(
    lib_dirs: Vec<PathBuf>,
    vm_policy: LuaVmPolicy,
) -> LuaResult<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::COROUTINE | StdLib::OS,
        mlua::LuaOptions::default(),
    )?;
    install_lua_limits(&lua, vm_policy.limits())?;

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

    register_lua_globals_with_policy(&lua, vm_policy)?;
    crate::lua::require::install_require(&lua, lib_dirs)?;

    Ok(lua)
}

/// Create a restricted Lua VM for tool execute functions.
/// Registers sandbox API: env(), and placeholders for llm, http, fs
/// (full llm/http/fs sandbox APIs will be wired when the tool is executed
/// with a provider context — see LuaScriptTool::execute).
pub fn create_tool_lua() -> LuaResult<Lua> {
    create_tool_lua_with_fs_roots(None, None)
}

#[cfg(test)]
pub fn create_tool_lua_with_base_dir(base_dir: Option<PathBuf>) -> LuaResult<Lua> {
    create_tool_lua_with_fs_roots(base_dir.clone(), base_dir)
}

pub fn create_tool_lua_with_fs_roots(
    read_base_dir: Option<PathBuf>,
    write_base_dir: Option<PathBuf>,
) -> LuaResult<Lua> {
    let (read_limit, write_limit) = if read_base_dir.is_some() || write_base_dir.is_some() {
        (
            lua_fs_limit("IRONCREW_LUA_FS_MAX_READ_BYTES")?,
            lua_fs_limit("IRONCREW_LUA_FS_MAX_WRITE_BYTES")?,
        )
    } else {
        (DEFAULT_LUA_FS_MAX_BYTES, DEFAULT_LUA_FS_MAX_BYTES)
    };
    create_tool_lua_with_fs_policy(read_base_dir, write_base_dir, read_limit, write_limit)
}

pub(crate) fn create_tool_lua_with_fs_policy(
    read_base_dir: Option<PathBuf>,
    write_base_dir: Option<PathBuf>,
    read_limit: usize,
    write_limit: usize,
) -> LuaResult<Lua> {
    create_tool_lua_with_execution_policy(
        read_base_dir,
        write_base_dir,
        read_limit,
        write_limit,
        crate::tools::runtime_policy::RuntimeExecutionPolicy::capture()
            .lua_vm_policy()
            .map_err(mlua::Error::external)?,
    )
}

pub(crate) fn create_tool_lua_with_execution_policy(
    read_base_dir: Option<PathBuf>,
    write_base_dir: Option<PathBuf>,
    read_limit: usize,
    write_limit: usize,
    vm_policy: LuaVmPolicy,
) -> LuaResult<Lua> {
    if !(1..=HARD_LUA_FS_MAX_BYTES).contains(&read_limit)
        || !(1..=HARD_LUA_FS_MAX_BYTES).contains(&write_limit)
    {
        return Err(mlua::Error::external(
            "Lua filesystem policy is outside its hard bounds",
        ));
    }
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH,
        mlua::LuaOptions::default(),
    )?;
    install_lua_limits(&lua, vm_policy.limits())?;

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

    register_lua_globals_with_policy(&lua, vm_policy)?;

    if read_base_dir.is_some() || write_base_dir.is_some() {
        let fs_table = lua.create_table()?;

        if let Some(read_base_dir) = read_base_dir {
            let read_root = crate::tools::project_fs::open_root(Some(&read_base_dir))
                .map_err(mlua::Error::external)?;
            let fs_read = lua.create_function(move |_, path: String| {
                crate::tools::project_fs::validate_agent_read_path(Path::new(&path))
                    .map_err(mlua::Error::external)?;
                crate::tools::project_fs::read_utf8_bounded(
                    &read_root,
                    Path::new(&path),
                    read_limit,
                )
                .map_err(mlua::Error::external)
            })?;
            fs_table.set("read", fs_read)?;
        }

        if let Some(write_base_dir) = write_base_dir {
            let write_root = crate::tools::project_fs::open_root(Some(&write_base_dir))
                .map_err(mlua::Error::external)?;
            let fs_write =
                lua.create_function(move |lua, (path, content): (String, mlua::LuaString)| {
                    crate::lua::bootstrap::reject_effect(lua, "fs.write")?;
                    let path = Path::new(&path);
                    crate::tools::project_fs::validate_agent_write_path(path)
                        .map_err(mlua::Error::external)?;
                    let bytes = content.as_bytes();
                    if bytes.len() > write_limit {
                        return Err(mlua::Error::external(format!(
                            "Lua fs.write content is {} bytes; limit is {write_limit} bytes",
                            bytes.len()
                        )));
                    }
                    crate::tools::project_fs::atomic_write(&write_root, path, &bytes)
                        .map_err(mlua::Error::external)
                })?;
            fs_table.set("write", fs_write)?;
        }
        lua.globals().set("fs", fs_table)?;
    }

    // `http` is registered by `register_lua_globals_with_policy` above and is
    // available to tools under the captured outbound-network policy (SSRF
    // validation plus the response body-size limit). The `llm` namespace still
    // needs a provider reference and is bound per-execution in
    // `LuaScriptTool::execute`.

    Ok(lua)
}

#[cfg(test)]
mod resource_limit_tests {
    use super::*;

    fn lua() -> Lua {
        let lua = Lua::new();
        register_lua_globals(&lua).unwrap();
        lua
    }

    fn regex_function(lua: &Lua, name: &str) -> mlua::Function {
        lua.globals()
            .get::<mlua::Table>("regex")
            .unwrap()
            .get(name)
            .unwrap()
    }

    #[test]
    fn bounded_replace_preserves_capture_and_dollar_semantics() {
        let lua = lua();
        let replace_all = regex_function(&lua, "replace_all");
        let replaced: String = replace_all
            .call((r"(?<word>\w+)", "ab cd", "<$word>"))
            .unwrap();
        assert_eq!(replaced, "<ab> <cd>");

        let replace = regex_function(&lua, "replace");
        let replaced: String = replace.call((r"(a)(b)", "ab", "$$:${2}${1}")).unwrap();
        assert_eq!(replaced, "$:ba");
    }

    #[test]
    fn regex_collection_operations_reject_excessive_item_counts() {
        let lua = lua();
        let text = "x".repeat(MAX_REGEX_RESULT_ITEMS + 1);

        let find_all = regex_function(&lua, "find_all");
        let error = find_all
            .call::<mlua::Table>(("", text.as_str()))
            .expect_err("empty matches must not create an unbounded Lua table");
        assert!(error.to_string().contains("10000-item"));

        let split = regex_function(&lua, "split");
        let error = split
            .call::<mlua::Table>(("", text.as_str()))
            .expect_err("empty splits must not create an unbounded Lua table");
        assert!(error.to_string().contains("10000-item"));
    }

    #[test]
    fn regex_replacement_expansion_is_measured_before_allocation() {
        let lua = lua();
        let replace = regex_function(&lua, "replace");
        let text = "x".repeat(1024);
        let replacement = "$1".repeat(MAX_REGEX_RESULT_BYTES / 1024 + 1);
        let error = replace
            .call::<String>((r"(x+)", text, replacement))
            .expect_err("capture expansion larger than the result budget must fail");
        assert!(error.to_string().contains("8388608-byte"));
    }

    #[test]
    fn regex_captures_charge_duplicate_named_entries() {
        let lua = lua();
        let captures = regex_function(&lua, "captures");
        let text = "x".repeat(2 * 1024 * 1024);
        let error = captures
            .call::<mlua::Table>((r"(?P<a>(?P<b>.*))", text))
            .expect_err("indexed and named captures must share one byte budget");
        assert!(error.to_string().contains("regex.captures result"));
    }

    #[test]
    fn regex_cache_and_compiled_patterns_have_hard_limits() {
        REGEX_CACHE.with(|cache| cache.borrow_mut().clear());
        for index in 0..=REGEX_CACHE_MAX {
            get_or_compile_regex(&format!("value{index}")).unwrap();
        }
        REGEX_CACHE.with(|cache| {
            let cache = cache.borrow();
            assert!(cache.len() <= REGEX_CACHE_MAX);
            assert!(cache.contains_key(&format!("value{REGEX_CACHE_MAX}")));
        });

        let error = get_or_compile_regex(&"x".repeat(MAX_REGEX_PATTERN_BYTES + 1)).unwrap_err();
        assert!(error.to_string().contains("4096-byte"));

        let too_many_captures = "()".repeat(MAX_REGEX_CAPTURE_GROUPS + 1);
        let error = get_or_compile_regex(&too_many_captures).unwrap_err();
        assert!(error.to_string().contains("capture groups"));
    }

    #[test]
    fn template_rendering_stops_at_writer_budget() {
        let lua = lua();
        let template: mlua::Function = lua.globals().get("template").unwrap();
        let data = lua.create_table().unwrap();
        data.set("value", "x".repeat(1024 * 1024)).unwrap();
        let source = "{{ value }}".repeat(9);
        let error = template
            .call::<String>((source, data))
            .expect_err("render_to writer must stop an oversized expansion");
        assert!(
            error.to_string().contains("template output"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn base64_json_and_log_inputs_are_bounded() {
        let lua = lua();

        let base64_encode: mlua::Function = lua.globals().get("base64_encode").unwrap();
        let error = base64_encode
            .call::<String>("x".repeat(MAX_LUA_BASE64_INPUT_BYTES + 1))
            .expect_err("base64 expansion must have a bounded input");
        assert!(error.to_string().contains("base64_encode input"));

        let json_parse: mlua::Function = lua.globals().get("json_parse").unwrap();
        let error = json_parse
            .call::<Value>(format!("\"{}\"", "x".repeat(MAX_LUA_JSON_INPUT_BYTES)))
            .expect_err("JSON parsing must reject oversized source before serde allocation");
        assert!(error.to_string().contains("json_parse input"));

        let error = lua
            .load(
                r#"
                local args = {}
                for i = 1, 65 do args[i] = "x" end
                log(table.unpack(args))
                "#,
            )
            .exec()
            .expect_err("log argument vectors must be bounded");
        assert!(error.to_string().contains("at most 64 arguments"));
    }

    #[test]
    fn validation_error_tables_are_bounded_and_mark_omissions() {
        let lua = lua();
        let validate_json: mlua::Function = lua.globals().get("validate_json").unwrap();
        let schema = lua.create_table().unwrap();
        schema.set("type", "array").unwrap();
        let items = lua.create_table().unwrap();
        items.set("type", "integer").unwrap();
        schema.set("items", items).unwrap();
        let data = serde_json::to_string(&vec!["bad"; MAX_JSON_VALIDATION_ERRORS + 1]).unwrap();

        let result: mlua::Table = validate_json.call((data, schema)).unwrap();
        assert!(!result.get::<bool>("valid").unwrap());
        let errors: mlua::Table = result.get("errors").unwrap();
        assert_eq!(errors.raw_len(), MAX_JSON_VALIDATION_ERRORS + 1);
        let marker: mlua::Table = errors.get(MAX_JSON_VALIDATION_ERRORS + 1).unwrap();
        assert!(marker.get::<String>("message").unwrap().contains("omitted"));
    }
}

#[cfg(test)]
mod schema_tests {
    use mlua::Lua;

    fn lua() -> Lua {
        let lua = Lua::new();
        super::register_lua_globals(&lua).unwrap();
        lua
    }

    #[test]
    fn lua_validator_supports_local_refs() {
        let lua = lua();
        let result: mlua::Table = lua
            .load(
                r##"
                return validate_json('{"id": 7}', {
                    definitions = { identifier = { type = "integer" } },
                    type = "object",
                    properties = { id = { ["$ref"] = "#/definitions/identifier" } }
                })
                "##,
            )
            .eval()
            .unwrap();
        assert!(result.get::<bool>("valid").unwrap());
    }

    #[test]
    fn lua_validator_rejects_remote_refs() {
        let error = lua()
            .load(
                r#"return validate_json('{}', {
                    ["$ref"] = "https://example.invalid/schema.json"
                })"#,
            )
            .eval::<mlua::Table>()
            .expect_err("remote ref must be rejected");
        assert!(error.to_string().contains("External JSON Schema $ref"));
    }
}

#[cfg(test)]
mod fs_tests {
    use super::*;

    #[test]
    fn lua_fs_round_trip_is_capability_relative() {
        let directory = tempfile::tempdir().unwrap();
        let lua = create_tool_lua_with_base_dir(Some(directory.path().to_path_buf())).unwrap();
        let value: String = lua
            .load("fs.write('nested/value.txt', 'hello'); return fs.read('nested/value.txt')")
            .eval()
            .unwrap();
        assert_eq!(value, "hello");
    }

    #[test]
    fn lua_fs_write_rejects_oversized_content_before_copying() {
        let directory = tempfile::tempdir().unwrap();
        let lua = create_tool_lua_with_base_dir(Some(directory.path().to_path_buf())).unwrap();
        let error = lua
            .load("fs.write('large.txt', string.rep('x', 1024 * 1024 + 1))")
            .exec()
            .expect_err("default 1 MiB write cap must be enforced");
        assert!(error.to_string().contains("limit is 1048576 bytes"));
        assert!(!directory.path().join("large.txt").exists());
    }

    #[test]
    fn lua_fs_reads_project_but_writes_only_output_root() {
        let project = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("input.txt"), "source").unwrap();
        let lua = create_tool_lua_with_fs_roots(
            Some(project.path().to_path_buf()),
            Some(output.path().to_path_buf()),
        )
        .unwrap();

        let value: String = lua
            .load("fs.write('result.txt', fs.read('input.txt')); return fs.read('input.txt')")
            .eval()
            .unwrap();
        assert_eq!(value, "source");
        assert!(!project.path().join("result.txt").exists());
        assert_eq!(
            std::fs::read_to_string(output.path().join("result.txt")).unwrap(),
            "source"
        );
    }

    #[test]
    fn lua_fs_never_writes_flow_source_or_control_files() {
        let directory = tempfile::tempdir().unwrap();
        let lua = create_tool_lua_with_base_dir(Some(directory.path().to_path_buf())).unwrap();
        for path in ["crew.lua", "hook.sh", ".env", "Dockerfile"] {
            let script = format!("fs.write({path:?}, 'malicious')");
            assert!(
                lua.load(&script).exec().is_err(),
                "unexpectedly wrote {path}"
            );
            assert!(!directory.path().join(path).exists());
        }
    }

    #[test]
    fn lua_fs_never_reads_flow_credentials_or_state() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".env"), "OPENAI_API_KEY=secret").unwrap();
        std::fs::create_dir_all(directory.path().join(".ironcrew")).unwrap();
        std::fs::write(directory.path().join(".ironcrew/run.json"), "secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            symlink(".env", directory.path().join("env-alias.txt")).unwrap();
            symlink(".ironcrew", directory.path().join("state-alias")).unwrap();
        }
        let lua = create_tool_lua_with_base_dir(Some(directory.path().to_path_buf())).unwrap();

        for path in [".env", ".ironcrew/run.json"] {
            let script = format!("return fs.read({path:?})");
            assert!(lua.load(&script).eval::<String>().is_err());
        }
        #[cfg(unix)]
        for path in ["env-alias.txt", "state-alias/run.json"] {
            let script = format!("return fs.read({path:?})");
            assert!(
                lua.load(&script).eval::<String>().is_err(),
                "unexpectedly read sensitive symlink alias {path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn lua_fs_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), directory.path().join("escape")).unwrap();

        let lua = create_tool_lua_with_base_dir(Some(directory.path().to_path_buf())).unwrap();
        let error = lua
            .load("return fs.read('escape/secret.txt')")
            .eval::<String>()
            .expect_err("capability path resolution must not follow an escaping symlink");
        assert!(
            error.to_string().contains("outside")
                || error.to_string().contains("symlink")
                || error.to_string().contains("symbolic link")
                || error.to_string().contains("permission")
                || error.to_string().contains("Not a directory"),
            "unexpected error: {error}"
        );
    }
}

#[cfg(test)]
mod env_tests {
    use mlua::Lua;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Shared lock for tests that mutate process env. Matches the pattern
    /// used in `src/mcp/config.rs` and `src/lua/agent_turn.rs`.
    fn env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn new_lua() -> Lua {
        let lua = Lua::new();
        super::register_lua_globals(&lua).unwrap();
        lua
    }

    /// SAFETY: env mutations are serialized via `env_guard()`. Tests in
    /// this module are the only place these vars are written.
    fn set(name: &str, value: &str) {
        unsafe { std::env::set_var(name, value) };
    }
    fn unset(name: &str) {
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn allowlist_unblocks_default_api_key_suffix() {
        let _guard = env_guard();
        set("AZURE_OPENAI_API_KEY", "sk-azure-test");
        set("IRONCREW_ENV_ALLOWLIST", "AZURE_OPENAI_API_KEY");

        let lua = new_lua();
        let got: Option<String> = lua
            .load(r#"return env("AZURE_OPENAI_API_KEY")"#)
            .eval()
            .unwrap();
        assert_eq!(got.as_deref(), Some("sk-azure-test"));

        unset("AZURE_OPENAI_API_KEY");
        unset("IRONCREW_ENV_ALLOWLIST");
    }

    #[test]
    fn allowlist_unblocks_default_blocked_exact_name() {
        let _guard = env_guard();
        set("DATABASE_URL", "postgres://localhost/test");
        set("IRONCREW_ENV_ALLOWLIST", "DATABASE_URL");

        let lua = new_lua();
        let got: Option<String> = lua.load(r#"return env("DATABASE_URL")"#).eval().unwrap();
        assert_eq!(got.as_deref(), Some("postgres://localhost/test"));

        unset("DATABASE_URL");
        unset("IRONCREW_ENV_ALLOWLIST");
    }

    #[test]
    fn allowlist_overrides_custom_blocklist() {
        let _guard = env_guard();
        set("MY_PUBLIC_VAR", "visible");
        set("IRONCREW_ENV_BLOCKLIST", "MY_PUBLIC_VAR");
        set("IRONCREW_ENV_ALLOWLIST", "MY_PUBLIC_VAR");

        let lua = new_lua();
        let got: Option<String> = lua.load(r#"return env("MY_PUBLIC_VAR")"#).eval().unwrap();
        assert_eq!(got.as_deref(), Some("visible"));

        unset("MY_PUBLIC_VAR");
        unset("IRONCREW_ENV_BLOCKLIST");
        unset("IRONCREW_ENV_ALLOWLIST");
    }

    #[test]
    fn no_allowlist_still_blocks_api_key_suffix() {
        let _guard = env_guard();
        set("SOME_OTHER_API_KEY", "sk-secret");
        unset("IRONCREW_ENV_ALLOWLIST");

        let lua = new_lua();
        let got: Option<String> = lua
            .load(r#"return env("SOME_OTHER_API_KEY")"#)
            .eval()
            .unwrap();
        assert_eq!(
            got, None,
            "*_API_KEY must still be blocked without an allowlist entry"
        );

        unset("SOME_OTHER_API_KEY");
    }

    #[test]
    fn empty_allowlist_entry_does_not_unblock_anything() {
        let _guard = env_guard();
        set("DATABASE_URL", "postgres://localhost/test");
        set("IRONCREW_ENV_ALLOWLIST", ",,,"); // empty entries

        let lua = new_lua();
        let got: Option<String> = lua.load(r#"return env("DATABASE_URL")"#).eval().unwrap();
        assert_eq!(got, None, "empty allowlist tokens must not match anything");

        unset("DATABASE_URL");
        unset("IRONCREW_ENV_ALLOWLIST");
    }
}
