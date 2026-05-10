//! Gemini `:streamGenerateContent` driver with multi-tool support and SSE.
//!
//! Roles are `user` / `model`; tool calls are `parts: [{ functionCall }]`,
//! tool results are `parts: [{ functionResponse: { name, response } }]`.
//! Gemini doesn't return tool_call ids — we synthesize them from the part
//! index so subsequent functionResponse turns can reference them. Gemini SSE
//! ships *cumulative* text per part on each chunk, not deltas; we slice
//! `text[last_len..]` to derive deltas (works either way).

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use futures::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event as SseEvent, RequestBuilderExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::decode::NormalizedTurn;
use super::tool_schema;
use super::{DecodedBlock, DriverChunkStream, DriverOutput, DriverStreamChunk};
use crate::server::direct_backend::common::truncate_for_log;
use crate::server::direct_backend::ResolvedProvider;

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
        return Err(anyhow!("DirectBackend Gemini agent: no turns to send"));
    }

    let contents = project_contents(turns);
    let advertised = tool_schema::advertised_tools(mcp);
    let function_declarations: Vec<FunctionDecl> = advertised
        .iter()
        .map(|kind| {
            let raw: Value = serde_json::from_str(tool_schema::schema(*kind))
                .context("tool schema must parse")?;
            Ok(FunctionDecl {
                name: tool_schema::name(*kind),
                description: tool_schema::description(*kind),
                parameters: sanitize_for_gemini(raw),
            })
        })
        .collect::<anyhow::Result<_>>()?;

    let system = super::compose_system_prompt(SYSTEM_PROMPT, mcp);
    let body = GeminiRequest {
        system_instruction: SystemInstruction {
            parts: vec![SystemPart { text: &system }],
        },
        contents,
        tools: vec![ToolSpec {
            function_declarations,
        }],
    };

    // `?alt=sse` is REQUIRED. Without it Gemini returns a JSON array of
    // generation chunks, not an SSE stream.
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
        provider.base_url, provider.model_id
    );

    let client = Client::builder()
        .timeout(super::HTTP_TIMEOUT)
        .build()
        .context("Gemini agent client init")?;
    let req = client
        .post(&url)
        .header("x-goog-api-key", &provider.api_key)
        .header("accept", "text/event-stream")
        .json(&body);
    let event_source = req.eventsource().context("Gemini SSE init")?;

    let model_id = provider.model_id.clone();
    let state = StreamState {
        es: event_source,
        last_text_by_block: HashMap::new(),
        function_call_seq: 0,
        pending: Vec::new(),
        finished: false,
        input_tokens: None,
        output_tokens: None,
        stop_reason: None,
        model_id,
    };
    let s = futures::stream::unfold(state, |mut st| async move {
        if st.finished {
            return None;
        }
        loop {
            if let Some(emit) = st.pending.pop() {
                return Some((Ok(emit), st));
            }
            match st.es.next().await {
                None => {
                    st.finished = true;
                    return Some((Ok(DriverStreamChunk::Stop {
                        stop_reason: st.stop_reason.clone(),
                        input_tokens: st.input_tokens,
                        output_tokens: st.output_tokens,
                        input_cache_read: None,
                        model_id: st.model_id.clone(),
                    }), st));
                }
                Some(Err(e)) => {
                    if matches!(e, reqwest_eventsource::Error::StreamEnded) {
                        st.finished = true;
                        return Some((Ok(DriverStreamChunk::Stop {
                            stop_reason: st.stop_reason.clone(),
                            input_tokens: st.input_tokens,
                            output_tokens: st.output_tokens,
                            input_cache_read: None,
                            model_id: st.model_id.clone(),
                        }), st));
                    }
                    st.finished = true;
                    return Some((Err(anyhow!("Gemini SSE: {e}")), st));
                }
                Some(Ok(SseEvent::Open)) => continue,
                Some(Ok(SseEvent::Message(m))) => {
                    let outcomes = parse_gemini_chunk(&m.data, &mut st);
                    if outcomes.is_empty() {
                        continue;
                    }
                    let mut iter = outcomes.into_iter();
                    let first = iter.next().unwrap();
                    for extra in iter {
                        st.pending.insert(0, extra);
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

struct StreamState {
    es: reqwest_eventsource::EventSource,
    /// Per part-index, the prefix we have already surfaced as deltas. Gemini
    /// sends the *full accumulated* text on each chunk; we diff via prefix
    /// match so a non-cumulative or rewritten chunk doesn't byte-slice into
    /// a UTF-8 boundary (which would panic).
    last_text_by_block: HashMap<u32, String>,
    /// Monotonic counter for synthetic tool_use_ids — old `gemini-{idx}` form
    /// could collide across parallel calls in the same response.
    function_call_seq: u32,
    pending: Vec<DriverStreamChunk>,
    finished: bool,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: Option<String>,
    model_id: String,
}

fn parse_gemini_chunk(data: &str, st: &mut StreamState) -> Vec<DriverStreamChunk> {
    let parsed: GeminiResponse = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Gemini SSE: skipping malformed chunk: {e}; data={}", truncate_for_log(data, 256));
            return vec![];
        }
    };
    if let Some(u) = &parsed.usage_metadata {
        if u.prompt_token_count.is_some() {
            st.input_tokens = u.prompt_token_count;
        }
        if u.candidates_token_count.is_some() {
            st.output_tokens = u.candidates_token_count;
        }
    }
    let mut out: Vec<DriverStreamChunk> = Vec::new();
    for candidate in parsed.candidates {
        if let Some(fr) = candidate.finish_reason {
            st.stop_reason = Some(fr);
        }
        let parts = candidate.content.map(|c| c.parts).unwrap_or_default();
        for (idx, part) in parts.into_iter().enumerate() {
            let block_idx = idx as u32;
            if let Some(text) = part.text {
                let prev = st.last_text_by_block.entry(block_idx).or_default();
                let delta = if text.starts_with(prev.as_str()) {
                    // Common case: cumulative append.
                    text[prev.len()..].to_string()
                } else {
                    // Defensive: provider sent a rewritten or non-cumulative
                    // chunk. Treat the whole `text` as a fresh delta and reset
                    // the tracker so future diffs are anchored on this string.
                    text.clone()
                };
                *prev = text;
                if !delta.is_empty() {
                    out.push(DriverStreamChunk::TextDelta {
                        block_idx,
                        text: delta,
                    });
                }
            }
            if let Some(call) = part.function_call {
                let name = call.name.clone();
                let seq = st.function_call_seq;
                st.function_call_seq += 1;
                let tool_use_id = format!("gemini-{}-{}", seq, Uuid::new_v4());
                out.push(DriverStreamChunk::ToolUseStart {
                    block_idx,
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                });
                out.push(DriverStreamChunk::ToolUseComplete {
                    block_idx,
                    tool_use_id,
                    name,
                    parsed_input: call.args,
                });
            }
        }
    }
    out
}

fn project_contents(turns: &[NormalizedTurn]) -> Vec<Value> {
    turns
        .iter()
        .map(|t| match t {
            NormalizedTurn::User { text, tool_results } => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    if !t.is_empty() {
                        parts.push(json!({"text": t}));
                    }
                }
                for r in tool_results {
                    parts.push(json!({
                        "functionResponse": {
                            "name": tool_schema::name(r.tool_kind),
                            "response": {"content": r.content, "is_error": r.is_error},
                        }
                    }));
                }
                if parts.is_empty() {
                    parts.push(json!({"text": ""}));
                }
                json!({"role": "user", "parts": parts})
            }
            NormalizedTurn::Assistant { text, tool_uses, reasoning: _ } => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    if !t.is_empty() {
                        parts.push(json!({"text": t}));
                    }
                }
                for u in tool_uses {
                    parts.push(json!({
                        "functionCall": {
                            "name": tool_schema::name(u.tool_kind),
                            "args": u.input,
                        }
                    }));
                }
                if parts.is_empty() {
                    parts.push(json!({"text": ""}));
                }
                json!({"role": "model", "parts": parts})
            }
        })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest<'a> {
    system_instruction: SystemInstruction<'a>,
    contents: Vec<Value>,
    tools: Vec<ToolSpec<'a>>,
}

