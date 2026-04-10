use std::path::Path;

use crate::cli::graph_types::GraphData;
use crate::utils::error::Result;

// ── Embedded assets ──────────────────────────────────────────────────────────

const CSS: &str = include_str!("../../examples/graph-prototype/assets/css/main.css");

const JS_THEME: &str = include_str!("../../examples/graph-prototype/assets/js/theme.js");
const JS_ICON_REGISTRY: &str =
    include_str!("../../examples/graph-prototype/assets/js/icon-registry.js");
const JS_GRAPH_SHAPES: &str =
    include_str!("../../examples/graph-prototype/assets/js/graph-shapes.js");
const JS_GRAPH_BUILDER: &str =
    include_str!("../../examples/graph-prototype/assets/js/graph-builder.js");
const JS_GRAPH_RUNTIME: &str =
    include_str!("../../examples/graph-prototype/assets/js/graph-runtime.js");
const JS_APP: &str = include_str!("../../examples/graph-prototype/assets/js/app.js");

const SVG_SPRITE: &str = include_str!("../../examples/graph-prototype/assets/images/icons.svg");

// ── Individual SVG icons ──────────────────────────────────────────────────────

const ICON_AGENT: &str = include_str!("../../examples/graph-prototype/assets/images/agent.svg");
const ICON_CHAT: &str = include_str!("../../examples/graph-prototype/assets/images/chat.svg");
const ICON_CODE: &str = include_str!("../../examples/graph-prototype/assets/images/code.svg");
const ICON_COLLAB: &str = include_str!("../../examples/graph-prototype/assets/images/collab.svg");
const ICON_CONDITION: &str =
    include_str!("../../examples/graph-prototype/assets/images/condition.svg");
const ICON_CREW: &str = include_str!("../../examples/graph-prototype/assets/images/crew.svg");
const ICON_DIALOG: &str = include_str!("../../examples/graph-prototype/assets/images/dialog.svg");
const ICON_INFO: &str = include_str!("../../examples/graph-prototype/assets/images/info.svg");
const ICON_LOOP: &str = include_str!("../../examples/graph-prototype/assets/images/loop.svg");
const ICON_MEMORY: &str = include_str!("../../examples/graph-prototype/assets/images/memory.svg");
const ICON_MESSAGE: &str = include_str!("../../examples/graph-prototype/assets/images/message.svg");
const ICON_PARALLEL: &str =
    include_str!("../../examples/graph-prototype/assets/images/parallel.svg");
const ICON_RESULT: &str = include_str!("../../examples/graph-prototype/assets/images/result.svg");
const ICON_RETRY: &str = include_str!("../../examples/graph-prototype/assets/images/retry.svg");
const ICON_SUBFLOW: &str = include_str!("../../examples/graph-prototype/assets/images/subflow.svg");
const ICON_TASK: &str = include_str!("../../examples/graph-prototype/assets/images/task.svg");
const ICON_TOOL_CALL: &str =
    include_str!("../../examples/graph-prototype/assets/images/tool-call.svg");

// ── JS bundling ───────────────────────────────────────────────────────────────

/// Strip ES module `import`/`export` syntax and concatenate all JS sources in
/// the correct dependency order.
fn build_js_bundle() -> String {
    let sources = [
        JS_THEME,
        JS_ICON_REGISTRY,
        JS_GRAPH_SHAPES,
        JS_GRAPH_BUILDER,
        JS_GRAPH_RUNTIME,
        JS_APP,
    ];

    let mut out = String::new();
    for source in &sources {
        for line in source.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("import ") && trimmed.contains(" from ") {
                // drop ES module import entirely
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("export ") {
                // keep the declaration, drop the `export ` keyword
                out.push_str(rest);
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

// ── SVG → data URI ────────────────────────────────────────────────────────────

fn svg_to_data_uri(svg: &str) -> String {
    // Percent-encode ALL characters that could break a JS string literal.
    // Both quote types must be encoded since the URI ends up inside either
    // single- or double-quoted JS strings.
    let encoded = svg
        .replace('\n', " ")
        .replace('\r', "")
        .replace('"', "%22")
        .replace('\'', "%27")
        .replace('#', "%23")
        .replace('<', "%3C")
        .replace('>', "%3E");
    format!("data:image/svg+xml;charset=utf-8,{}", encoded)
}

/// Build a JS `const __ICON_DATA_URIS = { ... };` block with every icon as a
/// data URI so the runtime can reference them by filename.
fn build_icon_data_uris() -> String {
    let icons: &[(&str, &str)] = &[
        ("agent.svg", ICON_AGENT),
        ("chat.svg", ICON_CHAT),
        ("code.svg", ICON_CODE),
        ("collab.svg", ICON_COLLAB),
        ("condition.svg", ICON_CONDITION),
        ("crew.svg", ICON_CREW),
        ("dialog.svg", ICON_DIALOG),
        ("info.svg", ICON_INFO),
        ("loop.svg", ICON_LOOP),
        ("memory.svg", ICON_MEMORY),
        ("message.svg", ICON_MESSAGE),
        ("parallel.svg", ICON_PARALLEL),
        ("result.svg", ICON_RESULT),
        ("retry.svg", ICON_RETRY),
        ("subflow.svg", ICON_SUBFLOW),
        ("task.svg", ICON_TASK),
        ("tool-call.svg", ICON_TOOL_CALL),
    ];

    let mut entries = String::new();
    for (name, svg) in icons {
        let uri = svg_to_data_uri(svg);
        entries.push_str(&format!("  '{}': '{}',\n", name, uri));
    }

    format!("const __ICON_DATA_URIS = {{\n{}}};\n", entries)
}

// ── HTML generation ───────────────────────────────────────────────────────────

/// Generate a fully self-contained HTML file for the crew DAG visualisation.
pub fn generate_html(data: &GraphData, output_path: &Path) -> Result<()> {
    let crew_name = &data.name;
    let css = CSS;
    let svg_sprite = SVG_SPRITE;
    let icon_overrides = build_icon_data_uris();
    let js_bundle = build_js_bundle();
    let data_json = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());

    let html = format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <title>IronCrew DAG — {crew_name}</title>
  <link href="https://fonts.googleapis.com/css2?family=IBM+Plex+Sans:wght@400;500;600;700&display=swap" rel="stylesheet" />
  <script src="https://cdn.jsdelivr.net/npm/@antv/x6/dist/index.js"></script>
  <style>{css}</style>
</head>
<body>
{svg_sprite}

<header>
  <div class="brand"><h1>IronCrew</h1><span class="tag">{crew_name}</span></div>
  <div class="toolbar">
    <button id="btn-reset" class="secondary">Reset</button>
    <button id="btn-play">Simulate run</button>
  </div>
</header>

<div id="main">
  <div id="graph-container">
    <div id="legend-panel" class="legend-shell is-collapsed">
      <button id="legend-toggle" class="legend-toggle" type="button" aria-expanded="false">&#9432;</button>
      <div id="legend"></div>
    </div>
  </div>
  <aside id="inspector">
    <h2>Inspector</h2>
    <div class="empty">Click a node to see its details</div>
  </aside>
</div>

<script>
{icon_overrides}
const CREW = {data_json};
{js_bundle}
</script>
</body>
</html>
"##,
        crew_name = crew_name,
        css = css,
        svg_sprite = svg_sprite,
        icon_overrides = icon_overrides,
        data_json = data_json,
        js_bundle = js_bundle,
    );

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output_path, html)?;

    Ok(())
}
