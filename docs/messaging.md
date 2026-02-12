# Messaging

How Spacebot connects to external chat platforms (Discord, Telegram, etc).

## Terminology

"Channel" in Spacebot means the user-facing LLM process -- the thing with soul, personality, and conversation history. A "messaging adapter" is the I/O layer that connects a chat platform (Discord, Telegram, etc) to the Spacebot system. Adapters produce inbound messages and deliver outbound responses. Channels do the thinking.

```
Discord message arrives
    → Discord adapter produces InboundMessage
    → Router maps conversation_id to a Channel
    → Channel processes (branches, workers, etc)
    → Channel produces OutboundResponse
    → Router sends it back through the Discord adapter
    → User sees the reply in Discord
```

## Core Types

These live in `src/` (not in the messaging module) because they're the contract between messaging adapters and the rest of the system. Every adapter produces and consumes these types.

### InboundMessage

```rust
pub struct InboundMessage {
    pub id: String,
    pub source: String,             // "discord", "telegram", "webhook"
    pub conversation_id: String,    // uniquely identifies a conversation (see Routing)
    pub sender_id: String,
    pub content: MessageContent,
    pub timestamp: DateTime<Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
}

pub enum MessageContent {
    Text(String),
    Media {
        text: Option<String>,
        attachments: Vec<Attachment>,
    },
}

pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub url: String,
    pub size_bytes: Option<u64>,
}
```

The `metadata` field carries platform-specific data (Discord message flags, Telegram reply-to info, etc) without polluting the core type. Adapters write it, the router ignores it, and the Channel's `reply` tool can pass it back when responding so the adapter knows how to format the response (thread reply, inline reply, etc).

### OutboundResponse

```rust
pub enum OutboundResponse {
    Text(String),
    StreamStart,
    StreamChunk(String),
    StreamEnd,
}
```

Text for complete messages. The `Stream*` variants enable real-time streaming where the platform supports it. Each adapter decides how to render streaming. Adapters that don't support streaming buffer all chunks and send the final text as a single message on `StreamEnd`.

### StatusUpdate

```rust
pub enum StatusUpdate {
    Thinking,
    ToolStarted { tool_name: String },
    ToolCompleted { tool_name: String },
    BranchStarted,
    WorkerStarted { task: String },
    WorkerCompleted { task: String, result: String },
}
```

Status updates flow from `SpacebotHook` through the adapter to the platform. Discord and Telegram show typing indicators. Webhook has no persistent connection so status is a no-op. Adapters that don't support status just ignore them (the trait method has a default no-op).

### InboundStream

```rust
pub type InboundStream = Pin<Box<dyn Stream<Item = InboundMessage> + Send>>;
```

Each adapter produces one of these from `start()`. The `MessagingManager` merges them all via `select_all()`.

## The Messaging Trait

```rust
pub trait Messaging: Send + Sync + 'static {
    /// Unique name for this adapter ("discord", "telegram", "webhook")
    fn name(&self) -> &str;

    /// Connect to the platform and return a stream of inbound messages.
    /// Called once at startup. The stream should run until shutdown.
    fn start(&self) -> impl Future<Output = Result<InboundStream>> + Send;

    /// Send a response to a specific inbound message.
    fn respond(
        &self,
        message: &InboundMessage,
        response: OutboundResponse,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Push a status update to the platform (typing indicator, tool activity).
    /// Default: no-op. Override if the platform supports status.
    fn send_status(
        &self,
        message: &InboundMessage,
        status: StatusUpdate,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Send a proactive message not tied to an inbound message.
    /// Used for heartbeat results, worker completions with notify flag, etc.
    fn broadcast(
        &self,
        target: &str,
        response: OutboundResponse,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Check if the platform connection is healthy.
    fn health_check(&self) -> impl Future<Output = Result<()>> + Send;

    /// Graceful shutdown. Close connections, flush pending messages.
    fn shutdown(&self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}
```

Since adapters are registered at runtime based on config, the `MessagingManager` needs dynamic dispatch. This is a case for the companion `Dyn` trait pattern from the style guide -- implement the static trait for type safety, use the `Dyn` version for runtime polymorphism.

## Module Layout

```
src/
├── lib.rs                  — InboundMessage, OutboundResponse, StatusUpdate, InboundStream
│
├── messaging.rs            → messaging/
│   ├── traits.rs           — Messaging trait + MessagingDyn companion
│   ├── manager.rs          — MessagingManager: start all, fan-in, route outbound
│   ├── discord.rs          — Discord adapter
│   ├── telegram.rs         — Telegram adapter
│   └── webhook.rs          — Webhook receiver (programmatic access)
```

