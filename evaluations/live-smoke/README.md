# Current-build provider compatibility smoke (IC-048)

A small compatibility canary, not a quality benchmark: native OpenAI Responses,
`gpt-5.6-luna`, one `none` effort request and otherwise `low`. It supplements the
deterministic blank-output/capability regressions and the separate
[crew-effectiveness evaluator](../crew-effectiveness/README.md). It does not
rewrite historical evaluation receipts or claim crew superiority.

## Inspect, validate, run

From the repository root, inspect the plan **without a credential or network**:

```bash
python3 -B evaluations/live-smoke/smoke.py
```

Build the current CLI and run all offline tests, including real CLI/PTY/HTTP
processes against the synthetic loopback provider:

```bash
cargo build --locked --bin ironcrew
IRONCREW_SMOKE_TEST_BIN=target/debug/ironcrew \
  python3 -B -m unittest discover -s evaluations/live-smoke -p 'test_*.py'
python3 -B evaluations/live-smoke/smoke.py --mode contract \
  --report /tmp/ironcrew-contract-receipt.json
```

Contract mode uses a dummy key, drops operator secrets and points only at its
own mock server. The fixture requires an explicitly allowlisted origin, so an
absent origin cannot fall back to a public endpoint. Token budgeting is disabled
in mock mode because production budgeting rejects proxy origins. Contract mode
tests the harness, not the native counting endpoint or live model behavior.

After approving a local run, reuse the existing `OPENAI_API_KEY` from the process
environment **or** the ignored root `.env` (duplicates are rejected):

```bash
IRONCREW_LIVE_SMOKE=1 python3 -B evaluations/live-smoke/smoke.py --mode live \
  --binary target/debug/ironcrew --max-cost-usd 0.15 \
  --report /tmp/ironcrew-live-receipt.json
```

Reports must be new files outside the checkout; existing receipts are never
overwritten. Without `IRONCREW_LIVE_SMOKE=1`, live mode reports `skipped/disabled`.
A missing key reports `skipped/credential_unconfigured`, not a successful test.
Malformed credentials/configuration and expired pricing review fail closed.
Only the native `https://api.openai.com` origin is supported; the conventional
`https://api.openai.com/v1` base spelling is normalized to that same origin.
The real credential is never allowlisted into Lua. Children run outside the
checkout and outside any ancestor `.env`; the harness imports the existing
secret-minimal credential and provenance helpers.

## Fixed envelope and assertions

| Scenario | Assertions | Maximum generation calls |
| --- | --- | ---: |
| CLI basic, `none` | Nonblank successful task, complete checked usage | 1 |
| CLI streaming dialog, `low` | Two named speakers, two nonblank turns, explicit `smoke_turn_cap` stop | 2 |
| CLI terminal HITL, `low` | Two agents ask; writer allowed, reviewer denied; actual artifacts and truthful structured outcomes | 22 |
| HTTP keyed flow HITL, `low` | Same two-agent interaction through questions/answer routes and terminal run inspection | 22 |

The task/tool-round structure bounds generation calls to **47**, with at most
47 corresponding input-count preflights. HITL tasks have zero retries and the
runtime's current ten-tool-round limit (at most eleven generation calls per
task); a regression test binds the plan to that limit. Successful normal runs
typically need only 15 generation requests. Polling is bounded and respects
local observation throttling; provider generation requests are never retried by
the harness. No arbitrary model, flow, origin or concurrency override is exposed.

Runs are sequential, with **10,000 input-plus-output tokens per run**, at most
**40,000 total**, and **1,024 output tokens per request**. IC-047's exact input
count and atomic reservation enforce the live run ceiling before dispatch.
The total work deadline is **300 seconds**, plus bounded process teardown;
request/connect/human timeouts, body/stream/log caps and disposable file-write
limits apply independently. The first failure stops later scenarios. Unknown
failed-attempt usage is reported as unreceipted capacity, not zero billed cost.

