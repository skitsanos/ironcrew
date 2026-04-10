/*
 * Central icon registry for the DAG prototype.
 *
 * Keeps icon definitions in one place so:
 *  - node rendering
 *  - legend generation
 *  - inspector icon rendering
 * all use the same icon source + labels.
 */

const IMAGES_PATH = 'assets/images';
const SPRITES_PATH = 'assets/images/icons.svg';

function imageIcon(fileName) {
  // Bundled mode: the Rust HTML generator injects __ICON_DATA_URIS with
  // inline data URIs for each SVG. Fall back to file paths for dev mode.
  if (typeof __ICON_DATA_URIS !== 'undefined' && __ICON_DATA_URIS[fileName]) {
    return { type: 'image', href: __ICON_DATA_URIS[fileName] };
  }
  return { type: 'image', href: `${IMAGES_PATH}/${fileName}` };
}

function spriteIcon(symbolId) {
  return { type: 'sprite', href: `${SPRITES_PATH}#${symbolId}` };
}

function htmlEscape(value) {
  return String(value == null ? '' : value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#039;');
}

export const NODE_ICON_DEFS = {
  crew: { kind: 'crew', label: 'Crew', icon: imageIcon('crew.svg') },
  agent: { kind: 'agent', label: 'Agent', icon: imageIcon('agent.svg') },
  task: { kind: 'task', label: 'Task', icon: imageIcon('task.svg') },
  functions: { kind: 'functions', label: 'Functions', icon: imageIcon('code.svg') },
  function: { kind: 'function', label: 'Code', icon: imageIcon('code.svg') },
  'tools-group': { kind: 'tools-group', label: 'Tools', icon: imageIcon('code.svg') },
  result: { kind: 'result', label: 'Result', icon: imageIcon('result.svg') },
  tool: { kind: 'tool', label: 'Tool', icon: imageIcon('tool-call.svg') },
  memory: { kind: 'memory', label: 'Memory', icon: imageIcon('memory.svg') },
  conversation: { kind: 'conversation', label: 'Conversation', icon: imageIcon('chat.svg') },
  dialog: { kind: 'dialog', label: 'Dialog', icon: imageIcon('dialog.svg') },
  message: { kind: 'message', label: 'Message', icon: imageIcon('message.svg') },

  // Fallback icons if a node kind is not explicitly mapped.
  runtime: { kind: 'runtime', label: 'Runtime', icon: spriteIcon('icon-message') },
  default: { kind: 'default', label: 'Node', icon: spriteIcon('icon-task') },
};

export const TASK_TYPE_ICON_DEFS = {
  task: { key: 'task', label: 'task', legendLabel: 'task', inspectorLabel: 'task', icon: imageIcon('task.svg') },
  standard: { key: 'standard', label: 'task', legendLabel: 'task', inspectorLabel: 'task', icon: imageIcon('task.svg') },
  retry: { key: 'retry', label: 'retry', legendLabel: 'retry', inspectorLabel: 'task • retry', icon: imageIcon('retry.svg') },
  condition: { key: 'condition', label: 'condition', legendLabel: 'condition', inspectorLabel: 'task • condition', icon: imageIcon('condition.svg') },
  foreach: { key: 'foreach', label: 'loop', legendLabel: 'loop', inspectorLabel: 'task • foreach', icon: imageIcon('loop.svg') },
  foreach_parallel: {
    key: 'foreach_parallel',
    label: 'parallel',
    legendLabel: 'parallel',
    inspectorLabel: 'task • foreach • parallel',
    icon: imageIcon('parallel.svg'),
  },
  collaborative: {
    key: 'collaborative',
    label: 'collab',
    legendLabel: 'collab',
    inspectorLabel: 'collaborative',
    icon: imageIcon('collab.svg'),
  },
  subworkflow: {
    key: 'subworkflow',
    label: 'subflow',
    legendLabel: 'subflow',
    inspectorLabel: 'subworkflow',
    icon: imageIcon('subflow.svg'),
  },
};

export const LEGEND_SECTIONS = [
  { title: 'Structural nodes', items: ['crew', 'agent', 'task', 'functions', 'result'] },
  { title: 'Task variants', items: ['retry', 'condition', 'foreach', 'foreach_parallel', 'collaborative', 'subworkflow'] },
  { title: 'Runtime events', items: ['conversation', 'dialog', 'tool', 'function', 'memory', 'message'] },
];

export function normalizeTaskType(taskOrType) {
  const configured = (typeof taskOrType === 'string'
    ? taskOrType
    : taskOrType?.task_type || taskOrType?.type || taskOrType?.kind || 'task');

  const normalized = String(configured == null ? '' : configured).trim();
  if (!normalized) return 'task';
  return normalized === 'standard' ? 'task' : normalized;
}

export function resolveNodeIcon(kind, taskOrType = null) {
  if (kind === 'task') return resolveTaskTypeIcon(taskOrType).icon || TASK_TYPE_ICON_DEFS.task.icon;
  return (NODE_ICON_DEFS[kind] || NODE_ICON_DEFS.default).icon;
}

export function resolveTaskTypeIcon(taskOrType) {
  const normalized = normalizeTaskType(taskOrType);
  return TASK_TYPE_ICON_DEFS[normalized] || TASK_TYPE_ICON_DEFS.task;
}

export function resolveTaskTypeLabel(taskOrType) {
  return resolveTaskTypeIcon(taskOrType).inspectorLabel || resolveTaskTypeIcon(taskOrType).label || resolveTaskTypeIcon(taskOrType).key;
}

export function iconToMarkup(iconRef, { className = 'legend-icon', title = '' } = {}) {
  if (!iconRef) return '';
  if (iconRef.type === 'image') {
    const safeTitle = htmlEscape(title);
    return `<img src="${iconRef.href}" alt="${safeTitle}" class="${className}" />`;
  }
  return `<svg class="${className}"><use href="${iconRef.href}"></use></svg>`;
}

export function buildLegendGroups(agentNames = []) {
  const sections = LEGEND_SECTIONS.map((section) => ({
    title: section.title,
    entries: section.items.map((entryKey) => {
      if (TASK_TYPE_ICON_DEFS[entryKey]) {
        const typeDef = TASK_TYPE_ICON_DEFS[entryKey];
        return {
          label: typeDef.legendLabel || typeDef.label || entryKey,
          icon: typeDef.icon,
        };
      }

      const kindDef = NODE_ICON_DEFS[entryKey];
      return {
        label: kindDef?.label || entryKey,
        icon: kindDef?.icon || NODE_ICON_DEFS.default.icon,
      };
    }),
  }));

  const agents = Array.isArray(agentNames)
    ? agentNames.filter(Boolean).map((name) => String(name).trim()).filter(Boolean)
    : [];
  if (agents.length > 0) {
    sections.push({
      title: 'Agents',
      entries: agents.map((name) => ({
        label: name,
        icon: resolveNodeIcon('agent'),
      })),
    });
  }

  return sections;
}
