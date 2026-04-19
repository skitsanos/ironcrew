# chat-ui

Bun + React showcase for the `examples/chat-http` IronCrew flow.

This demo is a **static SPA** — the Bun server only ships the HTML/JS
bundle and exposes one tiny config endpoint. All chat API calls go from
the browser **directly to IronCrew**, which keeps SSE streaming clean
(no proxy buffering) and removes an entire layer of plumbing.

## What it demonstrates

- explicit conversation start, resume, and delete
- sending messages to `chat-http`
- reading stored history
- listing stored sessions for the flow
- subscribing to `/events` via native `EventSource`

## Prerequisites

1. Run IronCrew with the `chat-http` flow available.
2. Allow the UI origin via CORS.
3. For this demo, run IronCrew **without** `IRONCREW_API_TOKEN`. The
   browser's `EventSource` can't attach `Authorization` headers, so
   enabling bearer auth breaks SSE. A fetch-based SSE client would lift
   that restriction (follow-up).

Example (with default UI port `5173`):

```bash
export OPENAI_API_KEY=sk-...
# Unset auth for the demo.
unset IRONCREW_API_TOKEN
export IRONCREW_CORS_ORIGINS=http://localhost:5173

ironcrew serve --flows-dir examples --host 127.0.0.1 --port 3000
```

## Bun app configuration

The Bun server reads:

| Variable | Default | Purpose |
|---|---|---|
| `IRONCREW_BASE_URL` | `http://127.0.0.1:3000` | Base URL of the IronCrew server |
| `IRONCREW_FLOW` | `chat-http` | Flow name to target |
| `IRONCREW_AGENT` | `concierge` | Default agent shown in the UI |
| `PORT` | `5173` | UI server port. Must differ from IronCrew's port. |

## Install

```bash
bun install
```

## Run in development

```bash
IRONCREW_BASE_URL=http://127.0.0.1:3000 \
IRONCREW_FLOW=chat-http \
IRONCREW_AGENT=concierge \
bun dev
```

Then open `http://localhost:5173`.

## Run in production mode

```bash
bun run build

IRONCREW_BASE_URL=http://127.0.0.1:3000 \
IRONCREW_FLOW=chat-http \
IRONCREW_AGENT=concierge \
bun start
```

## Endpoints the browser hits on IronCrew

| Method | Path |
|---|---|
| `GET` | `/flows/{flow}/conversations?limit=N` |
| `POST` | `/flows/{flow}/conversations/{id}/start` |
| `GET` | `/flows/{flow}/conversations/{id}/history` |
| `POST` | `/flows/{flow}/conversations/{id}/messages` |
| `GET` | `/flows/{flow}/conversations/{id}/events` (SSE) |
| `DELETE` | `/flows/{flow}/conversations/{id}` |

The Bun server itself only serves:

| Method | Path |
|---|---|
| `GET` | `/api/config` (returns `{ ironCrewBaseUrl, flow, defaultAgent }`) |
| `GET` | `/*` (SPA) |