#[derive(Serialize)]
struct SystemInstruction<'a> {
    parts: Vec<SystemPart<'a>>,
}

#[derive(Serialize)]
struct SystemPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolSpec<'a> {
    function_declarations: Vec<FunctionDecl<'a>>,
}

#[derive(Serialize)]
struct FunctionDecl<'a> {
    name: &'a str,
    description: &'a str,
    parameters: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponsePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    function_call: Option<FunctionCall>,
}

#[derive(Deserialize)]
struct FunctionCall {
    name: String,
    #[serde(default = "default_args")]
    args: Value,
}

fn default_args() -> Value {
    Value::Object(Default::default())
}

/// Strip JSON Schema keys that Gemini's function-declaration schema rejects.
/// Walks the schema tree recursively. Removed keys: `additionalProperties`,
/// `minItems`, `maxItems`, `minimum`, `maximum`, `default`. Leaves `type`,
/// `properties`, `items`, `required`, `enum`, `description` intact.
///
/// Additional fix-up: Gemini rejects `type:"object"` parameters that omit
/// `properties` (the MCP `arguments` schema is exactly this shape). Inject
/// an empty `properties` object so OBJECT-typed open schemas validate.
fn sanitize_for_gemini(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            let mut is_object_type = false;
            let mut has_properties = false;
            for (k, v) in map {
                if k == "type" && v.as_str() == Some("object") {
                    is_object_type = true;
                }
                if k == "properties" {
                    has_properties = true;
                }
                match k.as_str() {
                    "additionalProperties" | "minItems" | "maxItems" | "minimum" | "maximum"
                    | "default" => continue,
                    _ => {
                        out.insert(k, sanitize_for_gemini(v));
                    }
                }
            }
            if is_object_type && !has_properties {
                out.insert("properties".into(), Value::Object(Map::new()));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(sanitize_for_gemini).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_unsupported_keys() {
        let v = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "x": {"type": "integer", "minimum": 1, "maximum": 10},
                "y": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 5},
            },
            "required": ["x"]
        });
        let out = sanitize_for_gemini(v);
        assert!(out.get("additionalProperties").is_none());
        let x = &out["properties"]["x"];
        assert!(x.get("minimum").is_none());
        assert!(x.get("maximum").is_none());
        let y = &out["properties"]["y"];
        assert!(y.get("minItems").is_none());
        assert!(y.get("maxItems").is_none());
        // Preserved keys
        assert_eq!(out["type"], "object");
        assert_eq!(out["required"][0], "x");
        assert_eq!(y["items"]["type"], "string");
    }

    #[test]
    fn sanitize_injects_empty_properties_for_open_object() {
        // Gemini rejects `type:"object"` without `properties`; sanitizer must inject `{}`.
        let v = json!({"type": "object", "description": "named JSON args"});
        let out = sanitize_for_gemini(v);
        assert_eq!(out["type"], "object");
        assert!(out["properties"].is_object());
        assert_eq!(out["properties"].as_object().unwrap().len(), 0);
        assert_eq!(out["description"], "named JSON args");
    }

    #[test]
    fn sanitize_leaves_existing_properties_alone() {
        let v = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}}
        });
        let out = sanitize_for_gemini(v);
        // Should NOT clobber existing properties to {}.
        assert_eq!(out["properties"]["x"]["type"], "string");
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    #[serde(default)]
    prompt_token_count: Option<u32>,
    #[serde(default)]
    candidates_token_count: Option<u32>,
}
