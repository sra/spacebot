# Spacebot

A Rust agentic system where every LLM process has a dedicated role, and delegation is the only way work gets done.

## The Problem

In OpenClaw, everything happens inside sessions. A session receives messages, loads context, calls tools, compacts history, retrieves memories -- all in one thread. When it's doing work, it can't talk to you. When it's compacting, it goes dark for 20 seconds. When it retrieves memories, the raw results pollute the context with irrelevant noise. The session is the bottleneck for everything. OpenClaw _does_ have subagents, but handles them extremely poorly and there's no enforcement to their use.

Spacebot splits that monolith into specialized processes that only do one thing, and delegate everything else.

## How It Works

There are five concepts in Spacebot: channels, branches, workers, the compactor, and the cortex.

### Channels

A channel is the user-facing LLM process. One per conversation (a Telegram DM, a Discord thread, etc). It is the ambassador to the human.

A channel:

- Has the soul, identity, and personality
- Talks to the user
- Can see what workers are running
- Delegates everything else

A channel does not:

- Execute tasks directly
- Search memories itself
- Do any heavy tool work

The channel is always responsive. It is never blocked by work, never frozen by compaction, never polluted by retrieval noise. When it needs to think, it branches. When it needs work done, it spawns a worker. When its context gets close to full, the compactor has already handled it.

### Branches

A branch is a fork of the channel's context that goes off to think. It has the channel's full conversation history -- same context, same memories, same understanding of what's happening. But it operates independently, and the channel never sees the working, only the conclusion. Branches are where the deep thought happens. They're where recall happens. They're how the channel thinks without blocking.

When a message comes in that requires thought -- memory recall, deciding whether to spawn a worker, processing complex input -- the channel branches. The branch does its work (tool calls, memory searches, reasoning), and when it's done it sends a **branch result** back to the channel. This is a distinct message type in the channel's conversation -- not a user message, not a system message, but a conclusion from a thought process. The channel sees the result, incorporates it, and formulates a response to the user. Then the branch is deleted.

The channel is never waiting on a branch. While a branch is thinking, the channel keeps receiving messages. It can respond to simple messages directly, or spawn more branches for other messages. When a branch result arrives, the channel handles it on its next turn alongside whatever else is happening.

```
User A: "what do you know about X?"
    → Channel branches (branch-1)

User B: "hey, how's it going?"
    → Channel responds directly: "Going well! Working on something for A."

Branch-1 resolves: "Here's what I found about X: [curated memories]"
    → Channel sees the branch result on its next turn
    → Channel responds to User A with the findings
```

Branches can use certain tools -- memory recall, memory saving, spawning workers. But they don't have the channel's communication tools. They don't talk to users. They think and return a result.

In a busy environment (a Discord server with many active users), multiple branches can run concurrently. A message from user A triggers a branch, and before it returns, a message from user B triggers another branch. Both have the channel's context at the time they forked. The channel can throttle how many branches run at once (configurable).

**Concurrency model:** Branches run concurrently and merge on return -- first done, first incorporated. Each branch forks from the channel's context at creation time, like a git branch. If two branches both save a memory, that's fine -- they're writing to the database, not to each other. The channel sees each conclusion independently as it arrives.

The distinction between a branch and a worker:

- A **branch** has the channel's context. It's a thought process. It can recall memories, save memories, spawn workers. Short-lived.
- A **worker** gets a fresh prompt with a specific task. It has no channel context. It's a unit of work. Can be long-running.

A branch might decide that a task requires a worker and spawn one. The branch is how the channel thinks. The worker is how the system does work.

**Branch → Worker lifecycle:** The branch decides. For a quick memory recall, the branch waits for the worker and returns the result directly. For a long-running coding task, the branch spawns the worker, sets a status ("started refactoring auth module"), and returns. The worker lives on independently. The branch is gone, but the channel can see the worker's status.

### Workers

A worker is an independent process that does a job. It gets a specific task, a focused system prompt, and the tools it needs. No channel context, no soul, no personality. Workers don't know about channels or branches.

There are two kinds:

**Fire-and-forget workers** do a job and return a result. Memory recall, summarization, one-shot tasks. A branch spawns one, waits for the result (or not), and incorporates it. Once done, the worker is gone.

**Interactive workers** are long-running processes that accept follow-up input from the channel. A coding session is the canonical example. The user says "refactor the auth module", a branch spawns an interactive worker, the branch returns ("started a coding session"), and the worker keeps running. When the user later says "actually, update the tests too", the channel recognizes this is directed at the active worker and routes the message to it directly -- no branch needed for that.