The conservative estimate uses **$3 per million tokens for every token**, above
the current native short-context Fast output and cache-write rates. This gives
a **$0.12 estimated upper envelope**, admitted under the **$0.15 ceiling**. A
lower ceiling that cannot cover the plan is rejected; the ceiling cannot be
raised. Prices were checked on 2026-09-21 against
[official pricing](https://developers.openai.com/api/docs/pricing) and
[Luna's model contract](https://developers.openai.com/api/docs/models/gpt-5.6-luna).
The pricing review expires after 30 days and must be refreshed deliberately.
These are estimates, not invoices; provider-bound violations cannot be undone,
and this is not an account-wide spending limit. No provider-hosted tools run.

Receipts include timestamp, source revision/dirty-tree hashes, binary hash and
version, requested model alias, parameters, transport, latency, checked usage,
turn/stop assertions, decisions, output sizes/hashes, conservative estimates and
cleanup. No raw prompts, human answers beyond fixed fixture decisions, model
text, API errors or child logs are retained. Exact credential canaries are
checked before reporting. A provider outage, credential/quota failure, provider
contract failure and local contract failure have distinct safe categories.
An opaque input-count failure remains `admission_preflight_failed`; the harness
does not invent a provider-outage diagnosis when the runtime hides that detail.

This is local, single-process HTTP acceptance with disposable JSON storage.
It is not PostgreSQL, multi-replica, failover, endurance, Railway or OpenShift
evidence. Those retain their separate acceptance gates. All test directories
and child processes are cleaned up; user histories and databases are untouched.

## Protected nightly option — inactive by default

`.github/workflows/live-smoke.yml` offers an owner-only manual dispatch and an
optional **03:17 UTC nightly** run, from the repository's default branch only.
There is no PR trigger, arbitrary ref, model or command input. Before enabling:

1. Skitsanos approves this exact model/envelope/cadence/credential environment.
2. Configure the `live-provider-smoke` GitHub environment with the existing
   approved key as `OPENAI_API_KEY`, default-branch-only deployment restrictions
   and skitsanos as the required reviewer (or explicitly approve unattended
   environment access). Never store the key as a repository variable.
3. Set repository variable `IRONCREW_LIVE_SMOKE_APPROVED=true` for manual access;
   additionally set `IRONCREW_LIVE_SMOKE_NIGHTLY=true` for the schedule.

Neither variable nor environment secret is provisioned by this change. Removing
either approval flag disables its corresponding execution path. The key is
injected only into the live step after offline validation, never during builds
or fork tests. Jobs are serialized, have a 15-minute workflow timeout, and upload
only the sanitized JSON receipt with seven-day artifact retention.

Regular CI and the local pre-push gate always run the offline harness tests.
They do not activate paid or scheduled calls. Platform CI remains necessary
before integration; a local receipt alone is not deployment evidence.

## Retained local acceptance

[The 2026-09-21 Luna receipt](reports/2026-09-21-luna.json) passed all four cases:
15 generation requests, 5,631 total tokens (41 reasoning tokens), 31.583 seconds,
and a conservative $0.016893 estimate. Both two-agent HITL paths allowed the
writer, denied the reviewer, checked the actual file effects and verified
truthful final answers. All four native run budgets reconciled with zero
retained reservations or in-flight attempts. Runtime cleanup passed.

The first tool-enabled live probe exposed an existing Responses schema mismatch:
the adapter advertised strict function schemas despite optional properties.
Native admission rejected it before generation. The fix explicitly sends
`strict: false` for shared function-tool schemas, without altering strict response
formats, tool validation or approvals. Request-body tests, the token-count
transport fixture and the synthetic provider now cover that regression.

The receipt binds `48729e5` plus its recorded dirty source manifest and exact
binary hash, not a published revision. Only documentation/index changes and
this preserved receipt were added after that run. It is one local repetition,
not a nightly-history, provider-uptime, model-quality or scalability claim.
