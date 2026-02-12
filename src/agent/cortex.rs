//! Cortex: System-level observer and memory bulletin generator.
//!
//! The cortex's primary responsibility is generating the **memory bulletin** — a
//! periodically refreshed, LLM-curated summary of the agent's current knowledge.
//! This bulletin is injected into every channel's system prompt, giving all
//! conversations ambient awareness of who the user is, what's been decided,
//! what happened recently, and what's going on.
//!
//! The cortex also observes system-wide activity via signals for future use in
//! health monitoring and memory consolidation.

use crate::error::Result;
use crate::llm::SpacebotModel;
use crate::{AgentDeps, ProcessEvent, ProcessType};
use crate::hooks::CortexHook;

use rig::agent::AgentBuilder;
use rig::completion::{CompletionModel, Prompt};

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// The cortex observes system-wide activity and maintains the memory bulletin.
pub struct Cortex {
    pub deps: AgentDeps,
    pub hook: CortexHook,
    /// Recent activity signals (rolling window).
    pub signal_buffer: Arc<RwLock<Vec<Signal>>>,
    /// System prompt loaded from prompts/CORTEX.md.
    pub system_prompt: String,
}

/// A high-level activity signal (not raw conversation).
#[derive(Debug, Clone)]
pub enum Signal {
    /// Channel started.
    ChannelStarted { channel_id: String },
    /// Channel ended.
    ChannelEnded { channel_id: String },
    /// Memory was saved.
    MemorySaved {
        memory_type: String,
        content_summary: String,
        importance: f32,
    },
    /// Worker completed.
    WorkerCompleted {
        task_summary: String,
        result_summary: String,
    },
    /// Compaction occurred.
    Compaction {
        channel_id: String,
        turns_compacted: i64,
    },
    /// Error occurred.
    Error {
        component: String,
        error_summary: String,
    },
}

impl Cortex {
    /// Create a new cortex.
    pub fn new(deps: AgentDeps, system_prompt: impl Into<String>) -> Self {
        let hook = CortexHook::new();

        Self {
            deps,
            hook,
            signal_buffer: Arc::new(RwLock::new(Vec::with_capacity(100))),
            system_prompt: system_prompt.into(),
        }
    }

    /// Process a process event and extract signals.
    pub async fn observe(&self, event: ProcessEvent) {
        let signal = match &event {
            ProcessEvent::MemorySaved { memory_id, .. } => Some(Signal::MemorySaved {
                memory_type: "unknown".into(),
                content_summary: format!("memory {}", memory_id),
                importance: 0.5,
            }),
            ProcessEvent::WorkerComplete { result, .. } => Some(Signal::WorkerCompleted {
                task_summary: "completed task".into(),
                result_summary: result.lines().next().unwrap_or("done").into(),
            }),
            ProcessEvent::CompactionTriggered {
                channel_id,
                threshold_reached,
                ..
            } => Some(Signal::Compaction {
                channel_id: channel_id.to_string(),
                turns_compacted: (*threshold_reached * 100.0) as i64,
            }),
            _ => None,
        };

        if let Some(signal) = signal {
            let mut buffer = self.signal_buffer.write().await;
            buffer.push(signal);

            if buffer.len() > 100 {
                buffer.remove(0);
            }

            tracing::debug!("cortex received signal, buffer size: {}", buffer.len());
        }
    }

    /// Run periodic consolidation (future: health monitoring, memory maintenance).
    pub async fn run_consolidation(&self) -> Result<()> {
        tracing::info!("cortex running consolidation");
        Ok(())
    }
}

/// Spawn the cortex bulletin loop for an agent.
///
/// Generates a memory bulletin immediately on startup, then refreshes on a
/// configurable interval. The bulletin is stored in `RuntimeConfig::memory_bulletin`
/// and injected into every channel's system prompt.
pub fn spawn_bulletin_loop(deps: AgentDeps) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = run_bulletin_loop(&deps).await {
            tracing::error!(%error, "cortex bulletin loop exited with error");
        }
    })
}