```
User: "refactor the auth module"
    → Branch spawns interactive coding worker
    → Branch returns: "Started a coding session for the auth refactor"
    → Branch deleted

User: "actually, update the tests too"
    → Channel sees active worker in status, routes message to it
    → Worker receives follow-up, continues with its existing context
```

Interactive workers have an input channel that the channel can send messages to. The channel is smart enough to know when a user message is directed at a running worker based on conversation context and the status block.

A worker doesn't have to be a Spacebot LLM process. It can be anything that accepts a task and reports status. Some examples:

- **Built-in agentic worker** -- a Rig agent loop with shell/file/exec tools. General purpose.
- **OpenCode instance** -- a full coding agent with its own context management, codebase exploration, and tool suite. Spawned as a subprocess with a specific model, initial prompt, and working directory. This is the ideal worker for coding tasks because it builds deep codebase context over time.
- **External tool** -- any process that can accept input and report status. A long-running script, a CI pipeline, a data processing job.

The key property: workers are pluggable. The system doesn't care what's inside the worker as long as it can receive a task, report status, accept follow-ups (if interactive), and return a result.

### Status Injection

Every turn, the channel gets a live status block injected into its context -- a snapshot of active workers and recently completed work. The channel sees this automatically without polling or asking.

Workers set their own status via a tool -- the LLM decides when a status update is meaningful ("running tests, 3/7 passing") rather than reporting every micro-step. This keeps status useful without flooding.

Branches are usually too short-lived for status to matter (they return in seconds). They only appear in the status block if they've been running longer than a few seconds. Short branches are invisible -- the channel just sees the conclusion appear.

When a worker completes, its result appears in the status block as a completed item. Workers can be marked as notify-on-complete (important tasks the user should hear about) or silent (background work that just disappears from the list). If a worker finishes while the user is mid-conversation about something else, the channel decides whether to mention it based on the notify flag.

```
User is talking about dinner plans.
Auth refactor worker completes (notify: true).
Channel's next turn sees: "completed: refactor auth module, all tests passing"
Channel: "By the way, the auth refactor just finished -- all 7 tests passing. Anyway, about dinner..."
```

When the user asks "how's the refactoring going?", the channel already has the answer in its context. It can respond immediately.

If the user wants more detail -- "show me what the auth refactor has done so far" -- the channel branches. That branch reads the worker's logs, curates them, and returns a summary. The channel never has to load raw worker output into its own context. The branch handles the noise, the channel gets the signal.

The channel can cancel workers or branches. But it can't interfere with their execution beyond that -- no injecting instructions, no modifying their context mid-run. Cancel is the only control.

### The Compactor

A compactor is **not** an LLM process. It's a programmatic monitor attached to each channel. It watches the channel's context size and triggers compaction before the channel fills up.

When context crosses a threshold (say 80%), the compactor spins off a compaction worker. That worker reads the older conversation turns, produces a condensed summary, and extracts any memories worth keeping. The compacted summary swaps into the channel's context. The channel never stops, never blocks, never notices.

Compaction summaries stack at the top of the context window. There might be 5 or 10 compaction summaries covering the last hour, followed by the recent conversation. This gives the channel a rolling awareness of what happened without carrying the full raw history.

Because the compactor is programmatic (not an LLM), it's cheap. It's just watching a number (context token count) and spawning workers when needed. The LLM work happens in the compaction worker, which runs alongside the channel without blocking it.

### The Cortex

The cortex is the inner monologue of the entire system. It watches what's happening across all channels and makes system-level decisions.

Where compactors handle per-channel compaction, the cortex operates at the system level -- observing patterns across channels, consolidating related memories, managing associations and decay, triggering routines. Compactors see one conversation. The cortex sees the whole picture.

The cortex works on a rolling window of high-level activity. It sees signals from channels and compactors -- not raw conversation data or tool output. It never compacts because its context never fills up.

At small scale, there's one cortex. At large scale (100+ channels), multiple cortex instances can load-balance across channels, all writing to the same memory store.

## Why Nothing Ever Blocks

The whole design exists to prevent any process from ever going dark:

- **Channels** don't do work. They branch to think, spawn workers for tasks, and the compactor handles compaction. Always responsive.
- **Branches** fork the context, think, and return a conclusion. The channel keeps receiving messages while branches are active.
- **Workers** run independently. The channel knows they exist but doesn't wait for them.
- **Compactors** are programmatic. They monitor context size and trigger compaction workers in the background.
- **The cortex** only processes high-level signals. Its context stays small.

No process in the system ever stops to compact, ever blocks on a tool call, or ever goes unresponsive.

## The Delegation Principle

