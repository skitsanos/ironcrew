/*
 * Theme tokens — colors, fonts, and structural style values.
 *
 * Centralized so a future tier can load them from env vars or a JSON
 * config and re-apply without touching the markup or the graph logic.
 * Keep this file free of DOM / X6 references so it can be consumed by
 * any renderer.
 */

export const THEME = {
  fontSans: '"IBM Plex Sans", -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',

  crewAccent: '#64748b',

  statusColors: {
    pending: '#64748b',
    running: '#3b82f6',
    success: '#10b981',
    error:   '#ef4444',
  },

  node: {
    bodyFill:         '#1e293b',
    bodyStroke:       '#334155',
    bodyStrokeActive: '#3b82f6',
    titleFill:        '#e2e8f0',
    subtitleFill:     '#94a3b8',
    iconStroke:       '#ffffff',
  },

  edge: {
    idle:      '#475569',
    done:      '#10b981',
    active:    '#3b82f6',
    ownership: '#2a3448',
    tool:      '#f59e0b',
    memory:    '#14b8a6',
    message:   '#ef4444',
  },
};