async fn run_bulletin_loop(deps: &AgentDeps) -> anyhow::Result<()> {
    tracing::info!("cortex bulletin loop started");

    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY_SECS: u64 = 15;

    // Run immediately on startup, with retries
    for attempt in 0..=MAX_RETRIES {
        if generate_bulletin(deps).await {
            break;
        }
        if attempt < MAX_RETRIES {
            tracing::info!(
                attempt = attempt + 1,
                max = MAX_RETRIES,
                "retrying bulletin generation in {RETRY_DELAY_SECS}s"
            );
            tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
        }
    }

    loop {
        let cortex_config = **deps.runtime_config.cortex.load();
        let interval = cortex_config.bulletin_interval_secs;

        tokio::time::sleep(Duration::from_secs(interval)).await;

        generate_bulletin(deps).await;
    }
}

/// Generate a memory bulletin and store it in RuntimeConfig.
///
/// On failure, the previous bulletin is preserved (not blanked out).
/// Returns `true` if the bulletin was successfully generated.
pub async fn generate_bulletin(deps: &AgentDeps) -> bool {
    tracing::info!("cortex generating memory bulletin");

    let cortex_config = **deps.runtime_config.cortex.load();
    let bulletin_prompt = deps.runtime_config.prompts.load().cortex_bulletin.clone();

    let routing = deps.runtime_config.routing.load();
    let model_name = routing.resolve(ProcessType::Branch, None).to_string();
    let model =
        SpacebotModel::make(&deps.llm_manager, &model_name).with_routing((**routing).clone());

    let tool_server = crate::tools::create_branch_tool_server(deps.memory_search.clone());

    let agent = AgentBuilder::new(model)
        .preamble(&bulletin_prompt)
        .default_max_turns(cortex_config.bulletin_max_turns)
        .tool_server_handle(tool_server)
        .build();

    let user_prompt = format!(
        "Generate a memory bulletin for the agent. Target length: {} words or fewer. \
         Make one memory_recall call per memory type using the memory_type filter parameter: \
         identity, fact, decision, event, preference, observation, goal. That's 7 queries total, \
         one per turn. Then synthesize into a detailed briefing.",
        cortex_config.bulletin_max_words
    );

    let mut history = Vec::new();
    match agent.prompt(&user_prompt).with_history(&mut history).await {
        Ok(bulletin) => {
            let word_count = bulletin.split_whitespace().count();
            tracing::info!(
                words = word_count,
                bulletin = %bulletin,
                "cortex bulletin generated"
            );
            deps.runtime_config
                .memory_bulletin
                .store(Arc::new(bulletin));
            true
        }
        Err(rig::completion::PromptError::MaxTurnsError { .. }) => {
            // Extract whatever the LLM produced so far
            let partial = extract_last_assistant_text(&history);
            if let Some(text) = partial {
                let word_count = text.split_whitespace().count();
                tracing::warn!(
                    words = word_count,
                    bulletin = %text,
                    "cortex bulletin hit max turns, using partial result"
                );
                deps.runtime_config
                    .memory_bulletin
                    .store(Arc::new(text));
                true
            } else {
                tracing::warn!("cortex bulletin hit max turns with no usable output");
                false
            }
        }
        Err(error) => {
            tracing::error!(%error, "cortex bulletin generation failed, keeping previous bulletin");
            false
        }
    }
}

/// Extract the last assistant text message from a history.
fn extract_last_assistant_text(history: &[rig::message::Message]) -> Option<String> {
    for message in history.iter().rev() {
        if let rig::message::Message::Assistant { content, .. } = message {
            let texts: Vec<String> = content
                .iter()
                .filter_map(|c| {
                    if let rig::message::AssistantContent::Text(t) = c {
                        Some(t.text.clone())
                    } else {
                        None
                    }
                })
                .collect();
            if !texts.is_empty() {
                return Some(texts.join("\n"));
            }
        }
    }
    None
}
