/*
 * Shape and node/edge factories for the DAG prototype.
 */

import { THEME } from './theme.js';
import { resolveNodeIcon } from './icon-registry.js';
const EDGE_RADIUS = 0;
const X6 = window.X6;

function rootFontSizePx() {
  if (typeof window === 'undefined' || !window.getComputedStyle || !document?.documentElement) {
    return 16;
  }

  return parseFloat(window.getComputedStyle(document.documentElement).fontSize) || 16;
}

function remToPx(value) {
  return Math.round(value * rootFontSizePx());
}

export const NODE_TEXT_LAYOUT = {
  textPadX: 4.75,
  titleLineHeight: 1,
  subtitleLineHeight: 0.8125,
  titleSubtitleGap: 1.125,
  toolTopPad: 0.875,
  taskReserve: 1.625,
  defaultBottomPadding: 0.625,
  minTitleCharsAgent: 22,
  minTitleCharsOther: 24,
  minSubtitleChars: 28,
  subtitleFontSize: 0.6875,
};

export const CARD_MIN_HEIGHTS = {
  crew: 4.625,
  functions: 4.5,
  function: 4.5,
  'tools-group': 4.5,
  result: 4.625,
  agent: 3.875,
  task: 5.375,
  tool: 3.375,
  conversation: 3.875,
  dialog: 4,
  memory: 3.625,
  message: 3.875,
};

export const SIZE_BY_KIND = {
  crew: 18.75,
  functions: 18.75,
  function: 15.625,
  'tools-group': 18.75,
  result: 16.25,
  agent: 14.375,
  task: 15.625,
  tool: 14.6875,
  conversation: 15.625,
  dialog: 16.25,
  memory: 15,
  message: 16.25,
};

export const kindShapeByNode = {
  crew: 'ironcrew-crew',
  functions: 'ironcrew-runtime',
  function: 'ironcrew-runtime',
  'tools-group': 'ironcrew-runtime',
  result: 'ironcrew-task',
  agent: 'ironcrew-agent',
  task: 'ironcrew-task',
  tool: 'ironcrew-runtime',
  conversation: 'ironcrew-runtime',
  dialog: 'ironcrew-runtime',
  memory: 'ironcrew-runtime',
  message: 'ironcrew-runtime',
  runtime: 'ironcrew-runtime',
};

export const EDGE_LAYOUT = { orthogonalGapX: 60 };

export function iconSlotForKind(kind) {
  // Icon sized to fit inside the header (2.75rem tall), vertically
  // centered alongside the title text.
  const iconSize = remToPx(1.25);
  const headerH = remToPx(2.75);
  return {
    x: remToPx(0.75),
    y: (headerH - iconSize) / 2,
    width: iconSize,
    height: iconSize,
  };
}

export function iconRefForKind(kind, taskType = null) {
  return resolveNodeIcon(kind, taskType);
}

export function iconAttrsForKind(kind, taskType = null) {
  const slot = iconSlotForKind(kind);
  const icon = iconRefForKind(kind, taskType);
  const iconUse = {
    ...slot,
    color: THEME.node.iconStroke,
    display: 'none',
  };
  const iconImage = {
    ...slot,
    preserveAspectRatio: 'xMidYMid meet',
    display: 'none',
  };

  if (icon.type === 'image') {
    iconImage.href = icon.href;
    iconImage['xlink:href'] = icon.href;
    iconImage.display = 'inline';
  } else {
    iconUse.href = icon.href;
    iconUse['xlink:href'] = icon.href;
    iconUse.display = 'inline';
  }

  return { iconUse, iconImage };
}

function baseCardMarkup() {
  return {
    refWidth: '100%',
    refHeight: '100%',
    rx: 10,
    ry: 10,
    fill: THEME.node.bodyFill,
    stroke: THEME.node.bodyStroke,
    strokeWidth: 1,
    filter: 'drop-shadow(0 2px 8px rgba(0,0,0,0.28))',
  };
}

