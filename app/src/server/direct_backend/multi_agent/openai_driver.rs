//! OpenAI Chat Completions driver with multi-tool support and SSE streaming.
//!
//! Same wire shape powers OpenAI-compatible gateways (vLLM, LiteLLM, DeepSeek,
//! Qwen, Ollama, OpenRouter). Only `base_url` differs. Streaming uses
//! `stream: true` + `stream_options.include_usage: true` so the final chunk
//! carries `usage` for token accounting.

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use futures::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event as SseEvent, RequestBuilderExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::decode::NormalizedTurn;
use super::tool_schema;
use super::{DecodedBlock, DriverChunkStream, DriverOutput, DriverStreamChunk};
use crate::server::direct_backend::common::truncate_for_log;
use crate::server::direct_backend::{DirectProviderKind, ResolvedProvider};

const SYSTEM_PROMPT: &str = "You are a helpful coding assistant embedded in the Warp terminal. \
     The user is working in a software project on their local machine. \
     Use the provided tools to read files, search the codebase, run commands, \
     or apply edits as needed — do not guess. Reply with concise, \
     terminal-friendly markdown when no tool call is required.";

pub async fn call(
    provider: &ResolvedProvider,
    turns: &[NormalizedTurn],
) -> anyhow::Result<DriverOutput> {
    let stream = call_streaming(provider, turns, None).await?;
    super::aggregate_stream_to_output(stream).await
}

