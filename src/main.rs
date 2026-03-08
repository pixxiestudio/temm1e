use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, Subcommand};
use anyhow::Result;
use skyclaw_core::Channel;
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "skyclaw")]
#[command(about = "Cloud-native Rust AI agent runtime")]
#[command(version)]
struct Cli {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Runtime mode: cloud, local, or auto
    #[arg(long, default_value = "auto")]
    mode: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the SkyClaw gateway daemon
    Start {
        /// Enable GUI mode (headed browser, desktop interaction)
        #[arg(long)]
        gui: bool,
    },
    /// Interactive CLI chat with the agent
    Chat,
    /// Show gateway status, connected channels, provider health
    Status,
    /// Manage skills
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Migrate from OpenClaw or ZeroClaw
    Migrate {
        /// Source platform: openclaw or zeroclaw
        #[arg(long)]
        from: String,
        /// Path to source workspace
        path: String,
    },
    /// Show version information
    Version,
}

#[derive(Subcommand)]
enum SkillCommands {
    /// List installed skills
    List,
    /// Show skill details
    Info { name: String },
    /// Install a skill from a path
    Install { path: String },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Validate the current configuration
    Validate,
    /// Show resolved configuration
    Show,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    // Load configuration
    let config_path = cli.config.as_ref().map(std::path::Path::new);
    let config = skyclaw_core::config::load_config(config_path)?;

    tracing::info!(mode = %cli.mode, "SkyClaw starting");

    match cli.command {
        Commands::Start { gui } => {
            tracing::info!(gui = gui, "Starting SkyClaw gateway");

            // Initialize AI provider
            let provider: Arc<dyn skyclaw_core::Provider> = Arc::from(
                skyclaw_providers::create_provider(&config.provider)?
            );
            tracing::info!(provider = %provider.name(), "Provider initialized");

            // Initialize memory backend
            let memory_url = config.memory.path.clone().unwrap_or_else(|| {
                let data_dir = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".skyclaw");
                std::fs::create_dir_all(&data_dir).ok();
                format!("sqlite:{}/memory.db?mode=rwc", data_dir.display())
            });
            let memory: Arc<dyn skyclaw_core::Memory> = Arc::from(
                skyclaw_memory::create_memory_backend(&config.memory.backend, &memory_url).await?
            );
            tracing::info!(backend = %config.memory.backend, "Memory initialized");

            // Initialize Telegram channel if configured
            let mut channels: Vec<Arc<dyn skyclaw_core::Channel>> = Vec::new();
            let mut primary_channel: Option<Arc<dyn skyclaw_core::Channel>> = None;
            let mut tg_rx: Option<tokio::sync::mpsc::Receiver<skyclaw_core::types::message::InboundMessage>> = None;

            if let Some(tg_config) = config.channel.get("telegram") {
                if tg_config.enabled {
                    let mut tg = skyclaw_channels::TelegramChannel::new(tg_config)?;
                    tg.start().await?;
                    tg_rx = tg.take_receiver();
                    let tg_arc: Arc<dyn skyclaw_core::Channel> = Arc::new(tg);
                    channels.push(tg_arc.clone());
                    primary_channel = Some(tg_arc.clone());
                    tracing::info!("Telegram channel started");
                }
            }

            // Shared pending-message queue — the dispatcher pushes incoming
            // user messages here when the chat is busy; the check_messages
            // tool reads from it so the agent can stay aware.
            let pending_messages: skyclaw_tools::PendingMessages =
                Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

            // Initialize tools (with channel for file transfer if available)
            let tools = skyclaw_tools::create_tools(
                &config.tools,
                primary_channel.clone(),
                Some(pending_messages.clone()),
                gui,
            );
            tracing::info!(count = tools.len(), "Tools initialized");

            // System prompt — gives the agent identity and tool awareness
            let system_prompt = Some(
                "You are SkyClaw, a cloud-native AI agent running on a remote server. \
                 You have full access to these tools:\n\
                 - shell: run any command\n\
                 - file_read / file_write / file_list: filesystem operations\n\
                 - web_fetch: HTTP GET requests\n\
                 - browser: control a real Chrome browser (navigate, click, type, screenshot, \
                   get_text, evaluate JS, get_html) — use this for any website interaction\n\
                 - send_message: send real-time messages to the user during tasks\n\
                 - send_file: send files to the user\n\n\
                 KEY RULES:\n\
                 - Shell output (stdout/stderr) is NOT visible to the user. Only YOUR \
                   final text reply and send_message calls reach the user.\n\
                 - To send real-time updates during multi-step work, call send_message \
                   each round. You can do this repeatedly — each call delivers instantly.\n\
                 - For timed/periodic tasks (e.g. send something every N seconds), use \
                   shell(\"sleep N\") then send_message in a loop across tool rounds.\n\
                 - When asked to visit a website, open a page, or interact with a web app, \
                   USE THE BROWSER TOOL. Do not refuse or explain why you can't — just do it.\n\
                 - After finishing browser work, call browser with action 'close' to shut it down.\n\
                 - Reply in the same language the user writes in.\n\
                 - Be concise. No emoji unless the user uses them.\n\
                 - NEVER give up on a task by explaining limitations. You have a multi-round \
                   tool loop — keep calling tools until the task is done or you hit a real \
                   error. Do not stop early to explain what you 'cannot' do."
                    .to_string(),
            );

            // Create agent runtime
            let model = config.provider.model.clone()
                .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
            let agent = Arc::new(skyclaw_agent::AgentRuntime::with_limits(
                provider.clone(),
                memory.clone(),
                tools,
                model.clone(),
                system_prompt,
                config.agent.max_turns,
                config.agent.max_context_tokens,
                config.agent.max_tool_rounds,
            ));

            // Unified message channel — all sources (Telegram, heartbeat, future
            // channels) feed into a single processing loop.
            let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<skyclaw_core::types::message::InboundMessage>(32);

            // Wire Telegram messages into the unified channel
            if let Some(mut tg_rx) = tg_rx {
                let tx = msg_tx.clone();
                tokio::spawn(async move {
                    while let Some(msg) = tg_rx.recv().await {
                        if tx.send(msg).await.is_err() { break; }
                    }
                });
            }

            // Start heartbeat if enabled
            let workspace_path = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".skyclaw")
                .join("workspace");
            std::fs::create_dir_all(&workspace_path).ok();

