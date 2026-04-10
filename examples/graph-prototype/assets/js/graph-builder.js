/*
 * Graph construction for the prototype.
 */

import { THEME } from './theme.js';
import {
  buildWrappedNodeText,
  estimateNodeTextHeight,
  iconAttrsForKind,
  kindShapeByNode,
  makePorts,
  nodeWidth,
  registerPrototypeShapes,
  portId,
  SIZE_BY_KIND,
} from './graph-shapes.js';
import { normalizeTaskType, resolveTaskTypeLabel } from './icon-registry.js';

function normalizeCrew(crew) {
  return {
    ...crew,
    agents: Array.isArray(crew.agents) ? crew.agents : [],
    tools: Array.isArray(crew.tools) ? crew.tools : [],
    functions: Array.isArray(crew.functions) ? crew.functions : [],
    tasks: Array.isArray(crew.tasks) ? crew.tasks : [],
    memories: Array.isArray(crew.memories) ? crew.memories : [],
    conversations: Array.isArray(crew.conversations) ? crew.conversations : [],
    dialogs: Array.isArray(crew.dialogs) ? crew.dialogs : [],
    messages: Array.isArray(crew.messages) ? crew.messages : [],
  };
}

export const DAGRE_CONFIG = {
  rankdir: 'LR',
  ranksep: 160,
  nodesep: 50,
  marginx: 60,
  marginy: 60,
  align: 'UL',
};

function normalizeTaskKey(value) {
  return String(value == null ? '' : value).trim();
}

function normalizeFunctionKey(value) {
  return String(value == null ? '' : value)
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_]/g, '-')
    .replace(/-+/g, '-')
    .replace(/(^-|-$)/g, '');
}

function taskIdentity(taskOrValue) {
  if (typeof taskOrValue === 'string' || typeof taskOrValue === 'number') {
    return normalizeTaskKey(taskOrValue);
  }

  if (!taskOrValue || typeof taskOrValue !== 'object') {
    return '';
  }

  return (
    normalizeTaskKey(taskOrValue.id) ||
    normalizeTaskKey(taskOrValue.name) ||
    normalizeTaskKey(taskOrValue.task_id) ||
    normalizeTaskKey(taskOrValue.taskId) ||
    normalizeTaskKey(taskOrValue.task_name)
  );
}

function taskNodeId(taskOrValue) {
  return taskIdentity(taskOrValue) ? `task:${taskIdentity(taskOrValue)}` : null;
}

let anonymousTaskCounter = 0;
const anonymousTaskIds = new WeakMap();
function taskNodeIdStable(task) {
  const direct = taskNodeId(task);
  if (direct) return direct;

  if (!task || typeof task !== 'object') {
    return null;
  }

  let existing = anonymousTaskIds.get(task);
  if (existing) return existing;

  anonymousTaskCounter += 1;
  existing = `task:auto-${anonymousTaskCounter}`;
  anonymousTaskIds.set(task, existing);
  return existing;
}

function unique(list) {
  return [...new Set((list || []).filter(Boolean))];
}

function taskMeta(task) {
  const type = normalizeTaskType(task);
  return {
    type,
    label: resolveTaskTypeLabel(task),
    status: task.status || 'pending',
    duration: task.duration ?? null,
  };
}

export function taskOwnerName(task) {
  return task?.agent || task?.resolved_agent || null;
}

function titleForMeta(kind, data) {
  if (!data) return 'Node';
  if (kind === 'task') return data.task.name || taskIdentity(data.task) || 'Task';
  if (kind === 'functions') return 'Functions';
  if (kind === 'function') return data.function?.name || data.function || 'Code';
  if (kind === 'tools-group') return data.title || 'Tools';
  if (kind === 'result') return 'Run complete';
  if (kind === 'agent') return data.agent.name;
  if (kind === 'crew') return data.name;
  if (kind === 'tool') return data.tool?.name || data.tool?.id || 'Tool';
  if (kind === 'memory') return `memory:${data.key}`;
  if (kind === 'conversation') return `conversation:${data.id}`;
  if (kind === 'dialog') return `dialog:${data.id}`;
  if (kind === 'message') return `message:${data.id}`;
  return data.label || 'Node';
}