```
User sends message
    → Channel receives it
        → Branches to think (has channel's context)
            → Branch recalls memories, decides what to do
            → Branch might spawn a worker for heavy tasks
            → Branch returns conclusion
        → Branch deleted
    → Channel responds to user

Channel context hits 80%
    → Compactor (programmatic) notices
        → Spins off a compaction worker
            → Worker summarizes old context + extracts memories
            → Compacted summary swaps in
    → Channel never interrupted

Multiple users active in Discord
    → Message from user A → branch
    → Message from user B → branch (concurrent)
    → Both branches thinking independently
    → Channel incorporates conclusions as they return

Cortex observes system activity
    → Consolidates memories across channels
    → Manages decay, triggers routines
    → Delegates any heavy work to workers
```

## What Each Process Gets

| Process   | Type         | System Prompt               | Tools                                                      | Context                             |
| --------- | ------------ | --------------------------- | ---------------------------------------------------------- | ----------------------------------- |
| Channel   | LLM          | Soul, identity, personality | Reply, branch, spawn workers, memory save, route to worker | Conversation + compaction summaries |
| Branch    | LLM          | Inherited from channel      | Memory recall, memory save, spawn workers                  | Fork of channel's context           |
| Worker    | Pluggable    | Task-specific instructions  | Depends on worker type                                     | Fresh prompt, task description      |
| Compactor | Programmatic | N/A                         | Monitor context, trigger compaction workers                | N/A                                 |
| Cortex    | LLM          | System management           | Memory consolidation, system monitoring                    | Rolling window of system activity   |

## Heartbeats

In OpenClaw, the heartbeat is a single periodic LLM call that reads HEARTBEAT.md and decides what to do. It runs in the agent's main session, competing for context with the conversation. If it does too much work, it triggers compaction. While it runs, nothing else can use that session. All heartbeat tasks are crammed into one checklist in one file, executed in one call, serialized globally. If you want two things to happen at different intervals, you can't -- everything runs on the same timer.

Spacebot replaces this with multiple independent heartbeats, each with its own schedule and its own short-lived channel.

### How Heartbeats Work

A heartbeat is a task stored in the database with an interval and a prompt. When its timer fires, it gets a fresh channel -- the same kind of channel that talks to users, with the same branching and worker capabilities, but short-lived. It does its work, branches if it needs to think, spawns workers if it needs heavy execution, and shuts down when it's done.

```
Heartbeat "check-inbox" fires (every 30m)
    → Fresh channel spins up
        → Branches to recall relevant memories
        → Spawns a worker to check email
        → Worker returns results
        → Branch decides what to report
    → Channel delivers message to user (or stays silent)
    → Channel shuts down

Heartbeat "daily-summary" fires (every 24h)
    → Fresh channel spins up
        → Branches to gather activity across channels
        → Spawns workers to compile summaries
        → Branch composes a digest
    → Channel delivers summary to user
    → Channel shuts down
```

Multiple heartbeats run independently. "check-inbox" could still be running with its own workers when "daily-summary" fires. They don't share sessions, don't compete for context, don't block each other.

### What Changes from OpenClaw

| OpenClaw                                           | Spacebot                                                   |
| -------------------------------------------------- | ---------------------------------------------------------- |
| One heartbeat, one interval, one HEARTBEAT.md      | Multiple heartbeats, each with its own interval and prompt |
| Runs in the main session, competes for context     | Each heartbeat gets a fresh short-lived channel            |
| Single LLM call tries to do everything             | Branches and workers handle the actual work                |
| Serialized globally -- only one at a time          | Independent, concurrent                                    |
| HEARTBEAT_OK magic token to signal "nothing to do" | Programmatic -- if there's nothing to do, don't spin up    |
| Can trigger compaction on the main session         | No compaction risk -- channel is short-lived               |

### Heartbeat Storage

Heartbeats are defined in the database, not a markdown file. Each heartbeat has:

- A prompt (what to do)
- An interval (how often)
- A delivery target (which channel/user to report to, if anything)
- Active hours (optional -- only run during certain times)

They can be created, modified, and deleted through the system. The cortex could create heartbeats. A user could create them through conversation. They're just database rows with schedules.

### Persistence

Heartbeat channels are saved the same way regular channels are -- conversation history is persisted. This means you can look back at what a heartbeat did, when it ran, what it found. But because each run gets a fresh channel, there's no context accumulation across runs. Every heartbeat starts clean.

## Stack

Rust, tokio, SQLite, [LanceDB](https://lancedb.github.io/lancedb/) (embedded vector + FTS), [redb](https://github.com/cberner/redb) (embedded key-value for settings + secrets), [Rig](https://github.com/0xPlaygrounds/rig) (agentic loop framework)

No server dependencies. The entire system is a single binary that creates files in a data directory.

## Status

Design phase.
