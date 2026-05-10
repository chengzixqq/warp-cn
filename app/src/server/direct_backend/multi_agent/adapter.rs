//! Stream adapter: provider-neutral `DriverChunkStream` →
//! `Stream<Result<api::ResponseEvent, Arc<AIApiError>>>` for the multi-agent
//! gRPC response. Coalesces text deltas, materializes tool calls, and emits
//! a final `Finished` event.

use std::collections::HashMap;
use std::sync::Arc;

use futures::stream::{self, Stream, StreamExt};
use uuid::Uuid;
use warp_multi_agent_api as api;

use super::encode;
use super::tool_schema;
use super::{DriverChunkStream, DriverStreamChunk};
use crate::server::server_api::AIApiError;

/// Adapter state. `text_messages` / `reasoning_messages` map producer
/// block_idx → the synthesized `api::Message.id` we minted for that block,
/// so subsequent deltas know which message to `AppendToMessageContent`
/// against. Reasoning and text are tracked separately so they each land in
/// their own message bubble (`AgentReasoning` vs `AgentOutput`).
struct AdapterState {
    task_id: String,
    request_id: String,
    text_messages: HashMap<u32, String>,
    reasoning_messages: HashMap<u32, String>,
    /// Captured `Stop` payload — emitted as `Finished` once the source
    /// stream closes (or as soon as we observe it, depending on adapter).
    stop: Option<DriverStreamChunk>,
    /// True after we have produced the terminal `Finished` event so further
    /// poll calls return None.
    finished: bool,
}

pub fn adapt(
    src: DriverChunkStream,
    task_id: String,
    request_id: String,
) -> impl Stream<Item = Result<api::ResponseEvent, Arc<AIApiError>>> + Send {
    log::info!("DirectBackend adapter: starting (task={}, request={})", task_id, request_id);
    let state = AdapterState {
        task_id,
        request_id,
        text_messages: HashMap::new(),
        reasoning_messages: HashMap::new(),
        stop: None,
        finished: false,
    };
    stream::unfold((src, state), |(mut src, mut st)| async move {
        if st.finished {
            return None;
        }
        loop {
            match src.next().await {
                None => {
                    // Stream ended. Emit `Finished` synthesized from the captured
                    // Stop chunk (or a generic Done if Stop never arrived).
                    log::info!(
                        "DirectBackend adapter: stream drained — emitting Finished (had_stop={}, text_blocks={}, reasoning_blocks={})",
                        st.stop.is_some(),
                        st.text_messages.len(),
                        st.reasoning_messages.len(),
                    );
                    st.finished = true;
                    let ev = encode::build_finished_from_stop(st.stop.take());
                    return Some((Ok(ev), (src, st)));
                }
                Some(Err(e)) => {
                    log::warn!("DirectBackend stream error: {e:#}");
                    st.finished = true;
                    let msg = super::sanitize_error(&e);
                    return Some((Ok(encode::build_finished_error(msg)), (src, st)));
                }
                Some(Ok(DriverStreamChunk::TextDelta { block_idx, text })) => {
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(msg_id) = st.text_messages.get(&block_idx).cloned() {
                        let ev =
                            encode::build_append_to_text(&st.task_id, &msg_id, &text);
                        return Some((Ok(ev), (src, st)));
                    } else {
                        let msg_id = Uuid::new_v4().to_string();
                        st.text_messages.insert(block_idx, msg_id.clone());
                        let ev = encode::build_add_then_append_text(
                            &st.task_id,
                            &st.request_id,
                            &msg_id,
                            &text,
                        );
                        return Some((Ok(ev), (src, st)));
                    }
                }
                Some(Ok(DriverStreamChunk::ReasoningDelta { block_idx, text })) => {
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(msg_id) = st.reasoning_messages.get(&block_idx).cloned() {
                        let ev =
                            encode::build_append_to_reasoning(&st.task_id, &msg_id, &text);
                        return Some((Ok(ev), (src, st)));
                    } else {
                        let msg_id = Uuid::new_v4().to_string();
                        st.reasoning_messages.insert(block_idx, msg_id.clone());
                        let ev = encode::build_add_then_append_reasoning(
                            &st.task_id,
                            &st.request_id,
                            &msg_id,
                            &text,
                        );
                        return Some((Ok(ev), (src, st)));
                    }
                }
                Some(Ok(DriverStreamChunk::ToolUseStart { .. })) => continue,
                Some(Ok(DriverStreamChunk::ToolUseComplete {
                    tool_use_id,
                    name,
                    parsed_input,
                    ..
                })) => {
                    log::info!(
                        "DirectBackend adapter: ToolUseComplete name={} id={} args={}",
                        name, tool_use_id, parsed_input,
                    );
                    let kind = match tool_schema::from_name(&name) {
                        Some(k) => k,
                        None => {
                            log::warn!("Unknown tool from streaming driver: {name}");
                            let ev = encode::build_inline_text_message(
                                &st.task_id,
                                &st.request_id,
                                &format!(
                                    "[tool error] You called an unknown tool `{name}`."
                                ),
                            );
                            return Some((Ok(ev), (src, st)));
                        }
                    };
                    let tool = match tool_schema::parse_input(kind, parsed_input) {
                        Ok(t) => t,
                        Err(e) => {
                            let ev = encode::build_inline_text_message(
                                &st.task_id,
                                &st.request_id,
                                &format!(
                                    "[tool error] Your `{name}` tool call had \
                                     malformed input: {e}. Please fix and retry."
                                ),
                            );
                            return Some((Ok(ev), (src, st)));
                        }
                    };
                    log::info!(
                        "DirectBackend adapter: emitting ToolCall name={} id={}",
                        name, tool_use_id,
                    );
                    let ev = encode::build_tool_call_message_action(
                        &st.task_id,
                        &st.request_id,
                        &tool_use_id,
                        tool,
                    );
                    return Some((Ok(ev), (src, st)));
                }
                Some(Ok(DriverStreamChunk::ToolUseSoftError { message })) => {
                    let ev = encode::build_inline_text_message(
                        &st.task_id,
                        &st.request_id,
                        &message,
                    );
                    return Some((Ok(ev), (src, st)));
                }
                Some(Ok(stop @ DriverStreamChunk::Stop { .. })) => {
                    if let DriverStreamChunk::Stop {
                        stop_reason,
                        input_tokens,
                        output_tokens,
                        model_id,
                        ..
                    } = &stop
                    {
                        log::info!(
                            "DirectBackend adapter: captured Stop reason={:?} in_tokens={:?} out_tokens={:?} model={}",
                            stop_reason, input_tokens, output_tokens, model_id,
                        );
                    }
                    st.stop = Some(stop);
                    // Don't emit yet — wait for stream close so we don't miss
                    // any trailing chunks (Anthropic sends `message_delta` with
                    // usage metadata, then `message_stop`).
                    continue;
                }
            }
        }
    })
}
