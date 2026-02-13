# spacebot.sh — Hosted Deployment Plan

One-click hosted Spacebot for people who don't want to self-host.

---

## What We're Building

A web app at spacebot.sh where a user signs up, connects their Discord/Slack/Telegram, configures their agent (identity, model preferences, API keys or pay-per-use), and gets a running Spacebot instance with zero infrastructure knowledge.

Each user gets an isolated Spacebot process with its own databases, identity, and messaging connections. Not multi-tenant — full isolation per user.

---

## Architecture

### Why Per-User Isolation

Spacebot is a stateful, long-running daemon. Each instance holds:

- Open websocket connections to Discord/Slack/Telegram
- SQLite, LanceDB, and redb databases on local disk
- In-memory state (active channels, branches, workers, cortex)
- Optional headless Chrome for browser workers

Multi-tenanting this would mean rewriting the core. The binary already handles multiple agents within one process, but sharing a process across users introduces security boundaries, resource contention, and failure blast radius problems that aren't worth solving. The simpler answer: one container per user, same binary they'd self-host.

### Fly Machines

Each user gets a Fly Machine (Firecracker microVM) with an attached Fly Volume for persistent storage.

**Why Fly over Kubernetes:**

- Fly's model is literally "one stateful process with a volume" — maps 1:1 to Spacebot
- Machines suspend when idle (saves VM state including memory), resume in hundreds of milliseconds
- Suspended machines only pay for storage (~$0.90/mo per idle user)
- No cluster management, no PVC provisioning delays, no control plane scaling concerns
- The Docker image is standard — migration to kube later is just a deployment target change

**Why not multi-tenant on fewer machines:**

- Spacebot holds open websocket connections per messaging platform — one user's Discord reconnect loop shouldn't affect another user
- SQLite doesn't do concurrent writers well across processes — you'd need to move to Postgres, which changes the entire data layer
- Browser workers spawn headless Chrome — untrusted code execution needs process-level isolation anyway
- Failure isolation: one user's OOM or panic kills only their instance

### Per-User App Model

Fly recommends one App per customer for isolation. Each user's app contains:

```
fly-app: spacebot-{user_id}
  machine: spacebot-{user_id}-main
    image: ghcr.io/jamiepine/spacebot:latest
    size: shared-cpu-1x, 512MB RAM
    volume: /data (5GB default, expandable)
    auto_stop: suspend
    auto_start: true (on inbound webhook)
```

The volume mounts at `/data`, which becomes the `SPACEBOT_DIR`. Contains everything:

```
/data/
├── config.toml          # generated from dashboard settings
├── agents/
│   └── main/
│       ├── workspace/   # identity files
│       ├── data/        # SQLite, LanceDB, redb
│       └── archives/
├── prompts/
└── logs/
```

### Idle Economics

Most personal agents are idle 90%+ of the time. Fly's suspend mode saves full VM state (memory + CPU registers) and only charges for storage.

| State | Cost/mo |
|-------|---------|
| Idle (suspended, 5GB volume) | ~$0.90 |
| Light use (4 hrs/day active) | ~$1.50 |
| Heavy use (always active) | ~$5-7 |
| Heavy use + browser workers (1GB RAM) | ~$8-12 |

These costs are infrastructure-only, before LLM usage.

### Wake Path

Two ways a suspended machine wakes up:

1. **Inbound webhook** — Fly Proxy detects a request to the app's internal address, wakes the machine, routes the request. This handles the webhook messaging adapter.
2. **Platform websockets** — Discord/Slack/Telegram use persistent websocket connections that break on suspend. The machine needs to wake on a schedule (or stay alive) if the user has platform adapters configured.

This creates two tiers:

| Tier | Wake Model | Use Case |
|------|-----------|----------|
| **Webhook-only** | Suspend when idle, wake on HTTP request | Programmatic access, API-driven agents |
| **Always-on** | Machine stays running, no suspend | Discord/Slack/Telegram bots that need persistent connections |

Webhook-only is cheaper. Always-on is the default for anyone connecting a messaging platform. Users who only interact via the dashboard or API can use webhook-only.

A middle ground: wake on a schedule (every 5 minutes), reconnect websockets, process any queued messages, then suspend again. This trades latency for cost but adds complexity. Worth exploring post-launch if the always-on cost is a barrier.

---

## Control Plane

A separate service that manages the fleet. This is NOT Spacebot — it's a standard web app.

### Stack

- **Web framework** — Next.js or similar (dashboard + API)
- **Database** — Postgres (user accounts, billing state, machine metadata)
- **Auth** — OAuth (Discord, Google, GitHub) + email/password
- **Payments** — Stripe (subscriptions + metered billing for LLM usage)
- **Fly API client** — HTTP calls to `api.machines.dev` for machine lifecycle

### What It Does

1. **User signup** — create account, choose a plan
2. **Onboarding wizard** — connect messaging platforms, set identity, pick models
3. **Provision** — create Fly App + Machine + Volume, generate `config.toml`, start the machine
4. **Dashboard** — agent management, memory browser, conversation history, heartbeat config
5. **Settings** — update identity files, model preferences, messaging connections
6. **Billing** — subscription tiers + optional pay-per-use LLM billing
7. **Monitoring** — machine health, restart on crash, usage metrics

### Provisioning Flow