export function makePorts(kind, nodeId) {
  const nodePrefix = `${nodeId || 'node'}:`;
  const portStyle = {
    r: 0,
    magnet: false,
    fill: THEME.node.bodyFill,
    stroke: THEME.node.iconStroke,
    strokeWidth: 2,
  };

  if (kind === 'crew') {
    return {
      groups: {
        right: {
          position: 'right',
          attrs: { circle: portStyle },
        },
      },
      items: [{ id: `${nodePrefix}out`, group: 'right' }],
    };
  }

  if (kind === 'agent' || kind === 'result') {
    return {
      groups: {
        left: {
          position: 'left',
          attrs: { circle: portStyle },
        },
        right: {
          position: 'right',
          attrs: { circle: portStyle },
        },
      },
      items: [
        { id: `${nodePrefix}in`, group: 'left' },
        { id: `${nodePrefix}out`, group: 'right' },
      ],
    };
  }

  if (kind === 'task') {
    return {
      groups: {
        left: {
          position: 'left',
          attrs: { circle: portStyle },
        },
        right: {
          position: 'right',
          attrs: { circle: portStyle },
        },
        top: {
          position: 'top',
          attrs: { circle: portStyle },
        },
        bottom: {
          position: 'bottom',
          attrs: { circle: portStyle },
        },
      },
      items: [
        { id: `${nodePrefix}in`, group: 'left' },
        { id: `${nodePrefix}out`, group: 'right' },
        { id: `${nodePrefix}top`, group: 'top' },
        { id: `${nodePrefix}bottom`, group: 'bottom' },
      ],
    };
  }

  if (kind === 'tool') {
    return {
      groups: {
        right: {
          position: 'right',
          attrs: { circle: portStyle },
        },
      },
      items: [{ id: `${nodePrefix}out`, group: 'right' }],
    };
  }

  return {
    groups: {
      left: {
        position: 'left',
        attrs: { circle: portStyle },
      },
      right: {
        position: 'right',
        attrs: { circle: portStyle },
      },
      top: {
        position: 'top',
        attrs: { circle: portStyle },
      },
      bottom: {
        position: 'bottom',
        attrs: { circle: portStyle },
      },
    },
    items: [
      { id: `${nodePrefix}in`, group: 'left' },
      { id: `${nodePrefix}out`, group: 'right' },
      { id: `${nodePrefix}top`, group: 'top' },
      { id: `${nodePrefix}bottom`, group: 'bottom' },
    ],
  };
}

export function portId(nodeId, logicalPort) {
  return `${nodeId || 'node'}:${logicalPort}`;
}

function commonCardShape(name, width, height, opts = {}) {
  const { showStatus = false } = opts;
  const iconSlot = iconSlotForKind(name);

  // ── Header zone ─────────────────────────────────────────────────
  // Icon + title live in the header. The header is a tinted rect that
  // visually separates identity from metadata below.
  const headerH = remToPx(2.75);
  const headerPadX = remToPx(0.625);
  const iconX = headerPadX;
  const iconY = (headerH - iconSlot.height) / 2;
  const titleX = iconX + iconSlot.width + remToPx(0.625);
  const titleY = headerH / 2;

  // ── Body zone ───────────────────────────────────────────────────
  // Subtitle + status info live below the header divider.
  const bodyTopY = headerH + remToPx(0.5);
  const subtitleX = headerPadX;

  const markup = [
    { tagName: 'rect', selector: 'body' },
    { tagName: 'rect', selector: 'header' },
    { tagName: 'line', selector: 'divider' },
    { tagName: 'use', selector: 'iconUse' },
    { tagName: 'image', selector: 'iconImage' },
    { tagName: 'text', selector: 'title' },
    { tagName: 'text', selector: 'subtitle' },
  ];

  const attrs = {
    body: baseCardMarkup(),
    header: {
      refWidth: '100%',
      height: headerH,
      refY: 0,
      // Match the body's corner radius at the top; flat at the bottom
      // so the divider line sits cleanly.
      rx: 10,
      ry: 10,
      fill: '#1a2538',
      stroke: 'none',
    },
    divider: {
      x1: 0,
      y1: headerH,
      refX2: '100%',
      y2: headerH,
      stroke: THEME.node.bodyStroke,
      strokeWidth: 1,
    },
    iconUse: {
      x: iconX,
      y: iconY,
      width: iconSlot.width,
      height: iconSlot.height,
      color: THEME.node.iconStroke,
      display: 'none',
    },
    iconImage: {
      x: iconX,
      y: iconY,
      width: iconSlot.width,
      height: iconSlot.height,
      preserveAspectRatio: 'xMidYMid meet',
      display: 'none',
    },
    title: {
      refX: titleX,
      refY: titleY,
      fontSize: remToPx(name === 'agent' ? 0.8125 : 0.875),
      fontFamily: THEME.fontSans,
      fontWeight: 600,
      fill: THEME.node.titleFill,
      textAnchor: 'start',
      textVerticalAnchor: 'middle',
    },
    subtitle: {
      refX: subtitleX,
      refY: bodyTopY,
      fontSize: remToPx(NODE_TEXT_LAYOUT.subtitleFontSize),
      fontFamily: THEME.fontSans,
      fill: THEME.node.subtitleFill,
      textAnchor: 'start',
      textVerticalAnchor: 'top',
    },
  };

  const markupWithStatus = showStatus
    ? [...markup, { tagName: 'circle', selector: 'statusDot' }, { tagName: 'text', selector: 'statusLabel' }]
    : markup;

  if (showStatus) {
    attrs.statusDot = {
      refX: '100%',
      refX2: -remToPx(1.125),
      refY: titleY,
      r: remToPx(0.3125),
      fill: THEME.statusColors.pending,
    };
    attrs.statusLabel = {
      refX: '100%',
      refX2: -remToPx(0.75),
      refY: '100%',
      refY2: -remToPx(0.5),
      fontSize: remToPx(0.625),
      fontFamily: THEME.fontSans,
      fontWeight: 500,
      fill: THEME.node.subtitleFill,
      textAnchor: 'end',
      textVerticalAnchor: 'bottom',
    };
  }

  X6.Graph.registerNode(name, {
    inherit: 'rect',
    width,
    height,
    markup: markupWithStatus,
    attrs,
  }, true);
}

