//! Anthropic Messages API client with multi-tool support and SSE streaming.
//!
//! Iterates [`tool_schema::all_tools`] to advertise every Tier-1 tool, then
//! dispatches `tool_use` responses back through [`tool_schema::parse_input`]
//! so each tool's proto payload is constructed in one place. The streaming
//! variant `call_streaming` returns a [`super::DriverChunkStream`] of provider-
//! neutral chunks; the legacy `call` is now a thin aggregator-shim wrapper so
//! existing 36 tests assert on the unchanged `DriverOutput` shape.

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use futures::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event as SseEvent, EventSource, RequestBuilderExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::decode::NormalizedTurn;
use super::tool_schema;
use super::{DecodedBlock, DriverChunkStream, DriverOutput, DriverStreamChunk};
use crate::server::direct_backend::common::truncate_for_log;
use crate::server::direct_backend::ResolvedProvider;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

const SYSTEM_PROMPT: &str = "You are a helpful coding assistant embedded in the Warp terminal. \
     The user is working in a software project on their local machine. \
     Use the provided tools to read files, search the codebase, run commands, \
     or apply edits as needed — do not guess. Reply with concise, \
     terminal-friendly markdown when no tool call is required.";

/// Aggregator-shim entry. Drives `call_streaming` to completion and folds
/// the chunk stream back into the legacy `DriverOutput` shape. Existing
/// non-streaming callers + the 36 baseline tests use this path unchanged.
pub async fn call(
    provider: &ResolvedProvider,
    turns: &[NormalizedTurn],
) -> anyhow::Result<DriverOutput> {
    let stream = call_streaming(provider, turns, None).await?;
    super::aggregate_stream_to_output(stream).await
}

/// Open an SSE stream against the Anthropic Messages API and return a
/// provider-neutral [`DriverChunkStream`]. Caller is the multi-agent adapter
/// (or `aggregate_stream_to_output` in legacy mode).
pub async fn call_streaming(
    provider: &ResolvedProvider,
    turns: &[NormalizedTurn],
    mcp: Option<&warp_multi_agent_api::request::McpContext>,
) -> anyhow::Result<DriverChunkStream> {
    if turns.is_empty() {
        return Err(anyhow!("DirectBackend Anthropic agent: no turns to send"));
    }

    let messages = project_messages(turns);
    let advertised = tool_schema::advertised_tools(mcp);
    let tools: Vec<ToolSpec> = advertised
        .iter()
        .map(|kind| {
            let schema_json: Value = serde_json::from_str(tool_schema::schema(*kind))
                .context("tool schema must parse")?;
            Ok(ToolSpec {
                name: tool_schema::name(*kind),
                description: tool_schema::description(*kind),
                input_schema: schema_json,
            })
        })
        .collect::<anyhow::Result<_>>()?;

    let system = super::compose_system_prompt(SYSTEM_PROMPT, mcp);
    let body = AnthropicRequest {
        model: &provider.model_id,
        max_tokens: DEFAULT_MAX_TOKENS,
        system: &system,
        messages,
        tools,
        stream: true,
    };

    let url = format!("{}/v1/messages", provider.base_url);
    let client = Client::builder()
        .timeout(super::HTTP_TIMEOUT)
        .build()
        .context("Anthropic agent client init")?;
    let req = client
        .post(&url)
        .header("x-api-key", &provider.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("accept", "text/event-stream")
        .json(&body);
    let event_source = req
        .eventsource()
        .context("Anthropic SSE init")?;

    let model_id = provider.model_id.clone();
    let state = StreamState {
        es: event_source,
        accs: HashMap::new(),
        model_id,
        finished: false,
        msg_input_tokens: None,
        msg_input_cache_read: None,
        delta_output_tokens: None,
        stop_reason: None,
    };
    let s = futures::stream::unfold(state, |mut st| async move {
        if st.finished {
            return None;
        }
        loop {
            match st.es.next().await {
                None => {
                    // Underlying stream closed without `message_stop`.
                    if !st.accs.is_empty() {
                        log::warn!(
                            "Anthropic SSE closed with {} unfinished tool_use block(s); discarding",
                            st.accs.len()
                        );
                    }
                    st.finished = true;
                    return Some((Ok(make_stop_chunk(&st)), st));
                }
                Some(Err(e)) => {
                    if matches!(e, reqwest_eventsource::Error::StreamEnded) {
                        if !st.accs.is_empty() {
                            log::warn!(
                                "Anthropic SSE ended with {} unfinished tool_use block(s); discarding",
                                st.accs.len()
                            );
                        }
                        st.finished = true;
                        return Some((Ok(make_stop_chunk(&st)), st));
                    }
                    st.finished = true;
                    let msg = format!("Anthropic SSE: {e}");
                    return Some((Err(anyhow!(msg)), st));
                }
                Some(Ok(SseEvent::Open)) => continue,
                Some(Ok(SseEvent::Message(m))) => {
                    let outcome = parse_anthropic_event(&m.data, &mut st);
                    match outcome {
                        ParseOutcome::Emit(chunk) => return Some((Ok(chunk), st)),
                        ParseOutcome::Continue => continue,
                        ParseOutcome::End => {
                            st.finished = true;
                            return Some((Ok(make_stop_chunk(&st)), st));
                        }
                        ParseOutcome::Err(e) => {
                            st.finished = true;
                            return Some((Err(e), st));
                        }
                    }
                }
            }
        }
    });

    cfg_if::cfg_if! {
        if #[cfg(target_family = "wasm")] {
            Ok(s.boxed_local())
        } else {
            Ok(s.boxed())
        }
    }
}