```
User completes onboarding
    → Control plane creates Fly App (spacebot-{user_id})
    → Creates Volume (5GB, user's chosen region)
    → Creates Machine with:
        - Spacebot Docker image
        - Volume mounted at /data
        - Environment variables (API keys, config)
        - Auto-stop: suspend (or off for always-on tier)
    → Waits for machine to start
    → Writes config.toml to volume via init script
    → Machine starts Spacebot daemon
    → Messaging adapters connect
    → User gets a "your agent is live" confirmation
```

### Config Sync

When a user changes settings in the dashboard, the control plane needs to update the running Spacebot instance. Two approaches:

**Option A: Config file write + restart.** Control plane SSHs/execs into the machine, writes a new `config.toml`, and restarts the daemon. Simple but causes a brief interruption.

**Option B: Webhook API.** Spacebot exposes an HTTP endpoint (the webhook adapter) that accepts config updates. The control plane sends a PATCH to the running instance. Spacebot already supports hot-reload for config values, prompts, identity, and skills — this extends it to accept updates over HTTP.

Option B is better. The webhook adapter already exists in the architecture. Extend it with authenticated config endpoints.

### Dashboard Features

**Agent management:**
- Edit SOUL.md, IDENTITY.md, USER.md via a text editor in the browser
- Create/delete agents (multi-agent support)
- Configure model routing (which models for channels, workers, cortex)

**Memory browser:**
- Search memories by type, content, date
- View memory graph (associations, edges)
- Manual memory CRUD (create, edit, delete)
- Import memories from files (ingestion pipeline)

**Conversation history:**
- Browse past conversations across all channels
- View branch and worker activity per conversation
- Compaction history

**Heartbeats:**
- Create/edit/delete heartbeats
- View execution history and circuit breaker status
- Set active hours and delivery targets

**Monitoring:**
- Machine status (running, suspended, error)
- Resource usage (CPU, memory, disk)
- LLM usage (tokens consumed, cost estimate)
- Messaging adapter health

---

## Billing

### Plans

| Plan | Price | Includes |
|------|-------|----------|
| **Free** | $0/mo | Webhook-only, 1 agent, bring your own API keys, 1GB storage |
| **Pro** | $15/mo | Always-on, 3 agents, browser workers, 10GB storage, priority support |
| **Team** | $30/mo | Everything in Pro + 10 agents, shared API key pool with usage billing |

All plans support bring-your-own API keys (no markup). The Team plan adds a shared key pool where users pay per token at cost + margin.

### LLM Billing (Shared Keys)

For users who don't want to manage API keys:

- Track token usage per user via SpacebotHook (already reports usage events)
- Bill monthly at provider cost + 20% margin
- Set per-user spending limits with automatic pause
- Dashboard shows real-time usage and projected cost

### Storage Billing

Volume storage beyond plan limits: $0.20/GB/mo. Automatic alerts at 80% capacity.

---

## Docker Image

Single Dockerfile, multi-stage build:

```dockerfile
FROM rust:1.85-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    chromium \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/spacebot /usr/local/bin/
COPY prompts/ /opt/spacebot/prompts/

ENV SPACEBOT_DIR=/data
ENV CHROME_PATH=/usr/bin/chromium

VOLUME /data
EXPOSE 18789

ENTRYPOINT ["spacebot", "start", "--foreground"]
```

Chrome is included for browser workers. The image is ~150-200MB (Rust binary + Chromium + minimal Debian).

The `--foreground` flag is important — no daemonization inside a container. Logs go to stdout, container runtime handles lifecycle.

### Image Updates

When we push a new Spacebot version:

1. Build and push to `ghcr.io/jamiepine/spacebot:latest`
2. Control plane rolls out updates to all machines (Fly's machine update API swaps the image)
3. Machines restart with the new image, volume data persists
4. Rolling update — process a batch at a time, skip machines that are currently handling active conversations

---

## Phases

### Phase 1: MVP

Get one user running on Fly with a manually provisioned machine.

- Dockerfile that builds and runs Spacebot
- Fly App + Machine + Volume provisioned via `fly` CLI
- Config via environment variables
- No dashboard — config files edited directly
- Validates the deployment model works end-to-end

### Phase 2: Control Plane

- User signup with OAuth
- Onboarding wizard (connect Discord, set identity, pick model)
- Automated Fly provisioning via Machines API
- Basic dashboard (agent settings, start/stop)
- Stripe integration (Pro plan subscription)

### Phase 3: Dashboard

- Memory browser
- Conversation history viewer
- Heartbeat management
- Identity file editor
- LLM usage tracking

### Phase 4: Billing and Scale

- Shared API key pool with metered billing
- Storage expansion and billing
- Multi-region support (user picks region on signup)
- Image update rollout automation
- Monitoring and alerting

### Phase 5: Team Features

- Team accounts with shared billing
- Multiple agents per account with shared API keys
- Usage dashboards per agent
- Admin controls (spending limits, agent templates)

---

## Open Questions

1. **Region selection** — let users pick or auto-detect from browser geolocation? Volumes are region-pinned, so this is a permanent choice (or requires migration).

2. **Shared Discord bot** — should spacebot.sh provide a shared Discord bot token (users just invite "Spacebot" to their server), or require users to create their own bot? Shared is easier onboarding but means all users share one bot identity.

3. **Backup/export** — users should be able to export their data (memories, conversations, identity files). Fly Volume snapshots handle disaster recovery, but user-facing export needs a download endpoint.

4. **Custom domains** — for the webhook adapter, let users point their own domain at their Spacebot instance. Fly handles TLS automatically.

5. **Browser worker sandboxing** — Chrome in a per-user container is already isolated, but do we need additional sandboxing (seccomp profiles, network restrictions) to prevent abuse?