function subtitleForMeta(kind, data) {
  if (kind === 'task') {
    const owner = taskOwnerName(data.task) || 'unassigned';
    const qualifier = data.task?.assignment_source === 'auto' ? `auto -> ${owner}` : owner;
    return `${resolveTaskTypeLabel(data.task)} • ${qualifier}`;
  }
  if (kind === 'functions') return `${(data.functions || []).length} functions`;
  if (kind === 'function') return `${data.function?.signature || 'Lua function'} • ${data.function?.location || 'line ?'}`;
  if (kind === 'tools-group') return `${(data.tools || []).length} tool${(data.tools || []).length !== 1 ? 's' : ''} available`;
  if (kind === 'result') return `${data.taskCount || 0} tasks • awaiting run`;
  if (kind === 'agent') return `${data.agent.goal}${data.agent.source ? ` • ${data.agent.source.replace(/_/g, ' ')}` : ''}`;
  if (kind === 'tool') return (data.tool.owner || 'unassigned') + ' • tool';
  if (kind === 'memory') return `${data.memory.owner || 'crew'} • memory ${data.memory.action || 'set'}`;
  if (kind === 'conversation') return `${data.conversation.agent || 'agent'} • conversation`;
  if (kind === 'dialog') return `${(data.dialog.agents || []).length} participants • dialog`;
  if (kind === 'message') return `${data.message.from || '?'} → ${data.message.to || '?'} • ${data.message.type || 'message'}`;
  return data.goal || data.description || '';
}

function resolveAgentId(agentName) {
  if (!agentName) return null;
  return `agent:${agentName}`;
}

function resolveToolId(toolName) {
  if (!toolName) return null;
  return `tool:${toolName}`;
}

function resolveMemoryId(memoryKey) {
  if (!memoryKey) return null;
  return `memory:${memoryKey}`;
}

function resolveConversationId(conversationId) {
  if (!conversationId) return null;
  return `conversation:${conversationId}`;
}

function resolveDialogId(dialogId) {
  if (!dialogId) return null;
  return `dialog:${dialogId}`;
}

function resolveMessageId(messageId) {
  if (!messageId) return null;
  return `message:${messageId}`;
}

function resolveFunctionId(functionName, fallbackIndex = 0) {
  const normalized = normalizeFunctionKey(functionName);
  if (normalized) return `function:${normalized}`;
  return `function:fn-${fallbackIndex + 1}`;
}

function computeTaskExecutionOrder(tasks) {
  const normalized = [];
  const taskOrder = new Map();

  tasks.forEach((task, index) => {
    const id = taskIdentity(task);
    if (!id) return;
    normalized.push({ task, taskId: id });
    taskOrder.set(id, index);
  });

  const byId = new Map(normalized.map(({ taskId, task }) => [taskId, task]));
  const dependents = new Map();
  const inDeg = new Map();

  for (const { taskId } of normalized) {
    dependents.set(taskId, []);
    inDeg.set(taskId, 0);
  }

  for (const { task, taskId } of normalized) {
    for (const dep of task.depends_on || []) {
      const depId = taskIdentity(dep);
      if (!depId || !byId.has(depId)) continue;
      dependents.get(depId).push(taskId);
      inDeg.set(taskId, (inDeg.get(taskId) || 0) + 1);
    }
  }

  const queue = normalized
    .map(({ taskId }) => taskId)
    .filter((taskId) => (inDeg.get(taskId) || 0) === 0)
    .sort((a, b) => (taskOrder.get(a) || 0) - (taskOrder.get(b) || 0));

  const ordered = [];
  while (queue.length > 0) {
    const taskId = queue.shift();
    ordered.push(byId.get(taskId));

    for (const dependentId of dependents.get(taskId) || []) {
      inDeg.set(dependentId, (inDeg.get(dependentId) || 0) - 1);
      if ((inDeg.get(dependentId) || 0) === 0) {
        queue.push(dependentId);
        queue.sort((a, b) => (taskOrder.get(a) || 0) - (taskOrder.get(b) || 0));
      }
    }
  }

  if (ordered.length !== normalized.length) {
    return tasks.slice();
  }

  return ordered;
}