The module root (`src/messaging.rs`) re-exports the trait and manager. Individual adapters are private -- the manager is the only public interface.

## MessagingManager

Fan-in all inbound streams. Route outbound by adapter name.

```rust
pub struct MessagingManager {
    adapters: HashMap<String, Arc<dyn MessagingDyn>>,
}

impl MessagingManager {
    /// Register an adapter. Called during startup based on config.
    pub fn register(&mut self, adapter: impl Messaging) {
        let name = adapter.name().to_string();
        self.adapters.insert(name, Arc::new(adapter));
    }

    /// Start all registered adapters and merge their inbound streams.
    pub async fn start(&self) -> Result<InboundStream> {
        let streams = futures::future::try_join_all(
            self.adapters.values().map(|a| a.start())
        ).await?;
        Ok(Box::pin(futures::stream::select_all(streams)))
    }

    /// Route a response back to the correct adapter.
    pub async fn respond(
        &self,
        message: &InboundMessage,
        response: OutboundResponse,
    ) -> Result<()> {
        let adapter = self.adapters.get(&message.source)
            .with_context(|| format!("no messaging adapter named '{}'", message.source))?;
        adapter.respond(message, response).await
    }

    /// Route a status update to the correct adapter.
    pub async fn send_status(
        &self,
        message: &InboundMessage,
        status: StatusUpdate,
    ) -> Result<()> {
        let adapter = self.adapters.get(&message.source)
            .with_context(|| format!("no messaging adapter named '{}'", message.source))?;
        adapter.send_status(message, status).await
    }

    /// Send a proactive message through a specific adapter.
    pub async fn broadcast(
        &self,
        adapter_name: &str,
        target: &str,
        response: OutboundResponse,
    ) -> Result<()> {
        let adapter = self.adapters.get(adapter_name)
            .with_context(|| format!("no messaging adapter named '{adapter_name}'"))?;
        adapter.broadcast(target, response).await
    }
}
```

The main loop consumes the merged `InboundStream` and feeds messages into the router:

```rust
let mut inbound = messaging_manager.start().await?;

while let Some(message) = inbound.next().await {
    let channel = router.resolve_or_create(&message).await?;
    channel.handle_message(message).await?;
}
```

## Routing

Each adapter produces a `conversation_id` that maps to a Spacebot Channel. The format is platform-specific:

| Adapter | conversation_id format | Example |
|---------|----------------------|---------|
| Discord | `discord:<guild_id>:<channel_id>` | `discord:123:456` |
| Discord DM | `discord:dm:<user_id>` | `discord:dm:789` |
| Discord thread | `discord:<guild_id>:<thread_id>` | `discord:123:thread_456` |
| Telegram | `telegram:<chat_id>` | `telegram:-100123` |
| Webhook | `webhook:<caller_id>` | `webhook:github-ci` |

The router maintains a map of `conversation_id → Channel`. First message for a conversation creates a new Channel. Subsequent messages route to the existing one.

Cross-platform identity linking (same human on Discord and Telegram sharing a Channel) is a future concern. For now, each platform conversation gets its own Channel.

## Configuration

Messaging config lives in redb alongside the rest of the system config. Each adapter reads its own key namespace at startup.

```
messaging:discord:enabled       → bool
messaging:discord:token         → encrypted (via DecryptedSecret)
messaging:discord:guilds        → JSON array of guild IDs to monitor

messaging:telegram:enabled      → bool
messaging:telegram:token        → encrypted (via DecryptedSecret)

messaging:webhook:enabled       → bool
messaging:webhook:port          → u16
messaging:webhook:bind          → String (default "127.0.0.1")
messaging:webhook:secret        → encrypted (via DecryptedSecret, optional)
```

Tokens are stored encrypted using the existing secrets system (AES-256-GCM, redb). The `messaging:<name>:enabled` flag allows enabling/disabling adapters at runtime without removing credentials.

At startup, the system reads all `messaging:*:enabled` keys, constructs the enabled adapters, and registers them with the `MessagingManager`.

## Streaming

For platforms that support it, text deltas flow from the Channel to the user in real time.

The flow:

```
Channel's SpacebotHook.on_text_delta()
    → pushes StreamChunk to the messaging manager
    → adapter receives OutboundResponse::StreamChunk
    → platform-specific rendering
```

Each adapter handles streaming differently:

**Discord:** Send an initial message on `StreamStart`, edit it in-place on each `StreamChunk`. Rate-limit edits to avoid API throttling. Send final edit on `StreamEnd`.

**Telegram:** Send an initial message on `StreamStart`, call `editMessageText` on each `StreamChunk`. Buffer chunks and edit at intervals -- Telegram's `editMessageText` has a ~1 second effective rate limit.

**Webhook:** No streaming (request-response). The full response is returned when the agent finishes. If the caller wants real-time output, that's a web UI concern (separate from messaging).

Adapters that don't support streaming buffer all chunks and send the final text as a single message on `StreamEnd`.

### Block Streaming Coalescing

Forwarding every LLM token delta as a separate message edit is wasteful and hits platform rate limits. Adapters should coalesce chunks before sending updates.

The coalescer accumulates `StreamChunk` text and flushes based on configurable thresholds:

```rust
pub struct CoalesceConfig {
    /// Minimum chars accumulated before flushing an edit.
    pub min_chars: usize,
    /// Maximum chars before forcing a flush regardless of idle time.
    pub max_chars: usize,
    /// Flush after this duration of no new chunks (LLM is "pausing").
    pub idle_timeout: Duration,
}
```

Reasonable defaults per platform:

| Platform | min_chars | max_chars | idle_timeout |
|----------|-----------|-----------|-------------|
| Discord | 200 | 1500 | 500ms |
| Telegram | 300 | 2000 | 1000ms |

The coalescer runs per-message (one active response = one coalescer instance). On `StreamStart` it sends the initial message. As chunks arrive it accumulates text and flushes an edit when any threshold is hit. On `StreamEnd` it flushes whatever remains.

This sits inside each adapter's `respond()` implementation, not in the manager or core. Different platforms have different rate limits and the coalescing strategy is a platform concern.

## Webhook Adapter

The webhook adapter is the simplest one. It exposes an HTTP endpoint where external systems can POST messages into Spacebot.

```
POST /webhook
{
    "message": "deploy to staging",
    "sender_id": "github-ci",
    "conversation_id": "github-ci"
}

→ 202 Accepted { "id": "msg-uuid" }
```

This is for programmatic access -- CI hooks, monitoring alerts, external scripts, `curl` from the terminal during development. The "platform" is anything that can make an HTTP request.

Optionally supports sync mode: if the request includes `"wait": true`, the HTTP response blocks until the agent replies (with a timeout). Useful for scripts that need the result.

```
POST /webhook
{
    "message": "what's the status of the auth refactor?",
    "sender_id": "james",
    "conversation_id": "dev-queries",
    "wait": true
}

→ 200 OK { "response": "The auth refactor worker completed 10 minutes ago..." }
```

The webhook adapter does NOT include:
- SSE or WebSocket streaming
- A chat UI
- Session management
- Any browser-facing features

If Spacebot ever gets a web UI, that would be its own subsystem -- not part of the messaging layer.

## Platform-Specific Notes

### Discord

- Uses serenity or twilight as the Rust Discord library
- Gateway connection for real-time events (messages, reactions, etc)
- Bot needs `MESSAGE_CONTENT` intent for reading message text
- Thread support: messages in threads produce a `conversation_id` scoped to the thread, not the parent channel
- Status: send typing indicator on `StatusUpdate::Thinking`, stop on response
- Streaming: edit message in-place with coalesced chunks

### Telegram

- Uses teloxide or grammers as the Rust Telegram library
- Long polling or webhook mode for receiving updates
- Bot API for sending/editing messages
- Group support: bot must be mentioned or replied to in groups (configurable)
- Status: `sendChatAction("typing")` on `StatusUpdate::Thinking`
- Streaming: edit message in-place via `editMessageText`, coalesce to ~1s intervals

### Webhook

- Axum HTTP server bound to configurable address/port
- `POST /webhook` -- accepts JSON messages, produces `InboundMessage`
- Optional shared secret auth via `X-Webhook-Secret` header
- Optional sync mode (`"wait": true`) blocks until agent responds
- Localhost-only by default
- No streaming, no persistent connections, no UI

## Adding a New Adapter

1. Create `src/messaging/<name>.rs`
2. Implement the `Messaging` trait
3. Add the module declaration to `src/messaging.rs`
4. Add config keys (`messaging:<name>:*`) to redb
5. Register the adapter in the startup code (conditional on `messaging:<name>:enabled`)

The adapter only needs to produce `InboundMessage` and consume `OutboundResponse`. It doesn't know about Channels, branches, workers, memory, or any other internal system. The boundary is the shared types in `src/lib.rs`.
