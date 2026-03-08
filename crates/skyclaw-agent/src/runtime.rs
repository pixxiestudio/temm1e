//! AgentRuntime — main agent loop that processes messages through the
//! provider, executing tool calls in a loop until a final text reply.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use skyclaw_core::{Memory, Provider, Tool};
use skyclaw_core::types::error::SkyclawError;
use skyclaw_core::types::message::{
    ChatMessage, ContentPart, InboundMessage, MessageContent,
    OutboundMessage, ParseMode, Role,
};
use skyclaw_core::types::session::SessionContext;
use tracing::{debug, info, warn};

use crate::context::build_context;
use crate::executor::execute_tool;

/// Maximum characters per tool output (roughly ~8K tokens).
const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;

/// Shared pending-message queue (same type as skyclaw_tools::PendingMessages).
pub type PendingMessages = Arc<std::sync::Mutex<HashMap<String, Vec<String>>>>;

/// The core agent runtime. Holds references to the AI provider, memory backend,
/// and registered tools.
pub struct AgentRuntime {
    provider: Arc<dyn Provider>,
    memory: Arc<dyn Memory>,
    tools: Vec<Arc<dyn Tool>>,
    model: String,
    system_prompt: Option<String>,
    max_turns: usize,
    max_context_tokens: usize,
    max_tool_rounds: usize,
}

impl AgentRuntime {
    /// Create a new AgentRuntime.
    pub fn new(
        provider: Arc<dyn Provider>,
        memory: Arc<dyn Memory>,
        tools: Vec<Arc<dyn Tool>>,
        model: String,
        system_prompt: Option<String>,
    ) -> Self {
        Self {
            provider,
            memory,
            tools,
            model,
            system_prompt,
            max_turns: 6,
            max_context_tokens: 30_000,
            max_tool_rounds: 50,
        }
    }

    /// Create a new AgentRuntime with custom context limits.
    pub fn with_limits(
        provider: Arc<dyn Provider>,
        memory: Arc<dyn Memory>,
        tools: Vec<Arc<dyn Tool>>,
        model: String,
        system_prompt: Option<String>,
        max_turns: usize,
        max_context_tokens: usize,
        max_tool_rounds: usize,
    ) -> Self {
        Self {
            provider,
            memory,
            tools,
            model,
            system_prompt,
            max_turns,
            max_context_tokens,
            max_tool_rounds,
        }
    }

    /// Process an inbound message through the full agent loop.
    ///
    /// - `interrupt`: if set to `true` by another task, the tool loop exits
    ///   early so the dispatcher can serve a higher-priority message.
    /// - `pending`: shared queue of user messages that arrived while this task
    ///   is running. Pending texts are automatically appended to the last tool
    ///   result each round so the LLM sees them without extra API calls.
    pub async fn process_message(
        &self,
        msg: &InboundMessage,
        session: &mut SessionContext,
        interrupt: Option<Arc<AtomicBool>>,
        pending: Option<PendingMessages>,
    ) -> Result<OutboundMessage, SkyclawError> {
        info!(
            channel = %msg.channel,
            chat_id = %msg.chat_id,
            user_id = %msg.user_id,
            "Processing inbound message"
        );

        // Build user text — include attachment descriptions if no text provided
        let user_text = match (&msg.text, msg.attachments.is_empty()) {
            (Some(t), _) if !t.trim().is_empty() => t.clone(),
            (_, false) => {
                let descs: Vec<String> = msg.attachments.iter().map(|a| {
                    let name = a.file_name.as_deref().unwrap_or("file");
                    let mime = a.mime_type.as_deref().unwrap_or("unknown type");
                    format!("[Attached: {} ({})]", name, mime)
                }).collect();
                descs.join(" ")
            }
            _ => {
                return Ok(OutboundMessage {
                    chat_id: msg.chat_id.clone(),
                    text: "I received an empty message. Please send some text or a file.".to_string(),
                    reply_to: Some(msg.id.clone()),
                    parse_mode: None,
                });
            }
        };
        let detected_creds = skyclaw_vault::detect_credentials(&user_text);
        if !detected_creds.is_empty() {
            warn!(
                count = detected_creds.len(),
                "Detected credentials in user message — they will be noted but not stored in plain text history"
            );
            for cred in &detected_creds {
                debug!(
                    provider = %cred.provider,
                    key = %cred.key,
                    "Detected credential"
                );
            }
        }

        // Append the user message to session history
        session.history.push(ChatMessage {
            role: Role::User,
            content: MessageContent::Text(user_text),
        });

        // Tool-use loop
        let mut rounds = 0;
        let mut interrupted = false;
        loop {
            rounds += 1;

            // Check for preemption between rounds
            if let Some(ref flag) = interrupt {
                if flag.load(Ordering::Relaxed) {
                    info!("Agent interrupted by higher-priority message after {} rounds", rounds - 1);
                    interrupted = true;
                    break;
                }
            }

            if rounds > self.max_tool_rounds {
                warn!("Exceeded maximum tool rounds ({}), forcing text reply", self.max_tool_rounds);
                break;
            }

            // Build the completion request from full context
            let request = build_context(
                session,
                self.memory.as_ref(),
                &self.tools,
                &self.model,
                self.system_prompt.as_deref(),
                self.max_turns,
                self.max_context_tokens,
            )
            .await;

            debug!(round = rounds, messages = request.messages.len(), "Sending completion request");

            // Call the provider
            let response = self.provider.complete(request).await?;

            // Separate text content from tool-use content
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();

            for part in &response.content {
                match part {
                    ContentPart::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentPart::ToolUse { id, name, input } => {
                        tool_uses.push((id.clone(), name.clone(), input.clone()));
                    }
                    ContentPart::ToolResult { .. } => {
                        // Should not appear in provider response, ignore
                    }
                }
            }

            // If no tool calls, we have our final reply
            if tool_uses.is_empty() {
                let reply_text = text_parts.join("\n");

                // Record assistant reply in history
                session.history.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text(reply_text.clone()),
                });

                return Ok(OutboundMessage {
                    chat_id: msg.chat_id.clone(),
                    text: reply_text,
                    reply_to: Some(msg.id.clone()),
                    parse_mode: None,
                });
            }