function edgePortLayoutForKinds(sourceKind, targetKind) {
  if (sourceKind === 'crew' && targetKind === 'agent') {
    return { sourcePort: 'out', targetPort: 'in' };
  }
  if (sourceKind === 'task' && targetKind === 'task') {
    return { sourcePort: 'bottom', targetPort: 'top' };
  }
  if (sourceKind === 'task' && targetKind === 'result') {
    return { sourcePort: 'out', targetPort: 'in' };
  }
  if (sourceKind === 'agent' && targetKind === 'task') {
    return { sourcePort: 'out', targetPort: 'in' };
  }
  if (targetKind === 'tool') {
    if (sourceKind === 'crew') {
      return { sourcePort: 'out', targetPort: 'out' };
    }
    return { sourcePort: 'in', targetPort: 'out' };
  }
  if (sourceKind === 'agent') {
    return { sourcePort: 'out', targetPort: 'in' };
  }
  if (targetKind === 'agent') {
    return { sourcePort: 'out', targetPort: 'in' };
  }

  return { sourcePort: 'out', targetPort: 'in' };
}

function makePrototypeGraph(containerEl) {
  return new window.X6.Graph({
    container: containerEl,
    embedding: { enabled: true },
    panning: { enabled: true, eventTypes: ['leftMouseDown', 'mouseWheel'] },
    mousewheel: {
      enabled: true,
      modifiers: 'ctrl',
      factor: 1.1,
      maxScale: 2,
      minScale: 0.3,
    },
    interacting: {
      nodeMovable: false,
      edgeMovable: false,
    },
  });
}