/// Per-tool-block JSON accumulator. Anthropic streams `input_json_delta`
/// chunks of the function arguments JSON; we buffer until `content_block_stop`
/// then parse once.
struct ToolUseAccumulator {
    id: String,
    name: String,
    json_buf: String,
}

struct StreamState {
    es: EventSource,
    accs: HashMap<u32, ToolUseAccumulator>,
    model_id: String,
    finished: bool,
    msg_input_tokens: Option<u32>,
    msg_input_cache_read: Option<u32>,
    delta_output_tokens: Option<u32>,
    stop_reason: Option<String>,
}

enum ParseOutcome {
    Emit(DriverStreamChunk),
    Continue,
    End,
    Err(anyhow::Error),
}

fn make_stop_chunk(st: &StreamState) -> DriverStreamChunk {
    DriverStreamChunk::Stop {
        stop_reason: st.stop_reason.clone(),
        input_tokens: st.msg_input_tokens,
        output_tokens: st.delta_output_tokens,
        input_cache_read: st.msg_input_cache_read,
        model_id: st.model_id.clone(),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseEnvelope {
    MessageStart {
        message: MessageStartBody,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockBody,
    },
    ContentBlockDelta {
        index: u32,
        delta: BlockDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        #[serde(default)]
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: Option<UsageBlock>,
    },
    MessageStop,
    Ping,
    Error {
        error: SseError,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageStartBody {
    #[serde(default)]
    usage: Option<UsageBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockBody {
    Text {
        #[serde(default)]
        #[allow(dead_code)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        #[allow(dead_code)]
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct MessageDeltaBody {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct SseError {
    #[serde(default)]
    message: String,
}

fn parse_anthropic_event(data: &str, st: &mut StreamState) -> ParseOutcome {
    let env: SseEnvelope = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Anthropic SSE: skipping malformed event: {e}");
            return ParseOutcome::Continue;
        }
    };
    match env {
        SseEnvelope::MessageStart { message } => {
            if let Some(u) = message.usage {
                st.msg_input_tokens = u.input_tokens;
                st.msg_input_cache_read = u.cache_read_input_tokens;
            }
            ParseOutcome::Continue
        }
        SseEnvelope::ContentBlockStart {
            index,
            content_block,
        } => match content_block {
            ContentBlockBody::Text { .. } => ParseOutcome::Continue,
            ContentBlockBody::ToolUse { id, name, .. } => {
                st.accs.insert(
                    index,
                    ToolUseAccumulator {
                        id: id.clone(),
                        name: name.clone(),
                        json_buf: String::new(),
                    },
                );
                ParseOutcome::Emit(DriverStreamChunk::ToolUseStart {
                    block_idx: index,
                    tool_use_id: id,
                    name,
                })
            }
            ContentBlockBody::Other => ParseOutcome::Continue,
        },
        SseEnvelope::ContentBlockDelta { index, delta } => match delta {
            BlockDelta::TextDelta { text } => {
                if text.is_empty() {
                    ParseOutcome::Continue
                } else {
                    ParseOutcome::Emit(DriverStreamChunk::TextDelta {
                        block_idx: index,
                        text,
                    })
                }
            }
            BlockDelta::InputJsonDelta { partial_json } => {
                if let Some(acc) = st.accs.get_mut(&index) {
                    acc.json_buf.push_str(&partial_json);
                }
                ParseOutcome::Continue
            }
            BlockDelta::Other => ParseOutcome::Continue,
        },
        SseEnvelope::ContentBlockStop { index } => {
            if let Some(acc) = st.accs.remove(&index) {
                let parsed_input: Value = if acc.json_buf.is_empty() {
                    Value::Object(Default::default())
                } else {
                    match serde_json::from_str(&acc.json_buf) {
                        Ok(v) => v,
                        Err(e) => {
                            return ParseOutcome::Emit(DriverStreamChunk::ToolUseSoftError {
                                message: format!(
                                    "[tool error] Your `{}` tool input wasn't valid JSON: {e}",
                                    acc.name
                                ),
                            })
                        }
                    }
                };
                ParseOutcome::Emit(DriverStreamChunk::ToolUseComplete {
                    block_idx: index,
                    tool_use_id: acc.id,
                    name: acc.name,
                    parsed_input,
                })
            } else {
                ParseOutcome::Continue
            }
        }
        SseEnvelope::MessageDelta { delta, usage } => {
            if let Some(sr) = delta.stop_reason {
                st.stop_reason = Some(sr);
            }
            if let Some(u) = usage {
                if u.output_tokens.is_some() {
                    st.delta_output_tokens = u.output_tokens;
                }
            }
            ParseOutcome::Continue
        }
        SseEnvelope::MessageStop => ParseOutcome::End,
        SseEnvelope::Ping => ParseOutcome::Continue,
        SseEnvelope::Error { error } => {
            ParseOutcome::Err(anyhow!("Anthropic SSE error: {}", truncate_for_log(&error.message, 256)))
        }
        SseEnvelope::Other => ParseOutcome::Continue,
    }
}

fn project_messages(turns: &[NormalizedTurn]) -> Vec<Value> {
    turns
        .iter()
        .map(|t| match t {
            NormalizedTurn::User { text, tool_results } => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for r in tool_results {
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": r.tool_use_id,
                        "content": r.content,
                        "is_error": r.is_error,
                    }));
                }
                if blocks.len() == 1 && text.is_some() && tool_results.is_empty() {
                    // Optimisation: plain text content can be a string.
                    json!({"role": "user", "content": text.as_deref().unwrap_or("")})
                } else {
                    json!({"role": "user", "content": blocks})
                }
            }
            NormalizedTurn::Assistant { text, tool_uses, reasoning: _ } => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for u in tool_uses {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": u.tool_use_id,
                        "name": tool_schema::name(u.tool_kind),
                        "input": u.input,
                    }));
                }
                if blocks.len() == 1 && text.is_some() && tool_uses.is_empty() {
                    json!({"role": "assistant", "content": text.as_deref().unwrap_or("")})
                } else {
                    json!({"role": "assistant", "content": blocks})
                }
            }
        })
        .collect()
}

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<Value>,
    tools: Vec<ToolSpec<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ToolSpec<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: Value,
}


#[derive(Deserialize)]
struct UsageBlock {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}
