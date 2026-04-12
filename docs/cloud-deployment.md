# Cloud Deployment

How to run IronCrew in managed cloud environments: **Kubernetes**, **OpenShift**, **Railway**, and similar platforms. This doc covers graceful shutdown, resource limits, security posture, and platform-specific recipes.

IronCrew is a single statically-linked Rust binary. It runs in `serve` mode as a long-lived HTTP server, or in `run` mode as a one-shot job.

---

## Binary profile

- Single static binary (no shared libraries to bundle).
- Release build strips symbols and enables LTO — typical size is 15–25 MB.
- Base image for containers: `gcr.io/distroless/cc-debian12` (or `scratch` if you link musl).
- No systemd, no daemonization — runs in the foreground; logs to stderr.

---

## Graceful shutdown

IronCrew handles `SIGTERM` (Kubernetes pod termination) and `SIGINT` (Ctrl+C) cleanly:

1. Signal received → stops accepting new HTTP requests.
2. In-flight requests run to completion (via Axum's `with_graceful_shutdown`).
3. Per-request `LuaCrew` instances drop → MCP managers call `shutdown_blocking()` which spawns async cleanup tasks for each stdio child / HTTP connection.
4. A drain window lets those background tasks finish reaping stdio child processes before the Tokio runtime tears down.

### The drain window

| Variable | Default | Description |
|---|---|---|
| `IRONCREW_SHUTDOWN_DRAIN_MS` | `1000` | Milliseconds to wait after Axum returns, so Drop-spawned shutdown tasks can complete. Set to `0` to skip (children will be killed when the runtime drops). |

**Tune this value** to fit your platform's grace period:

- **Kubernetes `terminationGracePeriodSeconds: 30`** (default) → leave `IRONCREW_SHUTDOWN_DRAIN_MS=1000`. Plenty of headroom.
- **Tight grace periods (≤ 10 s)** → `IRONCREW_SHUTDOWN_DRAIN_MS=500`.
- **Heavy MCP stdio usage** (many long-lived child processes per request) → bump to `2000–3000` to ensure every `uvx` / `npx` child exits cleanly.

### Pod termination sequence (Kubernetes)

```
    kubelet              ironcrew pod
       │                      │
       │─── SIGTERM ─────────►│
       │                      │── stop accepting new requests
       │                      │── drain in-flight (up to max_run_lifetime)
       │                      │── drop LuaCrews, shutdown MCP clients
       │                      │── IRONCREW_SHUTDOWN_DRAIN_MS wait
       │                      │── exit 0
       │◄── container exit ───│
       │
       │ (if still running after terminationGracePeriodSeconds)
       │─── SIGKILL ─────────►│
```

Ensure `terminationGracePeriodSeconds ≥ IRONCREW_MAX_RUN_LIFETIME + IRONCREW_SHUTDOWN_DRAIN_MS/1000 + 5s margin`.

---

## Security posture

Production deployments should set these at minimum:

| Variable | Recommended | Why |
|---|---|---|
| `IRONCREW_API_TOKEN` | strong random string (32+ bytes) | Protects `/flows/*` endpoints. `/health` stays public. |
| `IRONCREW_CORS_ORIGINS` | explicit domain list | Default is **deny-all**. Set `https://app.example.com` (comma-separated for multiple). Avoid `*`. |
| `IRONCREW_ALLOW_SHELL` | **unset** | Leaving shell disabled prevents agents from running arbitrary commands. Only enable in sandboxed workloads. |
| `IRONCREW_ALLOW_PRIVATE_IPS` | **unset** | Keep SSRF protection on. Default blocks RFC1918, loopback, link-local in `http_request` and Lua `http.*`. |
| `IRONCREW_MCP_ALLOWED_COMMANDS` | comma-separated allowlist | If using MCP stdio, whitelist only the binaries you trust (e.g. `uvx,npx`). Unset = allow all. |
| `IRONCREW_MCP_ALLOW_LOCALHOST` | **unset** | Only enable if MCP servers run as sidecars. |
| `IRONCREW_MAX_BODY_SIZE` | `10485760` (10 MB) or lower | Caps request body size against memory-exhaustion DoS. |
| `IRONCREW_MAX_RESPONSE_SIZE` | `52428800` (50 MB) | Caps `http_request` tool responses. |
| `IRONCREW_ENV_BLOCKLIST` | comma-separated secrets | Augments the built-in blocklist so Lua `env()` cannot read them. |

### Secrets handling

- **Never** bake API keys into the container image.
- Mount them as environment variables via `Secret` (Kubernetes), `Environment Variables` (Railway), or equivalent.
- IronCrew's Lua `env()` already blocks `*_API_KEY`, `*_SECRET`, `*_TOKEN`, `IRONCREW_API_TOKEN`, and others by default.
- MCP stdio children do **not** inherit the parent environment by default — only `PATH`, `HOME`, `USER`, `LANG`, `LC_*`. Secrets are therefore isolated from spawned MCP servers unless you explicitly list them in `env = {...}` or set `inherit_env = true`.

---

## Resource limits (RAM/CPU)

### Tune these to your pod limits

| Variable | Default | Purpose |
|---|---|---|
| `IRONCREW_MAX_PROMPT_CHARS` | `100000` | Caps prompt size per task. |
| `IRONCREW_MAX_BODY_SIZE` | `10485760` (10 MB) | Request body cap. |
| `IRONCREW_MAX_RESPONSE_SIZE` | `52428800` (50 MB) | HTTP tool response cap. |
| `IRONCREW_WEB_SCRAPE_MAX_BYTES` | `10485760` | Cap on `web_scrape` HTML download. |
| `IRONCREW_FILE_READ_MAX_BYTES` | `10485760` | Cap on single `file_read` result. |
| `IRONCREW_GLOB_MAX_FILES` | `500` | Per-call limit for `file_read_glob`. |
| `IRONCREW_GLOB_MAX_BYTES` | `52428800` | Aggregate limit for `file_read_glob`. |
| `IRONCREW_SHELL_MAX_OUTPUT_BYTES` | `1048576` | Shell stdout/stderr cap. |
| `IRONCREW_MCP_TOOL_RESULT_MAX_BYTES` | `262144` | Cap on each MCP tool result. |
| `IRONCREW_DEFAULT_MAX_CONCURRENT` | `10` | Default semaphore for `foreach_parallel`. |
| `IRONCREW_MAX_EVENTS` | `1000` | SSE replay buffer size per run. |
| `IRONCREW_EVENT_REPLAY_MAX_BYTES` | `4194304` (4 MB) | SSE replay byte budget per run. |
| `IRONCREW_MESSAGEBUS_QUEUE_DEPTH` | `1000` | Max pending messages per agent. |
| `IRONCREW_MAX_RUN_LIFETIME` | `1800` (30 min) | Hard per-run timeout. Lower for short flows. |
| `IRONCREW_CONVERSATION_MAX_HISTORY` | `50` | Trim conversation history at this many turns. |
| `IRONCREW_DIALOG_MAX_HISTORY` | `100` | Trim dialog transcript at this many turns. |

### Recommended baselines

**Small pod (256 MB / 0.25 CPU):**
```bash
IRONCREW_MAX_PROMPT_CHARS=30000
IRONCREW_MAX_BODY_SIZE=2097152      # 2 MB
IRONCREW_MAX_RESPONSE_SIZE=10485760 # 10 MB
IRONCREW_DEFAULT_MAX_CONCURRENT=3
IRONCREW_MAX_EVENTS=200
```

**Medium pod (1 GB / 1 CPU):** defaults are fine.

**Large pod (4 GB / 4 CPU):**
```bash
IRONCREW_DEFAULT_MAX_CONCURRENT=40
IRONCREW_MAX_EVENTS=5000
```

### Rate limiting

- `IRONCREW_RATE_LIMIT_MS` — per-provider minimum interval between LLM calls (milliseconds). Use to stay within provider-side quotas.
- Combine with `IRONCREW_DEFAULT_MAX_CONCURRENT` to cap parallelism globally.

---

## Persistence

IronCrew can store run records in three backends. **Choose based on your platform's volume story**.

| Backend | Best for | Env |
|---|---|---|
| JSON files | single-pod deployments with mounted PVC | `IRONCREW_STORE=json` (default) |
| SQLite | single-pod or small team, self-contained | `IRONCREW_STORE=sqlite`, `IRONCREW_STORE_PATH=/data/ironcrew.db` |
| PostgreSQL | multi-pod, horizontally scaled | `IRONCREW_STORE=postgres`, `DATABASE_URL=postgres://...` |

**Kubernetes:** use PostgreSQL if you have `replicas > 1`. JSON/SQLite require a `ReadWriteOnce` PVC, which prevents horizontal scaling.

**Railway:** the built-in PostgreSQL add-on is the simplest path. Add it and Railway sets `DATABASE_URL` automatically.

### Postgres-specific

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | — | Standard Postgres DSN. Required. |
| `IRONCREW_PG_TABLE_PREFIX` | empty | Prefix for shared databases (e.g. `tenant1_`). Alphanumeric + underscore only. |
| `IRONCREW_DB_POOL_SIZE` | `10` | Connection pool size. Raise for concurrent load. |

---

## Observability

### Logs

- Tracing output → stderr. Logs do **not** mix with `run`-mode stdout.
- `IRONCREW_LOG` controls log level. Format: `env_logger` / `tracing` directive.

```bash
IRONCREW_LOG=info              # production default
IRONCREW_LOG=ironcrew=debug    # debug ironcrew-only
IRONCREW_LOG=debug,hyper=info  # broad debug, suppress hyper
```

### Health check

`GET /health` returns `200 OK` with `{"status": "ok", "version": "…"}`. Public, no auth. Use for:
- Kubernetes `livenessProbe` and `readinessProbe`
- Railway automatic health checks
- Load balancer target group health checks

### Metrics

Not built-in today. Structured tracing output can be scraped via Loki/Promtail or similar.

---

## Kubernetes recipe

### Minimal Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ironcrew
spec:
  replicas: 2
  selector:
    matchLabels: { app: ironcrew }
  template:
    metadata:
      labels: { app: ironcrew }
    spec:
      terminationGracePeriodSeconds: 60
      containers:
      - name: ironcrew
        image: your-registry/ironcrew:2.12.0
        args: ["serve", "--host", "0.0.0.0", "--port", "8080", "--flows-dir", "/flows"]
        ports:
        - containerPort: 8080
        env:
        - name: IRONCREW_LOG
          value: "info"
        - name: IRONCREW_STORE
          value: "postgres"
        - name: DATABASE_URL
          valueFrom: { secretKeyRef: { name: ironcrew-secrets, key: database-url } }
        - name: OPENAI_API_KEY
          valueFrom: { secretKeyRef: { name: ironcrew-secrets, key: openai-api-key } }
        - name: IRONCREW_API_TOKEN
          valueFrom: { secretKeyRef: { name: ironcrew-secrets, key: api-token } }
        - name: IRONCREW_CORS_ORIGINS
          value: "https://app.example.com"
        - name: IRONCREW_MAX_RUN_LIFETIME
          value: "300"
        - name: IRONCREW_SHUTDOWN_DRAIN_MS
          value: "1500"
        - name: IRONCREW_MCP_ALLOWED_COMMANDS
          value: "uvx,npx"
        resources:
          requests: { cpu: "200m", memory: "256Mi" }
          limits:   { cpu: "2",    memory: "1Gi"  }
        readinessProbe:
          httpGet: { path: /health, port: 8080 }
          periodSeconds: 5
        livenessProbe:
          httpGet: { path: /health, port: 8080 }
          periodSeconds: 15
        volumeMounts:
        - name: flows
          mountPath: /flows
          readOnly: true
      volumes:
      - name: flows
        configMap: { name: ironcrew-flows }
---
apiVersion: v1
kind: Service
metadata: { name: ironcrew }
spec:
  selector: { app: ironcrew }
  ports: [{ port: 80, targetPort: 8080 }]
```

### Flows as ConfigMap

For small flow sets, mount `crew.lua` / `config.lua` via ConfigMap. For larger sets, bake them into a separate image layer or pull from object storage at startup.

### HPA

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata: { name: ironcrew }
spec:
  scaleTargetRef: { apiVersion: apps/v1, kind: Deployment, name: ironcrew }
  minReplicas: 2
  maxReplicas: 10
  metrics:
  - type: Resource
    resource: { name: cpu, target: { type: Utilization, averageUtilization: 70 } }
```

Note: each replica holds its own `active_runs` map. SSE subscribers must stick to the replica running their flow. Configure session affinity on the Service (`sessionAffinity: ClientIP`) or use the `/runs/{id}` polling API instead of SSE.

---

## OpenShift specifics

OpenShift adds a few constraints on top of upstream Kubernetes:

### Restricted SCC and non-root

OpenShift's default `restricted-v2` SCC forces containers to run as a non-root UID assigned per-namespace. Ensure your container image:

- Does **not** `USER root` or rely on root-owned paths.
- Writes only to `/tmp` or mounted `emptyDir`/PVC volumes.
- Does not bind to ports < 1024.

The default port `8080` is fine. IronCrew does not require root at runtime.

Example distroless-based Dockerfile:

```dockerfile
FROM rust:1.86 AS build
WORKDIR /src
COPY . .
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/ironcrew /usr/local/bin/ironcrew
USER 1000
ENTRYPOINT ["/usr/local/bin/ironcrew"]
```

### Routes instead of Ingress

OpenShift uses `Route` objects for external traffic. Create one pointing at the `ironcrew` Service; TLS is terminated at the router.

```yaml
apiVersion: route.openshift.io/v1
kind: Route
metadata: { name: ironcrew }
spec:
  to: { kind: Service, name: ironcrew }
  port: { targetPort: 8080 }
  tls: { termination: edge }
```

### Secrets

OpenShift `Secret` objects work the same as Kubernetes ones. For stricter environments, use **SealedSecrets** or the platform's vault integration.

---

## Railway recipe

Railway has no Kubernetes manifests — everything is a service, an environment variable, or a config file in the repo.

### 1. Create a service from the GitHub repo

- **Build command:** `cargo build --release`
- **Start command:** `./target/release/ironcrew serve --host 0.0.0.0 --port $PORT --flows-dir ./flows`
- **Root directory:** repo root

Or use a Dockerfile-based build (recommended — faster cold starts).

### 2. Environment variables

In the Railway service settings, add:

```
IRONCREW_LOG=info
IRONCREW_API_TOKEN=<generated token>
IRONCREW_CORS_ORIGINS=https://your-frontend.example.com
IRONCREW_MAX_RUN_LIFETIME=300
IRONCREW_SHUTDOWN_DRAIN_MS=1500
IRONCREW_STORE=postgres       # if you added Postgres
OPENAI_API_KEY=sk-...
```

Railway's Postgres add-on auto-injects `DATABASE_URL`.

### 3. Health check

Railway auto-detects `/health` if you set **Health Check Path** to `/health` in the service settings. Unhealthy services are not routed traffic.

### 4. SIGTERM grace

Railway sends `SIGTERM` on deploys and scales. Its grace period is **10 seconds** — keep:

```
IRONCREW_MAX_RUN_LIFETIME=60        # fast runs
IRONCREW_SHUTDOWN_DRAIN_MS=500
```

Longer-running crews should run via Railway **Cron** jobs (one-shot `ironcrew run`) rather than the `serve` service.

### 5. Volumes

Railway volumes persist between deploys but do **not** survive region migrations. For production, use the Postgres backend instead of JSON/SQLite.

---

## Building container images

### Dockerfile (multi-stage, distroless)

```dockerfile
FROM rust:1.86 AS build
WORKDIR /src
# Cache dependencies
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/ironcrew /usr/local/bin/ironcrew
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/ironcrew"]
CMD ["serve", "--host", "0.0.0.0", "--port", "8080", "--flows-dir", "/flows"]
```

Image size ≈ 40 MB.

### Alternative: musl static

```dockerfile
FROM rust:1.86-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/ironcrew /ironcrew
EXPOSE 8080
ENTRYPOINT ["/ironcrew"]
```

Image size ≈ 20 MB, but you lose glibc-only TLS backends. Use rustls (default in IronCrew's `reqwest` stack).

### Excluding MCP

If you don't need MCP support, build with `--no-default-features`. This drops `rmcp` and shrinks the binary by ~2 MB:

```
RUN cargo build --release --no-default-features --features postgres
```

---

## Troubleshooting

### Pod killed by OOM
- Lower `IRONCREW_DEFAULT_MAX_CONCURRENT` and `IRONCREW_MAX_EVENTS`.
- Check for large SSE event histories (SSE replay buffer is per-run).
- Reduce `IRONCREW_MAX_PROMPT_CHARS` and per-tool byte caps.

### MCP stdio children orphaned after SIGKILL
- `terminationGracePeriodSeconds` was too short. Raise it so IronCrew's drain window (`IRONCREW_SHUTDOWN_DRAIN_MS`) fits comfortably.

### `/health` passes but real traffic 500s
- Health check is intentionally lightweight and does not probe LLM providers or the database. Add application-level monitoring for those.

### CORS blocks legitimate frontend
- Set `IRONCREW_CORS_ORIGINS` explicitly — default is deny-all. Never use `*` in production.

### Run records lost between deploys
- Using JSON or SQLite backend with an `emptyDir` volume? Switch to a persistent volume or PostgreSQL.

---

## Checklist before go-live

- [ ] `IRONCREW_API_TOKEN` set to a strong value
- [ ] `IRONCREW_CORS_ORIGINS` restricted to your frontend domains
- [ ] `IRONCREW_ALLOW_SHELL` unset (unless sandboxed)
- [ ] `IRONCREW_MCP_ALLOWED_COMMANDS` whitelist set (if using MCP stdio)
- [ ] `IRONCREW_MAX_RUN_LIFETIME` tuned to workload (shorter than `terminationGracePeriodSeconds`)
- [ ] `IRONCREW_SHUTDOWN_DRAIN_MS` fits within grace period
- [ ] PostgreSQL configured for multi-replica deployments
- [ ] Secrets mounted from `Secret` / vault, not baked into image
- [ ] Readiness + liveness probes hitting `/health`
- [ ] Resource `requests` and `limits` set on the container
- [ ] TLS terminated at ingress / router / load balancer
- [ ] Log level set to `info` or lower (never `debug` in prod)