            if config.heartbeat.enabled {
                let heartbeat_chat_id = config.heartbeat.report_to.clone()
                    .unwrap_or_else(|| "heartbeat".to_string());
                let runner = skyclaw_automation::HeartbeatRunner::new(
                    config.heartbeat.clone(),
                    workspace_path.clone(),
                    heartbeat_chat_id,
                );
                let hb_tx = msg_tx.clone();
                tokio::spawn(async move {
                    runner.run(hb_tx).await;
                });
                tracing::info!(
                    interval = %config.heartbeat.interval,
                    checklist = %config.heartbeat.checklist,
                    "Heartbeat runner started"
                );
            }

            // ── Stop-command detection ───────────────────────────
            // Hardcoded keywords so "stop" is instant and deterministic —
            // no LLM round-trip, zero tokens.
            fn is_stop_command(text: &str) -> bool {
                let t = text.trim().to_lowercase();
                // ── Single-word exact matches ──────────────────────
                const STOP_WORDS: &[&str] = &[
                    // English
                    "stop", "cancel", "abort", "quit", "halt", "enough",
                    // Vietnamese (with and without diacritics)
                    "dừng", "dung", "thôi", "thoi", "ngừng", "ngung",
                    "hủy", "huy", "dẹp", "dep",
                    // Spanish
                    "para", "detente", "basta", "cancela", "alto",
                    // French
                    "arrête", "arrete", "arrêter", "arreter", "annuler", "suffit",
                    // German
                    "stopp", "aufhören", "aufhoren", "abbrechen", "genug",
                    // Portuguese
                    "pare", "parar", "cancele", "cancelar", "chega",
                    // Italian
                    "ferma", "fermati", "basta", "annulla", "smettila",
                    // Russian
                    "стоп", "стой", "хватит", "отмена", "довольно",
                    // Japanese
                    "止めて", "やめて", "やめろ", "ストップ", "止め", "やめ",
                    // Korean
                    "멈춰", "그만", "중지", "취소", "됐어",
                    // Chinese
                    "停", "停止", "取消", "别说了", "够了", "算了",
                    // Arabic
                    "توقف", "الغاء", "كفى", "قف",
                    // Thai
                    "หยุด", "ยกเลิก", "พอ", "เลิก",
                    // Indonesian / Malay
                    "berhenti", "hentikan", "batalkan", "cukup", "sudah",
                    // Hindi (Devanagari + transliterated)
                    "रुको", "बंद", "रद्द", "बस", "ruko", "bas",
                    // Turkish
                    "dur", "durdur", "iptal", "yeter",
                ];

                if STOP_WORDS.contains(&t.as_str()) {
                    return true;
                }

                // ── Short phrases (≤60 chars) ──────────────────────
                // Keeps false positives low — normal long messages that
                // happen to contain "stop" won't trigger.
                if t.len() <= 60 {
                    const STOP_PHRASES: &[&str] = &[
                        // English
                        "stop it", "stop that", "please stop", "stop now",
                        "cancel that", "shut up",
                        // Vietnamese
                        "dừng lại", "dung lai", "thôi đi", "thoi di",
                        "dừng đi", "dung di", "ngừng lại", "ngung lai",
                        "dung viet", "dừng viết", "thoi dung", "thôi dừng",
                        "đừng nói nữa", "dung noi nua",
                        "im đi", "im di",
                        // Spanish
                        "para ya", "deja de",
                        // French
                        "arrête ça", "arrete ca",
                        // German
                        "hör auf", "hor auf",
                        // Japanese
                        "止めてください", "やめてください",
                        // Chinese
                        "停下来", "不要说了", "别说了",
                        // Korean
                        "그만해", "멈춰줘",
                    ];

                    for phrase in STOP_PHRASES {
                        if t.contains(phrase) {
                            return true;
                        }
                    }
                }

                false
            }