function clampChars(value, min, max) {
  return Math.max(min, Math.min(max, Math.floor(value)));
}

function estimateLineCapacityPx(width, fontSize, minChars) {
  const avgPxPerChar = Math.max(7, fontSize * 0.6);
  return clampChars(width / avgPxPerChar, minChars, Math.max(minChars, Math.floor(width / 6)));
}

function wrapTextToLines(text, maxChars) {
  const source = String(text == null ? '' : text).trim();
  if (!source) return [''];

  const words = source.split(/\s+/);
  const lines = [];
  let current = '';

  for (const word of words) {
    if (!word) continue;
    if (word.length > maxChars) {
      let start = 0;
      while (start < word.length) {
        const chunk = word.slice(start, start + maxChars);
        start += maxChars;
        if (!current) {
          lines.push(chunk);
          current = '';
        } else {
          lines.push(current);
          current = '';
        }
      }
      continue;
    }

    if (!current) {
      current = word;
      continue;
    }

    if (current.length + 1 + word.length <= maxChars) {
      current += ` ${word}`;
      continue;
    }

    lines.push(current);
    current = word;
  }

  if (current) {
    lines.push(current);
  }

  return lines;
}

export function nodeWidth(kind) {
  return remToPx(SIZE_BY_KIND[kind] || 15.625);
}

function minCardHeight(kind) {
  return remToPx(CARD_MIN_HEIGHTS[kind] || 4.625);
}

export function estimateNodeTextHeight(kind, title, subtitle) {
  const width = nodeWidth(kind) || 250;
  const contentWidth = Math.max(remToPx(7.5), width - remToPx(NODE_TEXT_LAYOUT.textPadX));
  const titleBaseY = remToPx(1.125);
  const subtitleBaseY = titleBaseY + remToPx(NODE_TEXT_LAYOUT.titleLineHeight) + remToPx(NODE_TEXT_LAYOUT.titleSubtitleGap);
  const titleFontSize = remToPx(kind === 'agent' ? 0.8125 : 0.875);
  const titleChars = estimateLineCapacityPx(
    contentWidth,
    titleFontSize,
    kind === 'agent' ? NODE_TEXT_LAYOUT.minTitleCharsAgent : NODE_TEXT_LAYOUT.minTitleCharsOther,
  );
  const subtitleChars = estimateLineCapacityPx(contentWidth, remToPx(NODE_TEXT_LAYOUT.subtitleFontSize), NODE_TEXT_LAYOUT.minSubtitleChars);

  const titleLines = wrapTextToLines(title, titleChars);
  const subtitleLines = wrapTextToLines(subtitle, subtitleChars);

  const titleY = titleBaseY;
  const subtitleY = subtitleBaseY;
  const contentBottom = Math.max(
    titleY + titleLines.length * remToPx(NODE_TEXT_LAYOUT.titleLineHeight),
    subtitleY + subtitleLines.length * remToPx(NODE_TEXT_LAYOUT.subtitleLineHeight),
  );
  const taskReserve = kind === 'task' ? remToPx(NODE_TEXT_LAYOUT.taskReserve) : 0;
  const minHeight = minCardHeight(kind);

  return Math.max(
    minHeight,
    Math.ceil(contentBottom + remToPx(NODE_TEXT_LAYOUT.defaultBottomPadding) + taskReserve),
  );
}

