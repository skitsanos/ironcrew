/*
 * IronCrew DAG prototype — entry point.
 */

import { createPrototypeGraphModel } from './graph-builder.js';
import { createPrototypeRuntime } from './graph-runtime.js';
import { CREW } from './data.js';
import { buildLegendGroups, iconToMarkup } from './icon-registry.js';

const graphContainer = document.getElementById('graph-container');
const inspector = document.getElementById('inspector');
const legend = document.getElementById('legend');
const legendPanel = document.getElementById('legend-panel');
const legendToggle = document.getElementById('legend-toggle');

function escapeHtml(value) {
  return String(value == null ? '' : value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#039;');
}

function renderLegend(target, crew) {
  if (!target) return;
  const sections = buildLegendGroups((crew?.agents || []).map((agent) => agent?.name));
  target.innerHTML = sections
    .map(
      (section) => `
      <div class="section">
        <h3>${section.title}</h3>
        ${section.entries
          .map(
            (entry) => `
              <div class="row">
                ${iconToMarkup(entry.icon, { className: 'legend-icon', title: entry.label })}
                <span>${escapeHtml(entry.label)}</span>
              </div>`,
          )
          .join('')}
      </div>`,
    )
    .join('');
}

renderLegend(legend, CREW);

if (legendPanel && legendToggle) {
  legendToggle.addEventListener('click', () => {
    const collapsed = legendPanel.classList.toggle('is-collapsed');
    legendToggle.setAttribute('aria-expanded', String(!collapsed));
  });
}

const graph = createPrototypeGraphModel(CREW, graphContainer);

const runtime = createPrototypeRuntime({
  graph: graph.graph,
  nodeDefById: graph.nodeDefById,
  nodeDefs: graph.nodeDefs,
  crew: graph.crew,
  crewNodeDef: graph.crewNodeDef,
  resultNodeDef: graph.resultNodeDef,
  inspector,
  taskExecutionOrder: graph.taskExecutionOrder,
  taskNodeIdStable: graph.taskNodeIdStable,
  taskOwnerName: graph.taskOwnerName,
  resolveAgentId: graph.resolveAgentId,
});

runtime.mount();
