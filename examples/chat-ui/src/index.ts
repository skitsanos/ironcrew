import { serve } from "bun";
import index from "./index.html";

// The Bun server now does only two things:
//   1. Serve the SPA (`/*` → index.html via Bun's bundler).
//   2. Expose a tiny `/api/config` that tells the browser where to find
//      IronCrew and which flow to drive. No proxying — the browser talks
//      to IronCrew directly, which avoids Bun fetch() buffering SSE
//      chunks and the associated ERR_INCOMPLETE_CHUNKED_ENCODING.
//
// Prerequisites:
//   * Run IronCrew with `IRONCREW_CORS_ORIGINS=http://localhost:<UI_PORT>`.
//   * Do NOT set `IRONCREW_API_TOKEN` in dev — browser EventSource
//     cannot attach `Authorization` headers. Reintroducing auth is a
//     follow-up (fetch-based SSE, cookies, or a signed query token).

const uiPort = Number(process.env.PORT ?? 5173);

const runtime = {
  ironCrewBaseUrl: process.env.IRONCREW_BASE_URL ?? "http://127.0.0.1:3000",
  ironCrewFlow: process.env.IRONCREW_FLOW ?? "chat-http",
  ironCrewAgent: process.env.IRONCREW_AGENT ?? "concierge",
};

// Guard against a misconfiguration where IRONCREW_BASE_URL points at
// the UI's own port — that would make the browser talk to the UI
// instead of IronCrew and every request would return the SPA.
(() => {
  try {
    const upstream = new URL(runtime.ironCrewBaseUrl);
    const upstreamPort =
      upstream.port || (upstream.protocol === "https:" ? "443" : "80");
    const loopbacks = new Set(["127.0.0.1", "localhost", "0.0.0.0", "[::1]"]);
    if (
      loopbacks.has(upstream.hostname) &&
      Number(upstreamPort) === uiPort
    ) {
      console.error(
        `[chat-ui] IRONCREW_BASE_URL (${runtime.ironCrewBaseUrl}) equals this UI server's port ${uiPort}.`,
      );
      console.error(
        "[chat-ui] Start IronCrew on a different port or set PORT to a free one.",
      );
      process.exit(1);
    }
  } catch {
    /* malformed URL — the browser will surface the error */
  }
})();

function jsonConfig() {
  return Response.json(
    {
      ironCrewBaseUrl: runtime.ironCrewBaseUrl,
      flow: runtime.ironCrewFlow,
      defaultAgent: runtime.ironCrewAgent,
    },
    {
      // Same-origin request, but the fetch happens before anything else
      // during mount so we disable caching to keep hot reload consistent.
      headers: { "Cache-Control": "no-store" },
    },
  );
}

const server = serve({
  port: uiPort,
  routes: {
    "/api/config": jsonConfig,
    "/*": index,
  },
  development: process.env.NODE_ENV !== "production" && {
    hmr: true,
    console: true,
  },
});

console.log(`Chat UI running at ${server.url}`);
console.log(
  `Target IronCrew: ${runtime.ironCrewBaseUrl} flow=${runtime.ironCrewFlow} agent=${runtime.ironCrewAgent}`,
);
console.log(
  "Remember to start IronCrew with `IRONCREW_CORS_ORIGINS=" +
    server.url.origin +
    "`",
);