export function createPrototypeGraphModel(crewInput, container) {
  registerPrototypeShapes();
  const graph = makePrototypeGraph(container);
  const crew = normalizeCrew(crewInput);
  const nodeDefs = [];
  const nodeDefById = new Map();
  const taskNodesByTaskId = new Map();
  const taskNodesByTaskRef = new WeakMap();
  const edges = [];
  const addedEdgeSignatures = new Set();
  const parentChildRelations = [];
  const parentChildSignatures = new Set();
  const usedFunctionIds = new Set();

  function addNodeDef(kind, id, meta, extra = {}) {
    if (nodeDefById.has(id)) return nodeDefById.get(id);

    const baseTitle = titleForMeta(kind, meta);
    const baseSubtitle = subtitleForMeta(kind, meta);
    const title = typeof extra.title === 'string' ? extra.title : baseTitle;
    const subtitle = typeof extra.subtitle === 'string' ? extra.subtitle : baseSubtitle;
    const wrappedText = buildWrappedNodeText(kind, title, subtitle);
    const nodeHeight = estimateNodeTextHeight(kind, wrappedText.title, wrappedText.subtitle);
    const iconTaskRef = kind === 'task' ? meta.task : extra.task;
    const iconAttrs = iconAttrsForKind(kind, iconTaskRef);

    const definition = {
      id,
      kind,
      meta,
      graph: {
        shape: kindShapeByNode[kind] || 'ironcrew-runtime',
        data: { kind, status: 'pending', ...extra },
        attrs: {
          ...iconAttrs,
          title: { text: wrappedText.title },
          subtitle: { text: wrappedText.subtitle },
          header: { fill: '#1a2538' },
        },
        ports: makePorts(kind, id),
        width: nodeWidth(kind),
        height: nodeHeight,
      },
    };

    if (kind === 'task') {
      definition.graph.data.task = meta.task;
      const type = taskMeta(meta.task);
      definition.graph.data.status = type.status;
      definition.graph.data.duration = type.duration;
      definition.graph.attrs.statusDot = { fill: THEME.statusColors.pending };
      definition.graph.attrs.statusLabel = { text: '' };
    }
    if (kind === 'functions') definition.graph.data.functions = meta.functions || [];
    if (kind === 'function') definition.graph.data.function = meta.function;
    if (kind === 'agent') definition.graph.data.agent = meta.agent;
    if (kind === 'memory') definition.graph.data.memory = meta.memory;
    if (kind === 'conversation') definition.graph.data.conversation = meta.conversation;
    if (kind === 'dialog') definition.graph.data.dialog = meta.dialog;
    if (kind === 'tool') definition.graph.data.tool = meta.tool;
    if (kind === 'message') definition.graph.data.message = meta.message;

    nodeDefById.set(id, definition);
    nodeDefs.push(definition);
    return definition;
  }

  function edgeId(source, target, shape) {
    return `${source}->${target}::${shape}`;
  }

  function addEdgeDef(source, target, shape) {
    if (!source || !target) return;
    const key = edgeId(source, target, shape);
    if (addedEdgeSignatures.has(key)) return;
    edges.push({ source, target, shape });
    addedEdgeSignatures.add(key);
  }

  function addParentChildDef(parentId, childId) {
    if (!parentId || !childId) return;
    const signature = `${parentId}->${childId}`;
    if (parentChildSignatures.has(signature)) return;
    parentChildRelations.push({ parent: parentId, child: childId });
    parentChildSignatures.add(signature);
  }

  const toolByName = new Map();
  for (const tool of crew.tools) {
    if (!tool?.name) continue;
    toolByName.set(tool.name, tool);
  }

  const crewNodeDef = addNodeDef('crew', 'crew', {
    name: crew.name,
    goal: crew.goal,
    provider: crew.provider,
    model: crew.model,
  }, {
    subtitle: `${crew.provider} • ${crew.model}`,
  });

  const functionsNodeDef = crew.functions.length > 0
    ? addNodeDef('functions', 'functions', { functions: crew.functions, label: 'Functions', title: 'Functions' })
    : null;

  if (functionsNodeDef) {
    crew.functions.forEach((fn, index) => {
      if (!fn || typeof fn !== 'object') return;
      const name = fn.name || `function-${index + 1}`;
      let fnId = resolveFunctionId(name);
      let suffix = 1;
      while (usedFunctionIds.has(fnId)) {
        suffix += 1;
        fnId = `${resolveFunctionId(name)}-${suffix}`;
      }

      usedFunctionIds.add(fnId);
      addNodeDef('function', fnId, { function: fn, name });
      addParentChildDef(functionsNodeDef.id, fnId);
    });
  }

  for (const agent of crew.agents) {
    if (!agent?.name) continue;
    const id = resolveAgentId(agent.name);
    addNodeDef('agent', id, { agent, name: agent.name });
    addEdgeDef(crewNodeDef.id, id, 'ironcrew-flow');
  }

  for (const tool of crew.tools) {
    if (!tool?.name) continue;
    const id = resolveToolId(tool.name);
    addNodeDef('tool', id, { tool, name: tool.name, memory: null });
  }

  const toolsContainerDef = crew.tools.length > 0
    ? addNodeDef('tools-group', 'tools-group', {
      name: 'Tools',
      title: 'Tools',
      tools: crew.tools,
    }, { subtitle: `${crew.tools.length} tool${crew.tools.length !== 1 ? 's' : ''} available` })
    : null;

  if (toolsContainerDef) {
    for (const tool of crew.tools) {
      if (!tool?.name) continue;
      const toolId = resolveToolId(tool.name);
      if (nodeDefById.has(toolId)) {
        addParentChildDef('tools-group', toolId);
      }
    }
  }

  for (const agent of crew.agents) {
    if (!agent?.name || !Array.isArray(agent.tools)) continue;
    const agentId = resolveAgentId(agent.name);
    for (const toolName of agent.tools) {
      const toolId = resolveToolId(toolName);
      if (toolId && nodeDefById.has(toolId)) {
        addEdgeDef(agentId, toolId, 'ironcrew-tool');
      }
    }
  }

  for (const [memoryIndex, memory] of crew.memories.entries()) {
    const memoryKey = memory.key || `memory-${memoryIndex + 1}`;
    const id = resolveMemoryId(memoryKey);
    addNodeDef('memory', id, { memory, key: memoryKey, value: memory.value });
    addEdgeDef(crewNodeDef.id, id, 'ironcrew-memory');
  }

  for (const [conversationIndex, conv] of crew.conversations.entries()) {
    const conversationId = conv.id || `conversation-${conversationIndex + 1}`;
    const id = resolveConversationId(conversationId);
    addNodeDef('conversation', id, { conversation: conv, id: conversationId, agent: conv.agent });
    addEdgeDef(crewNodeDef.id, id, 'ironcrew-own');
    const ownerId = resolveAgentId(conv.agent);
    if (ownerId) addEdgeDef(ownerId, id, 'ironcrew-own');
  }

  for (const [dialogIndex, dialog] of crew.dialogs.entries()) {
    const dialogId = dialog.id || `dialog-${dialogIndex + 1}`;
    const id = resolveDialogId(dialogId);
    addNodeDef('dialog', id, { dialog, id: dialogId, participants: dialog.agents });
    addEdgeDef(crewNodeDef.id, id, 'ironcrew-own');
    for (const participant of dialog.agents || []) {
      const ownerId = resolveAgentId(participant);
      if (ownerId) addEdgeDef(ownerId, id, 'ironcrew-own');
    }
  }

  for (const [messageIndex, message] of crew.messages.entries()) {
    const messageId = message.id || `message-${messageIndex + 1}`;
    const id = resolveMessageId(messageId);
    addNodeDef('message', id, { message, id: messageId, msg_type: message.type || 'message' });
    const sourceAgent = resolveAgentId(message.from);
    const targetAgent = resolveAgentId(message.to);
    const msgType = message.type || 'broadcast';
    const isRequest = msgType === 'request';
    if (sourceAgent) addEdgeDef(sourceAgent, id, isRequest ? 'ironcrew-message' : 'ironcrew-own');
    if (targetAgent) {
      addEdgeDef(id, targetAgent, isRequest ? 'ironcrew-message' : 'ironcrew-own');
      continue;
    }

    if (message.to === '*') {
      for (const recipient of crew.agents) {
        const recipientId = resolveAgentId(recipient?.name);
        if (!recipientId || recipientId === sourceAgent) continue;
        addEdgeDef(id, recipientId, 'ironcrew-message');
      }
    }
  }

  for (const task of crew.tasks) {
    const id = taskNodeIdStable(task);
    if (!id) continue;
    addNodeDef('task', id, { task, taskId: taskIdentity(task) });
    const identityKey = taskIdentity(task);
    if (identityKey) taskNodesByTaskId.set(identityKey, id);
    if (task && typeof task === 'object') taskNodesByTaskRef.set(task, id);
  }

  for (const task of crew.tasks) {
    const id = taskNodeIdStable(task);
    if (!id) continue;

    const agentId = resolveAgentId(task.agent);
    if (agentId && nodeDefById.has(agentId)) addEdgeDef(agentId, id, 'ironcrew-flow');
    const inferredAgentId = !task.agent ? resolveAgentId(task.resolved_agent) : null;
    if (inferredAgentId && nodeDefById.has(inferredAgentId)) addEdgeDef(inferredAgentId, id, 'ironcrew-inferred');

    for (const dep of task.depends_on || []) {
      const depIdentity = taskIdentity(dep);
      let depId = taskNodesByTaskId.get(depIdentity);
      if (!depId && dep && typeof dep === 'object') depId = taskNodesByTaskRef.get(dep);
      if (depId) addEdgeDef(depId, id, 'ironcrew-dep');
    }

    for (const toolName of unique(task.tools || [])) {
      const clean = normalizeTaskKey(toolName);
      if (!clean) continue;
      const tool = toolByName.get(clean);
      const toolId = resolveToolId(clean);
      if (!tool) {
        addNodeDef('tool', toolId, { tool: { name: clean, owner: 'unlisted', description: 'tool not declared in crew.tools' }, name: clean });
      }
      addEdgeDef(id, toolId || id, 'ironcrew-tool');
    }

    for (const key of unique([...(task.memory_reads || []), ...(task.memory_writes || [])])) {
      if (!key) continue;
      const memId = resolveMemoryId(key);
      const existing = crew.memories.find((m) => m.key === key);
      if (!existing) {
        addNodeDef('memory', memId, { memory: { key, owner: 'task', action: task.memory_writes?.includes(key) ? 'write' : 'read' }, key });
        addEdgeDef(crewNodeDef.id, memId, 'ironcrew-memory');
      }
      if (existing) addEdgeDef(id, memId, task.memory_writes?.includes(key) ? 'ironcrew-memory' : 'ironcrew-own');
    }

    const conversationId = resolveConversationId(task.conversation_id);
    if (conversationId) addEdgeDef(conversationId, id, 'ironcrew-memory');

    const dialogId = resolveDialogId(task.dialog_id);
    if (dialogId) addEdgeDef(dialogId, id, 'ironcrew-memory');
  }

  const allDepTargets = new Set();
  for (const task of crew.tasks) {
    for (const dep of task.depends_on || []) allDepTargets.add(taskIdentity(dep));
  }
  const leafTasks = crew.tasks.filter((t) => {
    const id = taskIdentity(t);
    return id && !allDepTargets.has(id);
  });
  const resultNodeDef = addNodeDef('result', 'result', {
    taskCount: crew.tasks.length,
    leafCount: leafTasks.length,
  }, {
    subtitle: `${crew.tasks.length} tasks • awaiting run`,
  });
  for (const leaf of leafTasks) {
    const leafNodeId = taskNodeIdStable(leaf);
    if (leafNodeId && nodeDefById.has(leafNodeId)) addEdgeDef(leafNodeId, 'result', 'ironcrew-flow');
  }

  const dagrePositions = new Map();

  function nodeBox(def) {
    const pos = dagrePositions.get(def.id) || { x: 40, y: 40 };
    const width = def.graph.width || 250;
    const height = def.graph.height || 74;
    return {
      x: pos.x,
      y: pos.y,
      width,
      height,
      right: pos.x + width,
      bottom: pos.y + height,
      cx: pos.x + width / 2,
      cy: pos.y + height / 2,
    };
  }

  function computeExtents() {
    let minX = Number.POSITIVE_INFINITY;
    let minY = Number.POSITIVE_INFINITY;
    let maxX = Number.NEGATIVE_INFINITY;
    let maxY = Number.NEGATIVE_INFINITY;

    for (const def of nodeDefs) {
      if (def.id === 'tools-group') continue;
      if (!dagrePositions.has(def.id)) continue;
      const box = nodeBox(def);
      minX = Math.min(minX, box.x);
      minY = Math.min(minY, box.y);
      maxX = Math.max(maxX, box.right);
      maxY = Math.max(maxY, box.bottom);
    }

    if (!Number.isFinite(minX)) {
      return { minX: 60, minY: 60, maxX: 60, maxY: 60 };
    }

    return { minX, minY, maxX, maxY };
  }

  const crewStartX = 60;
  const crewY = 220;
  const shellStep = 480;
  const agentShellRadius = shellStep;
  const taskShellRadius = shellStep * 2;
  const resultShellRadius = shellStep * 3;
  const shellAngleStart = -0.3;
  const shellAngleEnd = 0.3;
  const taskClusterSpread = 0.16;

  function polarTopLeft(cx, cy, radius, angle, width, height) {
    return {
      x: Math.round(cx + Math.cos(angle) * radius - width / 2),
      y: Math.round(cy + Math.sin(angle) * radius - height / 2),
    };
  }

  function spreadAngles(centerAngle, count, spread) {
    if (count <= 1) return [centerAngle];
    const start = centerAngle - spread / 2;
    const step = spread / (count - 1);
    return Array.from({ length: count }, (_, index) => start + step * index);
  }

  function taskFanCenter(agentAngle, taskCount) {
    if (taskCount <= 0) return agentAngle;
    if (Math.abs(agentAngle) < 0.01) return agentAngle;
    return agentAngle + (agentAngle > 0 ? 0.08 : -0.08);
  }

  dagrePositions.set(crewNodeDef.id, { x: crewStartX, y: crewY });
  const crewRootBox = nodeBox(crewNodeDef);
  const rootCx = crewRootBox.cx;
  const rootCy = crewRootBox.cy;

  const orderedTasks = computeTaskExecutionOrder(crew.tasks);
  const tasksByAgentId = new Map();
  const unassignedTaskDefs = [];

  for (const task of orderedTasks) {
    const taskId = taskNodeIdStable(task);
    const taskDef = nodeDefById.get(taskId);
    if (!taskDef) continue;

    const agentId = resolveAgentId(taskOwnerName(task));
    if (!agentId || !nodeDefById.has(agentId)) {
      unassignedTaskDefs.push(taskDef);
      continue;
    }

    if (!tasksByAgentId.has(agentId)) tasksByAgentId.set(agentId, []);
    tasksByAgentId.get(agentId).push(taskDef);
  }

  const agentCount = Math.max(1, crew.agents.length);
  const agentAngles = new Map();

  for (const [index, agent] of crew.agents.entries()) {
    const agentId = resolveAgentId(agent?.name);
    const agentDef = agentId ? nodeDefById.get(agentId) : null;
    if (!agentDef) continue;

    const progress = agentCount === 1 ? 0.5 : index / (agentCount - 1);
    const theta = shellAngleStart + (shellAngleEnd - shellAngleStart) * progress;
    agentAngles.set(agentId, theta);
    dagrePositions.set(
      agentDef.id,
      polarTopLeft(
        rootCx,
        rootCy,
        agentShellRadius,
        theta,
        agentDef.graph.width || 230,
        agentDef.graph.height || 62,
      ),
    );
  }

  if (unassignedTaskDefs.length > 0) {
    const extraAngles = spreadAngles(shellAngleEnd + 0.12, unassignedTaskDefs.length, 0.14);
    unassignedTaskDefs.forEach((taskDef, index) => {
      dagrePositions.set(
        taskDef.id,
        polarTopLeft(
          rootCx,
          rootCy,
          taskShellRadius,
          extraAngles[index],
          taskDef.graph.width || 250,
          taskDef.graph.height || 86,
        ),
      );
    });
  }

  for (const agent of crew.agents) {
    const agentId = resolveAgentId(agent?.name);
    const agentTasks = tasksByAgentId.get(agentId) || [];
    const centerAngle = agentAngles.get(agentId) || 0;
    const taskAngles = spreadAngles(taskFanCenter(centerAngle, agentTasks.length), agentTasks.length, taskClusterSpread);

    agentTasks.forEach((taskDef, index) => {
      dagrePositions.set(
        taskDef.id,
        polarTopLeft(
          rootCx,
          rootCy,
          taskShellRadius,
          taskAngles[index],
          taskDef.graph.width || 250,
          taskDef.graph.height || 86,
        ),
      );
    });
  }

  dagrePositions.set(
    resultNodeDef.id,
    polarTopLeft(
      rootCx,
      rootCy,
      resultShellRadius,
      0,
      resultNodeDef.graph.width || 260,
      resultNodeDef.graph.height || 74,
    ),
  );

  let layoutExtents = computeExtents();

  const toolDefs = crew.tools
    .map((tool) => nodeDefById.get(resolveToolId(tool.name)))
    .filter(Boolean)
    .sort((a, b) => a.id.localeCompare(b.id));

  if (toolsContainerDef && toolDefs.length > 0) {
    const crewBox = nodeBox(crewNodeDef);
    const pad = 16;
    const headerH = toolsContainerDef.graph.height || 72;
    const headerGap = 12;
    const childGapY = 16;
    const maxToolWidth = Math.max(...toolDefs.map((def) => def.graph.width || 235));
    const groupWidth = Math.max(toolsContainerDef.graph.width || 300, maxToolWidth + pad * 2);
    const groupX = crewBox.cx - groupWidth / 2;
    const groupY = crewBox.bottom + 56;
    let childY = groupY + headerH + headerGap;

    toolDefs.forEach((toolDef) => {
      dagrePositions.set(toolDef.id, { x: groupX + pad, y: childY });
      childY += (toolDef.graph.height || 54) + childGapY;
    });

    toolsContainerDef.graph.width = groupWidth;
    toolsContainerDef.graph.height = Math.max(headerH, childY - groupY - childGapY + pad);
    dagrePositions.set('tools-group', { x: groupX, y: groupY });
  } else if (toolsContainerDef) {
    dagrePositions.set('tools-group', { x: -9999, y: -9999 });
  }

  layoutExtents = computeExtents();

  if (functionsNodeDef) {
    const functionDefs = nodeDefs.filter((def) => def.kind === 'function');
    const groupX = layoutExtents.maxX + 180;
    const groupY = layoutExtents.minY;
    const headerH = functionsNodeDef.graph.height || 72;
    const childPadX = 24;
    const childGapY = 16;
    let childY = groupY + headerH + 12;
    let maxChildWidth = 0;

    for (const functionDef of functionDefs) {
      dagrePositions.set(functionDef.id, { x: groupX + childPadX, y: childY });
      childY += (functionDef.graph.height || 72) + childGapY;
      maxChildWidth = Math.max(maxChildWidth, functionDef.graph.width || 250);
    }

    functionsNodeDef.graph.width = Math.max(functionsNodeDef.graph.width || 300, maxChildWidth + childPadX * 2);
    functionsNodeDef.graph.height = Math.max(functionsNodeDef.graph.height || 72, childY - groupY + 8);
    dagrePositions.set(functionsNodeDef.id, { x: groupX, y: groupY });
  }

  layoutExtents = computeExtents();

  let sideX = layoutExtents.maxX + 180;
  let sideY = layoutExtents.minY;
  for (const def of nodeDefs) {
    if (dagrePositions.has(def.id) || def.id === 'tools-group') continue;
    dagrePositions.set(def.id, { x: sideX, y: sideY });
    sideY += (def.graph.height || 74) + 20;
  }

  for (const def of nodeDefs) {
    const pos = dagrePositions.get(def.id) || { x: 40, y: 40 };
    def.graph.x = pos.x;
    def.graph.y = pos.y;
    def.graph.width = def.graph.width || SIZE_BY_KIND[def.kind] || 250;
    const node = graph.addNode(def.graph);
    def.cell = node;
  }

  for (const { parent, child } of parentChildRelations) {
    if (parent === 'tools-group') continue;
    const parentNode = nodeDefById.get(parent)?.cell;
    const childNode = nodeDefById.get(child)?.cell;
    if (!parentNode || !childNode) continue;
    parentNode.addChild(childNode);
  }

  if (toolsContainerDef?.cell) {
    toolsContainerDef.cell.setZIndex(-5);
    toolsContainerDef.cell.attr('body/fill', '#151d2e');
    toolsContainerDef.cell.attr('body/stroke', '#334155');
    toolsContainerDef.cell.attr('body/strokeDasharray', '6 3');
    toolsContainerDef.cell.attr('header/opacity', 0.5);
  }

  function portDirection(portName) {
    if (portName === 'in' || portName === 'left') return 'left';
    if (portName === 'out' || portName === 'right') return 'right';
    if (portName === 'top') return 'top';
    if (portName === 'bottom') return 'bottom';
    return null;
  }

  function routerForEdge(shape, sourcePort, targetPort) {
    return undefined;
  }

  const usedPorts = new Set();
  for (const edge of edges) {
    if (!nodeDefById.has(edge.source) || !nodeDefById.has(edge.target)) continue;
    const src = nodeDefById.get(edge.source)?.cell;
    const dst = nodeDefById.get(edge.target)?.cell;
    if (!src || !dst) continue;

    const sourceDef = nodeDefById.get(edge.source);
    const targetDef = nodeDefById.get(edge.target);
    const { sourcePort, targetPort } = edgePortLayoutForKinds(sourceDef?.kind, targetDef?.kind);
    const sourcePortId = portId(sourceDef.id, sourcePort);
    const targetPortId = portId(targetDef.id, targetPort);

    usedPorts.add(`${src.id}:${sourcePortId}`);
    usedPorts.add(`${dst.id}:${targetPortId}`);
    graph.addEdge({
      shape: edge.shape,
      source: { cell: src.id, port: sourcePortId },
      target: { cell: dst.id, port: targetPortId },
      router: routerForEdge(edge.shape, sourcePort, targetPort),
    });
  }

  for (const def of nodeDefs) {
    const node = def.cell;
    if (!node) continue;
    for (const port of node.getPorts() || []) {
      const key = `${node.id}:${port.id}`;
      if (usedPorts.has(key)) node.setPortProp(port.id, 'attrs/circle/r', 4);
    }
  }

  graph.zoomToFit({ padding: { left: 40, right: 40, top: 40, bottom: 100 }, maxScale: 1 });

  return {
    graph,
    nodeDefById,
    nodeDefs,
    crew,
    crewNodeDef,
    resultNodeDef,
    taskExecutionOrder: computeTaskExecutionOrder(crew.tasks),
    taskNodeIdStable,
    taskOwnerName,
    resolveAgentId,
  };
}