export function buildWrappedNodeText(kind, title, subtitle) {
  const width = nodeWidth(kind) || 250;
  const contentWidth = Math.max(remToPx(7.5), width - remToPx(NODE_TEXT_LAYOUT.textPadX));
  const titleFontSize = remToPx(kind === 'agent' ? 0.8125 : 0.875);
  const titleChars = estimateLineCapacityPx(
    contentWidth,
    titleFontSize,
    kind === 'agent' ? NODE_TEXT_LAYOUT.minTitleCharsAgent : NODE_TEXT_LAYOUT.minTitleCharsOther,
  );
  const subtitleChars = estimateLineCapacityPx(contentWidth, remToPx(NODE_TEXT_LAYOUT.subtitleFontSize), NODE_TEXT_LAYOUT.minSubtitleChars);

  return {
    title: wrapTextToLines(title, titleChars).join('\n'),
    subtitle: wrapTextToLines(subtitle, subtitleChars).join('\n'),
  };
}

function kindColor() {
  return THEME.crewAccent;
}

function registerShapes() {
  commonCardShape('ironcrew-crew', nodeWidth('crew'), minCardHeight('crew'), {
    showStatus: false,
    ribbonFill: THEME.crewAccent,
  });

  commonCardShape('ironcrew-agent', nodeWidth('agent'), minCardHeight('agent'), {
    showStatus: false,
    ribbonFill: '#64748b',
  });

  commonCardShape('ironcrew-runtime', nodeWidth('conversation'), minCardHeight('conversation'), {
    showStatus: false,
    ribbonFill: '#475569',
  });

  commonCardShape('ironcrew-task', nodeWidth('task'), minCardHeight('task'), {
    showStatus: true,
    ribbonFill: kindColor(),
  });

  // ── Edge styles — ordered by visual weight (loudest first) ──────
  //
  // Task dependency edges are the PRIMARY flow and should be the most
  // prominent. Structural edges (crew→agent, agent→task) provide
  // context. Supplementary edges (tool, memory, inferred) are quiet.

  // Task → Task dependency — the pipeline. Loudest edge on the canvas.
  X6.Graph.registerEdge('ironcrew-dep', {
    inherit: 'edge',
    attrs: {
      line: {
        stroke: THEME.edge.idle,
        strokeWidth: 3,
        targetMarker: { name: 'block', width: 10, height: 10 },
      },
    },
    connector: { name: 'normal' },
    zIndex: 0,
  }, true);

  // Agent → Task (explicit assignment) — structural context.
  X6.Graph.registerEdge('ironcrew-flow', {
    inherit: 'edge',
    attrs: {
      line: {
        stroke: THEME.edge.ownership,
        strokeWidth: 1.5,
        targetMarker: { name: 'classic', width: 7, height: 7 },
      },
    },
    connector: { name: 'normal' },
    zIndex: -1,
  }, true);

  // Crew → Agent (membership) — background structure.
  X6.Graph.registerEdge('ironcrew-own', {
    inherit: 'edge',
    attrs: {
      line: {
        stroke: THEME.edge.ownership,
        strokeWidth: 1,
        strokeDasharray: '4 4',
        targetMarker: { name: 'classic', width: 6, height: 6 },
      },
    },
    connector: { name: 'normal' },
    zIndex: -2,
  }, true);

  // Agent → Task (auto-resolved / inferred) — supplementary.
  X6.Graph.registerEdge('ironcrew-inferred', {
    inherit: 'edge',
    attrs: {
      line: {
        stroke: THEME.statusColors.running,
        strokeWidth: 1,
        strokeDasharray: '6 4',
        targetMarker: { name: 'classic', width: 6, height: 6 },
      },
    },
    connector: { name: 'normal' },
    zIndex: -1,
  }, true);

  // Agent → Tool (tool access) — supplementary.
  X6.Graph.registerEdge('ironcrew-tool', {
    inherit: 'edge',
    attrs: {
      line: {
        stroke: THEME.edge.tool,
        strokeWidth: 1,
        strokeDasharray: '5 4',
        targetMarker: { name: 'classic', width: 6, height: 6 },
      },
    },
    connector: { name: 'normal' },
    zIndex: -2,
  }, true);

  // Memory edges — supplementary.
  X6.Graph.registerEdge('ironcrew-memory', {
    inherit: 'edge',
    attrs: {
      line: {
        stroke: THEME.edge.memory,
        strokeWidth: 1,
        strokeDasharray: '4 4',
        targetMarker: { name: 'classic', width: 6, height: 6 },
      },
    },
    connector: { name: 'normal' },
    zIndex: -2,
  }, true);

  // Message edges — supplementary.
  X6.Graph.registerEdge('ironcrew-message', {
    inherit: 'edge',
    attrs: {
      line: {
        stroke: THEME.edge.message,
        strokeWidth: 1,
        strokeDasharray: '4 4',
        targetMarker: { name: 'classic', width: 6, height: 6 },
      },
    },
    connector: { name: 'normal' },
    zIndex: -2,
  }, true);
}

let initialized = false;

export function registerPrototypeShapes() {
  if (initialized) return;
  initialized = true;
  registerShapes();
}