            // Per-chat serial executor with priority preemption.
            //
            // Each chat_id gets its own mpsc channel so messages for the same
            // chat are processed one at a time (serialization). When a user
            // message arrives while a heartbeat task is running, the heartbeat
            // is interrupted via an AtomicBool flag so the user gets a fast
            // response.
            //
            // State per chat:
            //   - tx: sender into that chat's dedicated task queue
            //   - interrupt: flag to preempt the currently running task
            //   - is_heartbeat: whether the active task is a heartbeat (only
            //     heartbeat tasks are interruptible by user messages)

            /// Tracks the active task state for a single chat.
            struct ChatSlot {
                tx: tokio::sync::mpsc::Sender<skyclaw_core::types::message::InboundMessage>,
                interrupt: Arc<AtomicBool>,
                is_heartbeat: Arc<AtomicBool>,
            }

            if let Some(sender) = primary_channel.clone() {
                let agent_clone = agent.clone();
                let ws_path = workspace_path.clone();
                let pending_clone = pending_messages.clone();

                // Chat dispatch table — maps chat_id to its dedicated worker.
                let chat_slots: Arc<Mutex<HashMap<String, ChatSlot>>> =
                    Arc::new(Mutex::new(HashMap::new()));

                tokio::spawn(async move {
                    while let Some(inbound) = msg_rx.recv().await {
                        let chat_id = inbound.chat_id.clone();
                        let is_heartbeat_msg = inbound.channel == "heartbeat";

                        let mut slots = chat_slots.lock().await;

                        // If a user message arrives while ANY task is active,
                        // decide how to handle it based on content.
                        if !is_heartbeat_msg {
                            if let Some(slot) = slots.get(&chat_id) {
                                // Always interrupt heartbeat tasks immediately
                                if slot.is_heartbeat.load(Ordering::Relaxed) {
                                    tracing::info!(
                                        chat_id = %chat_id,
                                        "User message preempting active heartbeat task"
                                    );
                                    slot.interrupt.store(true, Ordering::Relaxed);
                                }

                                // Stop command → set interrupt flag, don't queue.
                                // The runtime exits the tool loop on the next round.
                                let is_stop = inbound.text.as_deref()
                                    .map(is_stop_command)
                                    .unwrap_or(false);

                                if is_stop {
                                    tracing::info!(
                                        chat_id = %chat_id,
                                        "Stop command detected — interrupting active task"
                                    );
                                    slot.interrupt.store(true, Ordering::Relaxed);
                                    continue; // don't queue this message
                                }

                                // Normal message → push to pending queue so the
                                // runtime injects it into tool results.
                                if let Some(text) = inbound.text.as_deref() {
                                    if let Ok(mut pq) = pending_clone.lock() {
                                        pq.entry(chat_id.clone())
                                            .or_default()
                                            .push(text.to_string());
                                    }
                                }
                            }
                        }

                        // If a heartbeat arrives while the chat is busy, skip
                        // it — the agent is already occupied with that chat.
                        if is_heartbeat_msg {
                            if let Some(slot) = slots.get(&chat_id) {
                                // Channel full = worker is still busy
                                if slot.tx.try_send(inbound).is_err() {
                                    tracing::debug!(
                                        chat_id = %chat_id,
                                        "Skipping heartbeat — chat is busy"
                                    );
                                }
                                continue;
                            }
                        }

                        // Ensure a worker exists for this chat_id.
                        let slot = slots.entry(chat_id.clone()).or_insert_with(|| {
                            // Bounded channel (4 deep) per chat — backpressure
                            // prevents unbounded queue growth.
                            let (chat_tx, mut chat_rx) =
                                tokio::sync::mpsc::channel::<skyclaw_core::types::message::InboundMessage>(4);

                            let interrupt = Arc::new(AtomicBool::new(false));
                            let is_heartbeat = Arc::new(AtomicBool::new(false));

                            let agent = agent_clone.clone();
                            let sender = sender.clone();
                            let workspace_path = ws_path.clone();
                            let interrupt_clone = interrupt.clone();
                            let is_heartbeat_clone = is_heartbeat.clone();
                            let pending_for_worker = pending_clone.clone();
                            let worker_chat_id = chat_id.clone();

                            tokio::spawn(async move {
                                while let Some(mut msg) = chat_rx.recv().await {
                                    let is_hb = msg.channel == "heartbeat";
                                    is_heartbeat_clone.store(is_hb, Ordering::Relaxed);
                                    interrupt_clone.store(false, Ordering::Relaxed);

                                    // All tasks get an interrupt flag — heartbeat
                                    // tasks are interrupted immediately by user
                                    // messages; user tasks see pending messages
                                    // injected into their tool results.
                                    let interrupt_flag = Some(interrupt_clone.clone());

                                    // Download attachments
                                    if !msg.attachments.is_empty() {
                                        if let Some(ft) = sender.file_transfer() {
                                            match ft.receive_file(&msg).await {
                                                Ok(files) => {
                                                    let mut file_notes = Vec::new();
                                                    for file in &files {
                                                        let save_path = workspace_path.join(&file.name);
                                                        if let Err(e) = tokio::fs::write(&save_path, &file.data).await {
                                                            tracing::error!(error = %e, file = %file.name, "Failed to save attachment");
                                                        } else {
                                                            tracing::info!(file = %file.name, size = file.size, "Saved attachment to workspace");
                                                            file_notes.push(format!(
                                                                "[File received: {} ({}, {} bytes) — saved to workspace/{}]",
                                                                file.name, file.mime_type, file.size, file.name
                                                            ));
                                                        }
                                                    }
                                                    if !file_notes.is_empty() {
                                                        let prefix = file_notes.join("\n");
                                                        let existing = msg.text.take().unwrap_or_default();
                                                        msg.text = Some(format!("{}\n{}", prefix, existing));
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::error!(error = %e, "Failed to download attachments");
                                                }
                                            }
                                        }
                                    }

                                    let mut session = skyclaw_core::types::session::SessionContext {
                                        session_id: format!("{}-{}", msg.channel, msg.chat_id),
                                        user_id: msg.user_id.clone(),
                                        channel: msg.channel.clone(),
                                        chat_id: msg.chat_id.clone(),
                                        history: Vec::new(),
                                        workspace_path: workspace_path.clone(),
                                    };

                                    match agent.process_message(&msg, &mut session, interrupt_flag, Some(pending_for_worker.clone())).await {
                                        Ok(reply) => {
                                            if let Err(e) = sender.send_message(reply).await {
                                                tracing::error!(error = %e, "Failed to send reply");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!(error = %e, "Agent processing error");
                                            let error_reply = skyclaw_core::types::message::OutboundMessage {
                                                chat_id: msg.chat_id.clone(),
                                                text: format!("Error: {}", e),
                                                reply_to: Some(msg.id.clone()),
                                                parse_mode: None,
                                            };
                                            let _ = sender.send_message(error_reply).await;
                                        }
                                    }

                                    // Clear active state and pending queue
                                    is_heartbeat_clone.store(false, Ordering::Relaxed);
                                    interrupt_clone.store(false, Ordering::Relaxed);
                                    if let Ok(mut pq) = pending_for_worker.lock() {
                                        pq.remove(&worker_chat_id);
                                    }
                                }
                            });

                            ChatSlot { tx: chat_tx, interrupt, is_heartbeat }
                        });

                        // Send message into the chat's dedicated queue.
                        // For user messages we use `send` (wait for slot) so
                        // they are never dropped. For heartbeat we already
                        // used try_send above.
                        if !is_heartbeat_msg {
                            if let Err(e) = slot.tx.send(inbound).await {
                                tracing::error!(error = %e, "Chat worker closed unexpectedly");
                            }
                        }
                    }
                });
            }

            // Start the gateway server
            let gate = skyclaw_gateway::SkyGate::new(
                channels,
                agent,
                config.gateway.clone(),
            );

            println!("SkyClaw gateway starting...");
            println!("  Mode: {}", cli.mode);
            println!("  GUI: {}", gui);
            println!("  Gateway: http://{}:{}", config.gateway.host, config.gateway.port);
            println!("  Health: http://{}:{}/health", config.gateway.host, config.gateway.port);

            gate.start().await?;
        }
        Commands::Chat => {
            println!("SkyClaw interactive chat");
            println!("Type 'exit' to quit.");
            // TODO: Start CLI channel directly
        }
        Commands::Status => {
            println!("SkyClaw Status");
            println!("  Mode: {}", config.skyclaw.mode);
            println!("  Gateway: {}:{}", config.gateway.host, config.gateway.port);
            println!("  Provider: {}", config.provider.name.as_deref().unwrap_or("not configured"));
            println!("  Memory: {}", config.memory.backend);
            println!("  Vault: {}", config.vault.backend);
        }
        Commands::Skill { command } => match command {
            SkillCommands::List => {
                println!("Installed skills:");
                // TODO: List skills from registry
            }
            SkillCommands::Info { name } => {
                println!("Skill info: {}", name);
                // TODO: Show skill details
            }
            SkillCommands::Install { path } => {
                println!("Installing skill from: {}", path);
                // TODO: Install skill
            }
        },
        Commands::Config { command } => match command {
            ConfigCommands::Validate => {
                println!("Configuration valid.");
                println!("  Gateway: {}:{}", config.gateway.host, config.gateway.port);
                println!("  Provider: {}", config.provider.name.as_deref().unwrap_or("none"));
                println!("  Memory backend: {}", config.memory.backend);
                println!("  Channels: {}", config.channel.len());
            }
            ConfigCommands::Show => {
                let output = toml::to_string_pretty(&config)?;
                println!("{}", output);
            }
        },
        Commands::Migrate { from, path } => {
            println!("Migrating from {} at {}", from, path);
            // TODO: Run migration
        }
        Commands::Version => {
            println!("skyclaw {}", env!("CARGO_PKG_VERSION"));
            println!("Cloud-native Rust AI agent runtime");
        }
    }

    Ok(())
}