            // Record the assistant message (with tool_use parts) in history
            session.history.push(ChatMessage {
                role: Role::Assistant,
                content: MessageContent::Parts(response.content.clone()),
            });

            // Execute each tool call and collect results
            let mut tool_result_parts: Vec<ContentPart> = Vec::new();

            for (tool_use_id, tool_name, arguments) in &tool_uses {
                info!(tool = %tool_name, id = %tool_use_id, "Executing tool call");

                let result = execute_tool(tool_name, arguments.clone(), &self.tools, session).await;

                let (content, is_error) = match result {
                    Ok(output) => {
                        let c = if output.content.len() > MAX_TOOL_OUTPUT_CHARS {
                            let truncated = &output.content[..MAX_TOOL_OUTPUT_CHARS];
                            format!("{}...\n\n[Output truncated — {} chars total]", truncated, output.content.len())
                        } else {
                            output.content
                        };
                        (c, output.is_error)
                    }
                    Err(e) => (format!("Tool execution error: {}", e), true),
                };

                tool_result_parts.push(ContentPart::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content,
                    is_error,
                });
            }

            // Inject pending user messages into the last tool result so the
            // LLM sees them without any extra API call or tool invocation.
            if let Some(ref pq) = pending {
                if let Ok(mut map) = pq.lock() {
                    if let Some(msgs) = map.remove(&msg.chat_id) {
                        if !msgs.is_empty() {
                            info!(
                                count = msgs.len(),
                                chat_id = %msg.chat_id,
                                "Injecting pending user messages into tool results"
                            );
                            let notice = format!(
                                "\n\n---\n[PENDING MESSAGES — the user sent new message(s) while you were working. \
                                 Acknowledge with send_message and decide: finish current task or stop and respond.]\n{}",
                                msgs.iter()
                                    .enumerate()
                                    .map(|(i, t)| format!("  {}. \"{}\"", i + 1, t))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            );
                            // Append to last tool result
                            if let Some(ContentPart::ToolResult { content, .. }) =
                                tool_result_parts.last_mut()
                            {
                                content.push_str(&notice);
                            }
                        }
                    }
                }
            }

            // Append tool results as a Tool message in history
            session.history.push(ChatMessage {
                role: Role::Tool,
                content: MessageContent::Parts(tool_result_parts),
            });

            // Continue the loop — provider will see the tool results and may
            // issue more tool calls or produce a final text reply.
        }

        // Fallback: exited loop due to interruption or max rounds
        let text = if interrupted {
            "I was interrupted to handle a new message. I'll pick up where I left off if needed.".to_string()
        } else {
            "I reached the maximum number of tool execution steps. Here is what I have so far. Please let me know if you need me to continue.".to_string()
        };

        Ok(OutboundMessage {
            chat_id: msg.chat_id.clone(),
            text,
            reply_to: Some(msg.id.clone()),
            parse_mode: Some(ParseMode::Plain),
        })
    }

    /// Get a reference to the provider.
    pub fn provider(&self) -> &dyn Provider {
        self.provider.as_ref()
    }

    /// Get a reference to the memory backend.
    pub fn memory(&self) -> &dyn Memory {
        self.memory.as_ref()
    }

    /// Get the registered tools.
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }
}