pub async fn call_streaming(
    provider: &ResolvedProvider,
    turns: &[NormalizedTurn],
    mcp: Option<&warp_multi_agent_api::request::McpContext>,
) -> anyhow::Result<DriverChunkStream> {
    if turns.is_empty() {
        return Err(anyhow!("DirectBackend OpenAI agent: no turns to send"));
    }

    let system = super::compose_system_prompt(SYSTEM_PROMPT, mcp);
    let messages = project_messages(turns, &system);
    let advertised = tool_schema::advertised_tools(mcp);
    let tools: Vec<ToolSpec> = advertised
        .iter()
        .map(|kind| {
            let parameters: Value = serde_json::from_str(tool_schema::schema(*kind))
                .context("tool schema must parse")?;
            Ok(ToolSpec {
                r#type: "function",
                function: FunctionSpec {
                    name: tool_schema::name(*kind),
                    description: tool_schema::description(*kind),
                    parameters,
                },
            })
        })
        .collect::<anyhow::Result<_>>()?;

    // `stream_options.include_usage` is OpenAI-specific. Some compatible
    // gateways (Ollama, certain LiteLLM versions) reject the unknown field
    // outright, so we only send it for the canonical OpenAI provider.
    let stream_options = match provider.kind {
        DirectProviderKind::OpenAi => Some(StreamOptions {
            include_usage: true,
        }),
        _ => None,
    };
    let body = ChatRequest {
        model: &provider.model_id,
        messages,
        tools,
        tool_choice: "auto",
        stream: true,
        stream_options,
    };

    let url = format!("{}/v1/chat/completions", provider.base_url);
    let client = Client::builder()
        .timeout(super::HTTP_TIMEOUT)
        .build()
        .context("OpenAI agent client init")?;
    let req = client
        .post(&url)
        .bearer_auth(&provider.api_key)
        .header("accept", "text/event-stream")
        .json(&body);
    log::info!(
        "DirectBackend OpenAI: opening SSE url={} model={} messages={} tools={}",
        url, provider.model_id, body.messages.len(), body.tools.len(),
    );
    let event_source = req.eventsource().context("OpenAI SSE init")?;

    let model_id = provider.model_id.clone();
    let state = StreamState {
        es: event_source,
        accs: HashMap::new(),
        pending_tools: std::collections::VecDeque::new(),
        finished: false,
        input_tokens: None,
        output_tokens: None,
        stop_reason: None,
        model_id,
        chunk_count: 0,
        first_chunk_logged: false,
    };
    let s = futures::stream::unfold(state, |mut st| async move {
        if st.finished {
            return None;
        }
        loop {
            // First drain any tool-completion events queued on the previous chunk.
            if let Some(emit) = st.pending_tools.pop_front() {
                return Some((Ok(emit), st));
            }
            match st.es.next().await {
                None => {
                    log::info!(
                        "DirectBackend OpenAI: SSE closed gracefully after {} chunks (stop_reason={:?})",
                        st.chunk_count, st.stop_reason,
                    );
                    drain_and_finish(&mut st);
                    if let Some(first) = st.pending_tools.pop_front() {
                        return Some((Ok(first), st));
                    }
                    return None;
                }
                Some(Err(e)) => {
                    if matches!(e, reqwest_eventsource::Error::StreamEnded) {
                        drain_and_finish(&mut st);
                        if let Some(first) = st.pending_tools.pop_front() {
                            return Some((Ok(first), st));
                        }
                        return None;
                    }
                    st.finished = true;
                    let detail = describe_sse_error(e).await;
                    return Some((Err(anyhow!("OpenAI SSE: {detail}")), st));
                }
                Some(Ok(SseEvent::Open)) => {
                    log::info!("DirectBackend OpenAI: SSE Open event received");
                    continue;
                }
                Some(Ok(SseEvent::Message(m))) => {
                    if !st.first_chunk_logged {
                        st.first_chunk_logged = true;
                        log::info!(
                            "DirectBackend OpenAI: first SSE chunk received (data_len={})",
                            m.data.len(),
                        );
                    }
                    st.chunk_count += 1;
                    if m.data.trim() == "[DONE]" {
                        // Drain any half-buffered tool calls before closing —
                        // some gateways send [DONE] before a per-choice
                        // finish_reason, which would otherwise discard them.
                        drain_and_finish(&mut st);
                        if let Some(first) = st.pending_tools.pop_front() {
                            return Some((Ok(first), st));
                        }
                        return None;
                    }
                    let outcomes = parse_openai_chunk(&m.data, &mut st);
                    if outcomes.is_empty() {
                        continue;
                    }
                    let mut iter = outcomes.into_iter();
                    let first = iter.next().unwrap();
                    for extra in iter {
                        st.pending_tools.push_back(extra);
                    }
                    return Some((Ok(first), st));
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

struct ToolCallAccumulator {
    id: String,
    name: String,
    args_buf: String,
    started: bool,
}

struct StreamState {
    es: reqwest_eventsource::EventSource,
    /// Keyed by `tool_calls[*].index`. OpenAI delta chunks reference parallel
    /// tool calls by their array index, only sending `id` + `name` once.
    accs: HashMap<i32, ToolCallAccumulator>,
    /// FIFO of tool-completion / Stop chunks generated on a single SSE event
    /// (or during stream-end drain) so we can emit them one-per-poll.
    pending_tools: std::collections::VecDeque<DriverStreamChunk>,
    finished: bool,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: Option<String>,
    model_id: String,
    chunk_count: u32,
    first_chunk_logged: bool,
}

/// Drain any half-buffered tool calls into `pending_tools` and append the
/// terminal `Stop`. Marks the stream finished. Used for [DONE] sentinel,
/// graceful EOF, and `Error::StreamEnded` — none of which guarantee a
/// per-choice `finish_reason` arrived before the close.
fn drain_and_finish(st: &mut StreamState) {
    let mut drained: Vec<_> = st.accs.drain().collect();
    drained.sort_by_key(|(idx, _)| *idx);
    for (idx, acc) in drained {
        let parsed_input: serde_json::Value = if acc.args_buf.is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            match serde_json::from_str(&acc.args_buf) {
                Ok(v) => v,
                Err(e) => {
                    st.pending_tools.push_back(DriverStreamChunk::ToolUseSoftError {
                        message: format!(
                            "[tool error] Your `{}` tool arguments weren't valid JSON: {e}",
                            acc.name
                        ),
                    });
                    continue;
                }
            }
        };
        st.pending_tools.push_back(DriverStreamChunk::ToolUseComplete {
            block_idx: idx as u32,
            tool_use_id: acc.id,
            name: acc.name,
            parsed_input,
        });
    }
    st.pending_tools.push_back(DriverStreamChunk::Stop {
        stop_reason: st.stop_reason.clone(),
        input_tokens: st.input_tokens,
        output_tokens: st.output_tokens,
        input_cache_read: None,
        model_id: st.model_id.clone(),
    });
    st.finished = true;
}

/// Surface the gateway's response body for `400/401/4xx/5xx`. Without this
/// the user only sees `Invalid status code: 400 Bad Request`, which masks
/// useful provider feedback like "model X does not support tools" or
/// "stream_options not allowed". Body is truncated to 500 chars to keep the
/// in-app error block readable.
async fn describe_sse_error(e: reqwest_eventsource::Error) -> String {
    match e {
        reqwest_eventsource::Error::InvalidStatusCode(code, resp) => {
            let body = resp.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(500).collect();
            if truncated.is_empty() {
                format!("HTTP {code}")
            } else {
                format!("HTTP {code}: {truncated}")
            }
        }
        reqwest_eventsource::Error::InvalidContentType(ct, resp) => {
            let body = resp.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(500).collect();
            format!("invalid content-type {ct:?}: {truncated}")
        }
        other => format!("{other}"),
    }
}

#[derive(Deserialize)]
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<UsageBlock>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek-R1 / o1-style reasoning models emit chain-of-thought tokens
    /// here instead of `content`. Without this field the entire reply would
    /// be silently dropped because every chunk's `content` is `null`.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: i32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn parse_openai_chunk(data: &str, st: &mut StreamState) -> Vec<DriverStreamChunk> {
    let chunk: OpenAiChunk = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("OpenAI SSE: skipping malformed chunk: {e}");
            return vec![];
        }
    };
    if let Some(u) = chunk.usage {
        if u.prompt_tokens.is_some() {
            st.input_tokens = u.prompt_tokens;
        }
        if u.completion_tokens.is_some() {
            st.output_tokens = u.completion_tokens;
        }
    }
    let mut out: Vec<DriverStreamChunk> = Vec::new();
    for choice in chunk.choices {
        if let Some(text) = choice.delta.content {
            if !text.is_empty() {
                // OpenAI never emits parallel text streams: block_idx = 0.
                out.push(DriverStreamChunk::TextDelta {
                    block_idx: 0,
                    text,
                });
            }
        }
        if let Some(text) = choice.delta.reasoning_content {
            if !text.is_empty() {
                out.push(DriverStreamChunk::ReasoningDelta {
                    block_idx: 0,
                    text,
                });
            }
        }
        for tc in choice.delta.tool_calls {
            let acc = st.accs.entry(tc.index).or_insert_with(|| ToolCallAccumulator {
                id: String::new(),
                name: String::new(),
                args_buf: String::new(),
                started: false,
            });
            if let Some(id) = tc.id {
                if !id.is_empty() {
                    acc.id = id;
                }
            }
            if let Some(f) = tc.function {
                if let Some(name) = f.name {
                    if !name.is_empty() {
                        acc.name = name;
                    }
                }
                if let Some(args) = f.arguments {
                    acc.args_buf.push_str(&args);
                }
            }
            // Emit ToolUseStart once we have BOTH id and name (OpenAI typically
            // sends them in the same first chunk, but be defensive).
            if !acc.started && !acc.id.is_empty() && !acc.name.is_empty() {
                acc.started = true;
                out.push(DriverStreamChunk::ToolUseStart {
                    block_idx: tc.index as u32,
                    tool_use_id: acc.id.clone(),
                    name: acc.name.clone(),
                });
            }
        }
        if let Some(fr) = choice.finish_reason {
            // Drain accumulators on terminal finish_reason.
            st.stop_reason = Some(fr.clone());
            if matches!(fr.as_str(), "tool_calls" | "stop" | "length" | "content_filter") {
                let drained: Vec<_> = st.accs.drain().collect();
                let mut sorted = drained;
                sorted.sort_by_key(|(idx, _)| *idx);
                for (idx, acc) in sorted {
                    let parsed_input: Value = if acc.args_buf.is_empty() {
                        Value::Object(Default::default())
                    } else {
                        match serde_json::from_str(&acc.args_buf) {
                            Ok(v) => v,
                            Err(e) => {
                                out.push(DriverStreamChunk::ToolUseSoftError {
                                    message: format!(
                                        "[tool error] Your `{}` tool arguments weren't valid JSON: {e}",
                                        acc.name
                                    ),
                                });
                                continue;
                            }
                        }
                    };
                    out.push(DriverStreamChunk::ToolUseComplete {
                        block_idx: idx as u32,
                        tool_use_id: acc.id,
                        name: acc.name,
                        parsed_input,
                    });
                }
            }
        }
    }
    out
}

/// Walks the projected message array and ensures every assistant message's
/// `tool_calls[*].id` has a matching `tool` message after it. Missing IDs get
/// a stub so DeepSeek/OpenAI's strict pairing check (HTTP 400 "insufficient
/// tool messages following tool_calls message") doesn't blow up the whole
/// turn.
///
/// The stub is phrased as a *transient* error so the model retries the call
/// in the next turn instead of treating it as a permanent failure and
/// hallucinating around it. (Earlier "[result unavailable]" wording made
/// v4-flash apologize and stop using the tool.)
fn ensure_tool_results_complete(messages: &mut Vec<Value>) {
    let mut i = 0;
    let mut total_stubs = 0usize;
    let mut total_assistant_with_tools = 0usize;
    while i < messages.len() {
        let needed_ids: Vec<String> = messages[i]
            .get("tool_calls")
            .and_then(|tc| tc.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.get("id").and_then(|id| id.as_str()).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if needed_ids.is_empty() {
            i += 1;
            continue;
        }
        total_assistant_with_tools += 1;
        // Scan the contiguous run of `tool` messages immediately following.
        let mut j = i + 1;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while j < messages.len() {
            let role = messages[j].get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "tool" {
                break;
            }
            if let Some(id) = messages[j].get("tool_call_id").and_then(|v| v.as_str()) {
                seen.insert(id.to_owned());
            }
            j += 1;
        }
        let missing: Vec<&String> = needed_ids.iter().filter(|id| !seen.contains(*id)).collect();
        if !missing.is_empty() {
            log::warn!(
                "DirectBackend OpenAI: assistant turn at idx {i} has {needed} tool_calls but only {seen} results returned; stubbing {n} missing id(s): {ids:?}",
                needed = needed_ids.len(),
                seen = seen.len(),
                n = missing.len(),
                ids = missing,
            );
        }
        for id in &missing {
            let stub = json!({
                "role": "tool",
                "tool_call_id": id,
                "content": "[client did not return a result for this tool call before the next turn was sent — please re-invoke the same tool with identical arguments to retry; do NOT apologize or substitute a fabricated answer]",
            });
            messages.insert(j, stub);
            j += 1;
            total_stubs += 1;
        }
        i = j;
    }
    if total_stubs > 0 {
        log::warn!(
            "DirectBackend OpenAI: stubbed {total_stubs} missing tool result(s) across {total_assistant_with_tools} assistant turn(s) before sending request",
        );
    }
}

fn project_messages(turns: &[NormalizedTurn], system: &str) -> Vec<Value> {
    let mut out = Vec::with_capacity(turns.len() + 1);
    out.push(json!({"role": "system", "content": system}));
    for t in turns {
        match t {
            NormalizedTurn::User { text, tool_results } => {
                if let Some(t) = text {
                    if !t.is_empty() {
                        out.push(json!({"role": "user", "content": t}));
                    }
                }
                // OpenAI requires one `tool` message per tool result.
                for r in tool_results {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": r.tool_use_id,
                        "content": r.content,
                    }));
                }
            }
            NormalizedTurn::Assistant { text, tool_uses, reasoning } => {
                let tool_calls: Vec<Value> = tool_uses
                    .iter()
                    .map(|u| {
                        let arguments = match serde_json::to_string(&u.input) {
                            Ok(s) => s,
                            Err(e) => {
                                // Should not happen — Value always serializes —
                                // but log if it does and fall back to empty obj
                                // rather than emitting bad JSON to OpenAI.
                                log::error!(
                                    "OpenAI history projection failed to re-serialize tool input for {}: {e}",
                                    tool_schema::name(u.tool_kind)
                                );
                                "{}".to_string()
                            }
                        };
                        json!({
                            "id": u.tool_use_id,
                            "type": "function",
                            "function": {
                                "name": tool_schema::name(u.tool_kind),
                                "arguments": arguments,
                            }
                        })
                    })
                    .collect();
                if tool_calls.is_empty() {
                    if let Some(t) = text {
                        let mut msg = serde_json::Map::new();
                        msg.insert("role".into(), json!("assistant"));
                        msg.insert("content".into(), json!(t));
                        // DeepSeek-R1 / o1 require the prior turn's CoT echoed
                        // back; vanilla OpenAI ignores unknown fields.
                        if let Some(r) = reasoning.as_ref() {
                            msg.insert("reasoning_content".into(), json!(r));
                        }
                        out.push(Value::Object(msg));
                    }
                } else {
                    let mut msg = serde_json::Map::new();
                    msg.insert("role".into(), json!("assistant"));
                    match text.as_deref() {
                        Some(t) if !t.is_empty() => {
                            msg.insert("content".into(), json!(t));
                        }
                        _ => {
                            msg.insert("content".into(), Value::Null);
                        }
                    }
                    if let Some(r) = reasoning.as_ref() {
                        msg.insert("reasoning_content".into(), json!(r));
                    }
                    msg.insert("tool_calls".into(), Value::Array(tool_calls));
                    out.push(Value::Object(msg));
                }
            }
        }
    }
    ensure_tool_results_complete(&mut out);
    out
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Value>,
    tools: Vec<ToolSpec<'a>>,
    tool_choice: &'static str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ToolSpec<'a> {
    r#type: &'static str,
    function: FunctionSpec<'a>,
}

#[derive(Serialize)]
struct FunctionSpec<'a> {
    name: &'a str,
    description: &'a str,
    parameters: Value,
}

#[derive(Deserialize)]
struct UsageBlock {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
}
