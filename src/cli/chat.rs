//! `ironcrew chat <path>` — interactive REPL against a conversational agent.
//!
//! Loads a flow project, marks the Lua VM with `ChatMode`, executes the
//! entrypoint (which must construct a `Crew` and call `crew:add_agent(...)`
//! — but should NOT call `crew:run()` in chat mode), retrieves the crew
//! via the `__ironcrew_chat_crew` registry slot, binds a conversation to
//! the requested agent, and enters a stdin-driven read-eval loop.
//!
//! Slash commands: /help, /exit, /quit, /reset, /id, /save, /history.

use std::path::Path;
use std::sync::Arc;

use mlua::AnyUserData;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::lua::api::{CHAT_CREW_REGISTRY_KEY, ChatMode, set_ironcrew_mode};
use crate::lua::conversation::{LuaConversation, LuaConversationInner};
use crate::utils::error::{IronCrewError, Result};

use super::project::{load_project, setup_crew_runtime};

/// Entrypoint for the `chat` subcommand.
pub async fn cmd_chat(path: &Path, agent: Option<String>, id: Option<String>) -> Result<()> {
    let loader = load_project(path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Flag the VM as chat-mode so the Crew constructor stashes itself in the
    // registry and top-level `if IRONCREW_MODE ~= "chat" then crew:run() end`
    // guards behave correctly.
    lua.set_app_data(ChatMode);
    set_ironcrew_mode(&lua, "chat").map_err(IronCrewError::Lua)?;

    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;
    let script = std::fs::read_to_string(entrypoint)?;

    // Execute the entrypoint. The user's crew.lua is expected to build a
    // Crew and add at least one agent. With ChatMode set, the Crew
    // constructor will park the userdata in the registry.
    lua.load(&script)
        .exec_async()
        .await
        .map_err(IronCrewError::Lua)?;

    let agent_name = agent.ok_or_else(|| {
        IronCrewError::Validation(
            "chat: missing --agent <name>; pass the agent declared in your crew.lua".into(),
        )
    })?;

    // Build the conversation via the Lua-exposed `crew:conversation({...})`
    // method. We drive it through a small Lua snippet so it goes through
    // the exact same code path as `conv = crew:conversation({...})` inside
    // a user script — no duplicate plumbing.
    let snippet = format!(
        r#"
            local crew = ...
            return crew:conversation({{
                agent = {agent_expr},
                {id_field}
                stream = false,
            }})
        "#,
        agent_expr = lua_string_literal(&agent_name),
        id_field = match id {
            Some(ref s) => format!("id = {},", lua_string_literal(s)),
            None => String::new(),
        }
    );

    let crew_ud: AnyUserData = lua
        .named_registry_value(CHAT_CREW_REGISTRY_KEY)
        .map_err(|_| {
            IronCrewError::Validation(
                "No Crew.new(...) call detected in entrypoint — chat mode requires a crew".into(),
            )
        })?;

    let conv_ud: AnyUserData = lua
        .load(&snippet)
        .call_async::<AnyUserData>(crew_ud)
        .await
        .map_err(IronCrewError::Lua)?;

    // Grab the Arc out of the LuaConversation wrapper.
    let conv: Arc<LuaConversationInner> = {
        let wrapper = conv_ud
            .borrow::<LuaConversation>()
            .map_err(IronCrewError::Lua)?;
        wrapper.inner()
    };

    // Print the banner and enter the REPL.
    print_banner(&conv, path);
    repl(conv).await
}

/// Escape an arbitrary string for embedding as a Lua string literal using
/// the long-bracket form, which needs no character escaping.
pub(crate) fn __chat_lua_literal(s: &str) -> String {
    // Pick a level of brackets that does not appear in the string.
    let mut level = 0usize;
    loop {
        let closer = format!("]{}]", "=".repeat(level));
        if !s.contains(&closer) {
            let eq = "=".repeat(level);
            return format!("[{eq}[{s}]{eq}]");
        }
        level += 1;
        if level > 32 {
            // Ridiculous; fall back to quoted string with minimal escaping.
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            return format!("\"{}\"", escaped);
        }
    }
}

/// Compat alias used by `chat.rs` call sites before extraction.
fn lua_string_literal(s: &str) -> String {
    __chat_lua_literal(s)
}

fn print_banner(conv: &LuaConversationInner, project_path: &Path) {
    eprintln!();
    eprintln!("IronCrew chat — v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("  flow:        {}", project_path.display());
    eprintln!("  agent:       {}", conv.agent.name);
    eprintln!("  session id:  {}", conv.id);
    eprintln!(
        "  persistent:  {}",
        if conv.persistent { "yes" } else { "no" }
    );
    eprintln!();
    eprintln!("Slash commands: /help /exit /quit /reset /id /save /history");
    eprintln!("Type a message and press Enter. Ctrl-C or /exit to quit.");
    eprintln!();
}

async fn repl(conv: Arc<LuaConversationInner>) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = stdout;

    loop {
        stdout.write_all(b"> ").await.ok();
        stdout.flush().await.ok();

        tokio::select! {
            line = reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Some(cmd) = trimmed.strip_prefix('/') {
                            if handle_slash(cmd, &conv).await? {
                                break;
                            }
                            continue;
                        }
                        match conv.run_turn(trimmed, None).await {
                            Ok((reply, _reasoning)) => {
                                println!("{}", reply);
                            }
                            Err(e) => {
                                eprintln!("[error] {}", e);
                            }
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        eprintln!("[stdin error] {}", e);
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!();
                break;
            }
        }
    }

    // Best-effort final persist on exit.
    if let Err(e) = conv.persist().await {
        tracing::warn!("Final persist failed: {}", e);
    }
    println!();
    Ok(())
}

/// Returns `Ok(true)` to indicate the REPL should exit.
async fn handle_slash(cmd: &str, conv: &Arc<LuaConversationInner>) -> Result<bool> {
    let cmd = cmd.trim();
    match cmd {
        "help" | "?" => {
            eprintln!("Available commands:");
            eprintln!("  /help         Show this help");
            eprintln!("  /exit, /quit  Exit the chat session");
            eprintln!("  /reset        Clear history (keep system prompt)");
            eprintln!("  /id           Show the session id");
            eprintln!("  /save         Persist the session now");
            eprintln!("  /history      Print the conversation transcript");
            Ok(false)
        }
        "exit" | "quit" => Ok(true),
        "reset" => {
            conv.reset_history().await;
            if let Err(e) = conv.persist().await {
                eprintln!("[warning] persist after reset failed: {}", e);
            }
            eprintln!("[reset] history cleared");
            Ok(false)
        }
        "id" => {
            println!("{}", conv.id);
            Ok(false)
        }
        "save" => {
            match conv.persist().await {
                Ok(_) => eprintln!("[saved]"),
                Err(e) => eprintln!("[save failed] {}", e),
            }
            Ok(false)
        }
        "history" => {
            let history = conv.messages_snapshot().await;
            for (i, msg) in history.iter().enumerate() {
                let content = msg.content.as_deref().unwrap_or("");
                println!("[{:>3}] {:>9}: {}", i, msg.role, content);
            }
            Ok(false)
        }
        other => {
            eprintln!("[unknown command] /{}; try /help", other);
            Ok(false)
        }
    }
}
