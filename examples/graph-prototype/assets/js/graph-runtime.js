import { THEME } from './theme.js';
import {
  iconToMarkup,
  resolveNodeIcon,
  resolveTaskTypeIcon,
  resolveTaskTypeLabel,
} from './icon-registry.js';

export function createPrototypeRuntime({
  graph,
  nodeDefById,
  nodeDefs,
  crew,
  crewNodeDef,
  resultNodeDef,
  inspector,
  taskExecutionOrder = [],
  taskNodeIdStable,
  taskOwnerName,
  resolveAgentId,
}) {
  const runningEdgeAnimations = new Map();
  let hoveredNodeId = null;
  let selectedNodeId = null;

  function clearSelection() {
    if (selectedNodeId) {
      const prev = graph.getCellById(selectedNodeId);
      if (prev) {
        prev.attr('body/stroke', THEME.node.bodyStroke);
        prev.attr('body/strokeWidth', 1);
        prev.attr('header/fill', '#1a2538');
      }
      selectedNodeId = null;
    }
  }

  function selectNode(node) {
    clearSelection();
    selectedNodeId = node.id;
    node.attr('body/stroke', THEME.statusColors.running);
    node.attr('body/strokeWidth', 2);
    // Tint the header to match the selection color.
    node.attr('header/fill', '#1e3a5f');
  }

  function renderNamedField(label, text, iconRef, iconTitle) {
    return `
      <div class="field">
        <div class="label">${label}</div>
        <div class="value">${iconToMarkup(iconRef, { className: 'inline-icon', title: iconTitle })} ${escapeHtml(text || '-')}</div>
      </div>
    `;
  }

  function renderValueLines(items) {
    return items
      .map(({ label, value }) => `<div class="field"><div class="label">${label}</div><div class="value block">${value}</div></div>`)
      .join('');
  }

  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#039;');
  }

  function toStatusChip(status) {
    return `<span class="status-pill ${status}">${status}</span>`;
  }

  function renderCrewNode(crewData) {
    inspector.innerHTML = `
      <h2>Crew</h2>
      ${renderNamedField('Name', crewData.name, resolveNodeIcon('crew'), 'crew')}
      ${renderValueLines([
        { label: 'Goal', value: crewData.goal || '-' },
        { label: 'Provider', value: crewData.provider || '-' },
        { label: 'Model', value: crewData.model || '-' },
        { label: 'Memory mode', value: crewData.memory || 'ephemeral' },
        { label: 'Max concurrent', value: crewData.max_concurrent || '-' },
      ])}
      <div class="field">
        <div class="label">Counts</div>
        <div class="value small">Agents: ${crew.agents.length} · Tasks: ${crew.tasks.length} · Tools: ${crew.tools.length} · Messages: ${crew.messages.length}</div>
      </div>
    `;
  }

  function renderAgentNode(agent) {
    inspector.innerHTML = `
      <h2>Agent</h2>
      ${renderNamedField('Name', agent.name, resolveNodeIcon('agent'), 'agent')}
      ${renderValueLines([
        { label: 'Goal', value: agent.goal || '-' },
        { label: 'Capabilities', value: (agent.capabilities || []).join(', ') || '-' },
        { label: 'Tools', value: (agent.tools || []).join(', ') || 'none' },
        { label: 'Temperature', value: agent.temperature == null ? '-' : String(agent.temperature) },
        { label: 'Model', value: agent.model || '-' },
      ])}
    `;
  }

  function renderTaskNode(task) {
    const owner = taskOwnerName ? taskOwnerName(task) : null;
    const ownershipLabel = task.assignment_source === 'auto' ? `${owner || 'auto-selected'} (auto-selected)` : owner || 'auto-selected';
    const taskType = resolveTaskTypeLabel(task);
    const typeIcon = resolveTaskTypeIcon(task).icon;

    inspector.innerHTML = `
      <h2>Task</h2>
      ${renderNamedField('Name', task.name || task.id || task.task_id, typeIcon, taskType)}
      ${renderValueLines([
        { label: 'ID', value: task.id || task.task_id || '-' },
        { label: 'Agent', value: ownershipLabel },
        { label: 'Description', value: task.description },
        { label: 'Depends on', value: (task.depends_on || []).join(', ') || '—' },
        { label: 'Tools', value: (task.tools || []).join(', ') || '—' },
        { label: 'Status', value: toStatusChip(task.status || 'pending') },
      ])}
      ${task.duration != null ? `<div class="field"><div class="label">Duration</div><div class="value small">${task.duration}ms</div></div>` : ''}
      ${task.condition ? `<div class="field"><div class="label">Condition</div><div class="value block small">${task.condition}</div></div>` : ''}
      ${task.max_turns ? `<div class="field"><div class="label">Turns</div><div class="value small">${task.max_turns}</div></div>` : ''}
      ${task.max_retries ? `<div class="field"><div class="label">Retry policy</div><div class="value small">${task.max_retries} attempts</div></div>` : ''}
    `;
  }

  function renderToolNode(tool) {
    const args = tool.parameters || [];
    const argText = args.length ? args.map((arg) => `${arg.name}: ${arg.type || 'any'}${arg.required ? ' (required)' : ''}`).join(', ') : '—';
    inspector.innerHTML = `
      <h2>Tool</h2>
      ${renderNamedField('Name', tool.name, resolveNodeIcon('tool'), 'tool')}
      ${renderValueLines([
        { label: 'Owner', value: tool.owner || '-' },
        { label: 'Description', value: tool.description || '-' },
        { label: 'Parameters', value: argText },
        { label: 'Timeout', value: tool.timeout_ms ? `${tool.timeout_ms}ms` : '-' },
      ])}
    `;
  }

  function renderConversationNode(conversation) {
    inspector.innerHTML = `
      <h2>Conversation</h2>
      ${renderValueLines([
        { label: 'ID', value: conversation.id || '-' },
        { label: 'Agent', value: conversation.agent || '-' },
        { label: 'Stream', value: conversation.stream ? 'true' : 'false' },
        { label: 'Max history', value: conversation.max_history || '-' },
        { label: 'Description', value: conversation.description || '-' },
      ])}
    `;
  }

  function renderDialogNode(dialog) {
    inspector.innerHTML = `
      <h2>Dialog</h2>
      ${renderValueLines([
        { label: 'ID', value: dialog.id || '-' },
        { label: 'Agents', value: (dialog.agents || []).join(', ') || '-' },
        { label: 'Starter', value: dialog.starter || '-' },
        { label: 'Max turns', value: dialog.max_turns || '-' },
        { label: 'Starting speaker', value: dialog.starting_speaker || '-' },
        { label: 'Should-stop policy', value: dialog.should_stop || '-' },
      ])}
    `;
  }

  function renderMemoryNode(memory) {
    inspector.innerHTML = `
      <h2>Memory</h2>
      ${renderValueLines([
        { label: 'Key', value: memory.key || '-' },
        { label: 'Owner', value: memory.owner || '-' },
        { label: 'Action', value: memory.action || '-' },
        { label: 'TTL', value: memory.ttl_ms != null ? `${memory.ttl_ms}ms` : 'none' },
        { label: 'Value', value: memory.value || '-' },
      ])}
    `;
  }

  function renderMessageNode(message) {
    inspector.innerHTML = `
      <h2>Message</h2>
      ${renderValueLines([
        { label: 'ID', value: message.id || '-' },
        { label: 'From', value: message.from || '-' },
        { label: 'To', value: message.to || '-' },
        { label: 'Type', value: message.type || 'notification' },
        { label: 'Content', value: message.content || '-' },
      ])}
    `;
  }

  function renderFunctionsNode(functionsData) {
    const fnCount = Array.isArray(functionsData?.functions) ? functionsData.functions.length : 0;
    inspector.innerHTML = `
      <h2>Functions</h2>
      ${renderValueLines([
        { label: 'Namespace', value: 'Lua local helpers' },
        { label: 'Total', value: String(fnCount) },
      ])}
      <div class="field">
        <div class="label">Contained functions</div>
        <div class="value block small">${fnCount ? functionsData.functions.map((fn) => fn.name || 'unnamed').join(', ') : '—'}</div>
      </div>
    `;
  }

  function renderFunctionNode(fn) {
    inspector.innerHTML = `
      <h2>Code</h2>
      ${renderValueLines([
        { label: 'Name', value: fn.name || 'unnamed' },
        { label: 'Signature', value: fn.signature || 'local function (...) end' },
        { label: 'Location', value: fn.location || '-' },
        { label: 'Description', value: fn.description || '-' },
      ])}
      ${fn.source ? `<div class="field"><div class="label">Source</div><div class="value block small"><pre>${escapeHtml(fn.source)}</pre></div></div>` : ''}
    `;
  }

  function getNodeMetaFromCell(cell) {
    const data = cell.getData() || {};
    return {
      kind: data.kind,
      task: data.task,
      agent: data.agent,
      tool: data.tool,
      functions: data.functions,
      functionDef: data.function,
      memory: data.memory,
      conversation: data.conversation,
      dialog: data.dialog,
      message: data.message,
      crewData: crew,
    };
  }

  function setTaskStatus(node, status, duration) {
    const data = node.getData();
    data.status = status;
    if (duration != null) data.duration = duration;
    node.setData(data, { silent: true });

    node.attr('statusDot/fill', THEME.statusColors[status] || THEME.statusColors.pending);

    const ribbonColor = status === 'running'
      ? THEME.statusColors.running
      : status === 'success'
        ? THEME.statusColors.success
        : status === 'error'
          ? THEME.statusColors.error
          : THEME.crewAccent;
    node.attr('header/fill', ribbonColor);
    node.attr('body/stroke', status === 'running' ? THEME.node.bodyStrokeActive : THEME.node.bodyStroke);
    node.attr('body/strokeWidth', status === 'running' ? 2 : 1);

    node.attr(
      'statusLabel/text',
      status === 'success' && duration != null
        ? `${duration}ms`
        : status === 'running'
          ? 'running…'
          : status === 'error'
            ? 'failed'
            : '',
    );
    node.attr(
      'statusLabel/fill',
      status === 'success'
        ? THEME.statusColors.success
        : status === 'running'
          ? THEME.statusColors.running
          : THEME.node.subtitleFill,
    );
  }

  function edgeBaseStyle(edge) {
    switch (edge.shape) {
      case 'ironcrew-dep':
        return { stroke: THEME.edge.idle, strokeWidth: 3, strokeDasharray: 0, strokeDashoffset: 0 };
      case 'ironcrew-flow':
        return { stroke: THEME.edge.ownership, strokeWidth: 1.5, strokeDasharray: 0, strokeDashoffset: 0 };
      case 'ironcrew-own':
        return { stroke: THEME.edge.ownership, strokeWidth: 1, strokeDasharray: '4 4', strokeDashoffset: 0 };
      case 'ironcrew-inferred':
        return { stroke: THEME.statusColors.running, strokeWidth: 1, strokeDasharray: '6 4', strokeDashoffset: 0 };
      case 'ironcrew-tool':
        return { stroke: THEME.edge.tool, strokeWidth: 1, strokeDasharray: '5 4', strokeDashoffset: 0 };
      case 'ironcrew-memory':
        return { stroke: THEME.edge.memory, strokeWidth: 1, strokeDasharray: '4 4', strokeDashoffset: 0 };
      case 'ironcrew-message':
        return { stroke: THEME.edge.message, strokeWidth: 1, strokeDasharray: '4 4', strokeDashoffset: 0 };
      default:
        return { stroke: THEME.edge.idle, strokeWidth: 1.5, strokeDasharray: '4 4', strokeDashoffset: 0 };
    }
  }

  function resetHoverHighlight() {
    hoveredNodeId = null;

    graph.getNodes().forEach((node) => {
      const isToolsGroup = node.id === 'tools-group';
      node.attr('body/opacity', 1);
      node.attr('title/opacity', 1);
      node.attr('subtitle/opacity', 1);
      node.attr('iconUse/opacity', 1);
      node.attr('iconImage/opacity', 1);
      node.attr('header/opacity', isToolsGroup ? 0 : 1);

      (node.getPorts?.() || []).forEach((port) => {
        node.setPortProp(port.id, 'attrs/circle/opacity', 1);
      });
    });

    graph.getEdges().forEach((edge) => {
      edge.attr('line/opacity', 1);
      edge.attr('line/strokeWidth', edge.shape === 'ironcrew-flow' ? 2 : 1.5);
    });
  }

  function applyHoverHighlight(node) {
    hoveredNodeId = node.id;

    const activeNodeIds = new Set([node.id]);
    const activeEdgeIds = new Set();

    (graph.getConnectedEdges(node) || []).forEach((edge) => {
      activeEdgeIds.add(edge.id);
      const sourceId = edge.getSourceCellId?.();
      const targetId = edge.getTargetCellId?.();
      if (sourceId) activeNodeIds.add(sourceId);
      if (targetId) activeNodeIds.add(targetId);
    });

    graph.getNodes().forEach((cellNode) => {
      const isActive = activeNodeIds.has(cellNode.id);
      const isToolsGroup = cellNode.id === 'tools-group';
      cellNode.attr('body/opacity', isActive ? 1 : 0.24);
      cellNode.attr('title/opacity', isActive ? 1 : 0.4);
      cellNode.attr('subtitle/opacity', isActive ? 1 : 0.28);
      cellNode.attr('iconUse/opacity', isActive ? 1 : 0.28);
      cellNode.attr('iconImage/opacity', isActive ? 1 : 0.28);
      cellNode.attr('header/opacity', isToolsGroup ? 0 : isActive ? 1 : 0.24);

      (cellNode.getPorts?.() || []).forEach((port) => {
        cellNode.setPortProp(port.id, 'attrs/circle/opacity', isActive ? 1 : 0.16);
      });
    });

    graph.getEdges().forEach((edge) => {
      const isActive = activeEdgeIds.has(edge.id);
      edge.attr('line/opacity', isActive ? 1 : 0.1);
      edge.attr('line/strokeWidth', isActive ? 2.5 : 1);
    });
  }

  function stopEdgeAnimation(edge) {
    const animation = runningEdgeAnimations.get(edge.id);
    if (!animation) return;
    window.cancelAnimationFrame(animation.frameId);
    runningEdgeAnimations.delete(edge.id);
  }

  function resetEdgeStyle(edge) {
    const base = edgeBaseStyle(edge);
    edge.attr('line/stroke', base.stroke);
    edge.attr('line/strokeWidth', base.strokeWidth);
    edge.attr('line/strokeDasharray', base.strokeDasharray);
    edge.attr('line/strokeDashoffset', base.strokeDashoffset);
  }

  function startEdgeDashAnimation(edge, from = 0, to = -24, duration = 800) {
    stopEdgeAnimation(edge);
    edge.attr('line/strokeDashoffset', from);

    const range = to - from;
    const startedAt = performance.now();

    const tick = (now) => {
      if (!runningEdgeAnimations.has(edge.id)) return;

      const elapsed = (now - startedAt) % duration;
      const progress = elapsed / duration;
      edge.attr('line/strokeDashoffset', from + range * progress);

      const frameId = window.requestAnimationFrame(tick);
      runningEdgeAnimations.set(edge.id, { frameId });
    };

    const frameId = window.requestAnimationFrame(tick);
    runningEdgeAnimations.set(edge.id, { frameId });
  }

  function setTaskEdgesRunning(node, running) {
    graph.getIncomingEdges(node)?.forEach((edge) => {
      if (running) {
        edge.attr('line/stroke', THEME.edge.active);
        edge.attr('line/strokeDasharray', 8);
        edge.attr('line/strokeDashoffset', 0);
        startEdgeDashAnimation(edge);
      } else {
        stopEdgeAnimation(edge);
        edge.attr('line/stroke', THEME.edge.done);
        edge.attr('line/strokeDasharray', edge.shape === 'ironcrew-flow' ? 0 : 8);
        edge.attr('line/strokeDashoffset', 0);
      }
    });
  }

  function setAgentEdgesRunning(node, running) {
    graph.getIncomingEdges(node)?.forEach((edge) => {
      if (edge.shape !== 'ironcrew-flow') return;

      if (running) {
        edge.attr('line/stroke', THEME.edge.active);
        edge.attr('line/strokeDasharray', '10 6');
        edge.attr('line/strokeDashoffset', 0);
        startEdgeDashAnimation(edge);
      } else {
        stopEdgeAnimation(edge);
        edge.attr('line/stroke', THEME.edge.done);
        edge.attr('line/strokeDasharray', 0);
        edge.attr('line/strokeDashoffset', 0);
      }
    });
  }

  function setCrewStatus(status) {
    const crewCell = crewNodeDef?.cell;
    if (!crewCell) return;

    crewCell.attr('body/stroke', status === 'running' ? THEME.node.bodyStrokeActive : THEME.node.bodyStroke);
    crewCell.attr('body/strokeWidth', status === 'running' ? 2 : 1);
    crewCell.attr('header/fill', status === 'running' ? THEME.statusColors.running : THEME.crewAccent);
  }

  function setAgentStatus(node, status) {
    if (!node) return;

    const active = status === 'running';
    const ribbonColor = active ? THEME.statusColors.running : THEME.crewAccent;
    node.attr('header/fill', ribbonColor);
    node.attr('body/stroke', active ? THEME.node.bodyStrokeActive : THEME.node.bodyStroke);
    node.attr('body/strokeWidth', active ? 2 : 1);
  }

  function resetAll() {
    for (const nodeDef of nodeDefs) {
      if (nodeDef.kind === 'task') {
        setTaskStatus(nodeDef.cell, 'pending', null);
        continue;
      }

      if (nodeDef.kind === 'agent') {
        setAgentStatus(nodeDef.cell, 'pending');
      }
    }

    setCrewStatus('pending');

    if (resultNodeDef?.cell) {
      resultNodeDef.cell.attr('header/fill', THEME.crewAccent);
      resultNodeDef.cell.attr('body/stroke', THEME.node.bodyStroke);
      resultNodeDef.cell.attr('body/strokeWidth', 1);
      resultNodeDef.cell.attr('statusDot/fill', THEME.statusColors.pending);
      resultNodeDef.cell.attr('statusLabel/text', '');
      resultNodeDef.cell.attr('statusLabel/fill', THEME.node.subtitleFill);
      resultNodeDef.cell.attr('subtitle/text', `${crew.tasks.length} tasks • awaiting run`);
    }

    // Clear selection on reset.
    clearSelection();

    graph.getEdges().forEach((edge) => {
      stopEdgeAnimation(edge);
      resetEdgeStyle(edge);
    });
  }

  async function simulate() {
    resetAll();
    const btnPlay = document.getElementById('btn-play');
    if (btnPlay) btnPlay.disabled = true;

    if (crewNodeDef?.cell) {
      setCrewStatus('running');
      await new Promise((r) => setTimeout(r, 350));
      setCrewStatus('success');
    }

    for (const task of taskExecutionOrder) {
      const agentId = resolveAgentId(taskOwnerName(task));
      const agentNode = agentId ? nodeDefById.get(agentId)?.cell : null;
      if (agentNode) {
        setAgentStatus(agentNode, 'running');
        setAgentEdgesRunning(agentNode, true);
        await new Promise((r) => setTimeout(r, 220));
      }

      const node = nodeDefById.get(taskNodeIdStable(task))?.cell;
      if (!node) {
        if (agentNode) {
          setAgentStatus(agentNode, 'pending');
          setAgentEdgesRunning(agentNode, false);
        }
        continue;
      }

      setTaskStatus(node, 'running', null);
      setTaskEdgesRunning(node, true);

      const duration = 600 + Math.floor(Math.random() * 800);
      await new Promise((r) => setTimeout(r, duration));

      setTaskStatus(node, 'success', duration);
      setTaskEdgesRunning(node, false);

      if (agentNode) {
        setAgentStatus(agentNode, 'pending');
        setAgentEdgesRunning(agentNode, false);
      }
    }

    // Compute run summary from completed tasks.
    let totalDuration = 0;
    let successCount = 0;
    for (const def of nodeDefs) {
      if (def.kind !== 'task') continue;
      const data = def.cell?.getData();
      if (data?.status === 'success') {
        successCount++;
        totalDuration += data.duration || 0;
      }
    }

    if (resultNodeDef?.cell) {
      setTaskEdgesRunning(resultNodeDef.cell, true);
      await new Promise((r) => setTimeout(r, 400));
      setTaskEdgesRunning(resultNodeDef.cell, false);

      resultNodeDef.cell.attr('header/fill', THEME.statusColors.success);
      resultNodeDef.cell.attr('body/stroke', THEME.statusColors.success);
      resultNodeDef.cell.attr('body/strokeWidth', 2);
      resultNodeDef.cell.attr('statusDot/fill', THEME.statusColors.success);
      resultNodeDef.cell.attr('statusLabel/text', `${totalDuration}ms`);
      resultNodeDef.cell.attr('statusLabel/fill', THEME.statusColors.success);
      resultNodeDef.cell.attr('subtitle/text', `${successCount}/${crew.tasks.length} passed • ${totalDuration}ms total`);
    }

    if (btnPlay) btnPlay.disabled = false;
  }

  function mount() {
    if (!inspector) return;

    graph.on('node:click', ({ node }) => {
      selectNode(node);
      const meta = getNodeMetaFromCell(node);
      if (meta.kind === 'crew') return renderCrewNode(meta.crewData);
      if (meta.kind === 'agent') return renderAgentNode(meta.agent);
      if (meta.kind === 'task') return renderTaskNode(meta.task);
      if (meta.kind === 'tool') return renderToolNode(meta.tool);
      if (meta.kind === 'functions') return renderFunctionsNode({ functions: meta.functions });
      if (meta.kind === 'function') return renderFunctionNode(meta.functionDef);
      if (meta.kind === 'memory') return renderMemoryNode(meta.memory);
      if (meta.kind === 'conversation') return renderConversationNode(meta.conversation);
      if (meta.kind === 'dialog') return renderDialogNode(meta.dialog);
      if (meta.kind === 'message') return renderMessageNode(meta.message);
    });

    // Click on blank canvas → deselect.
    graph.on('blank:click', () => {
      clearSelection();
      inspector.innerHTML = '<h2>Inspector</h2><div class="empty">Click a node to see its details</div>';
    });

    graph.on('node:mouseenter', ({ node }) => {
      applyHoverHighlight(node);
    });

    graph.on('node:mouseleave', ({ node }) => {
      if (hoveredNodeId !== node.id) return;
      resetHoverHighlight();
    });

    const btnPlay = document.getElementById('btn-play');
    const btnReset = document.getElementById('btn-reset');
    if (btnPlay) btnPlay.addEventListener('click', simulate);
    if (btnReset) btnReset.addEventListener('click', resetAll);

    resetAll();
    resetHoverHighlight();
  }

  return {
    mount,
    simulate,
    resetAll,
  };
}
