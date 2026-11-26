//! OpenAI Responses API provider (`wire_api = "responses"`).
//!
//! Required for models that only work (or work best) on `/v1/responses`:
//! encrypted reasoning items are round-tripped statelessly (`store: false`),
//! so reasoning quality survives across tool-calling turns.

use crate::context::tokens::ModelPricing;
use crate::events::{ToolCall, ToolCallStatus};
use crate::model::model_trait::{
    GenerationParams, Model, ModelCapabilities, ModelError, ModelMessage, ModelResponse, ModelRole,
    ModelStreamChunk, ReasoningBlock, ToolChoice, Usage, is_openai_reasoning_model,
    retry_after_secs,
};
use crate::model::openai_native::OpenAIModel;
use crate::tool::tool_trait::ToolSpec;
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::pin::Pin;

const PROVIDER_TAG: &str = "openai";

/// OpenAI Responses model provider.
pub struct OpenAIResponsesModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model_id: String,
    capabilities: ModelCapabilities,
    pricing: Option<ModelPricing>,
}

#[derive(Debug, Default, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
    #[serde(default)]
    total_tokens: usize,
    #[serde(default)]
    input_tokens_details: Option<Value>,
    #[serde(default)]
    output_tokens_details: Option<Value>,
}

impl ResponsesUsage {
    fn to_usage(&self) -> Usage {
        let detail = |v: &Option<Value>, key: &str| {
            v.as_ref()
                .and_then(|d| d.get(key))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize
        };
        Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: if self.total_tokens > 0 {
                self.total_tokens
            } else {
                self.input_tokens + self.output_tokens
            },
            cache_read_tokens: detail(&self.input_tokens_details, "cached_tokens"),
            cache_write_tokens: 0,
            reasoning_tokens: detail(&self.output_tokens_details, "reasoning_tokens"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesBody {
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
}

impl OpenAIResponsesModel {
    /// Create a new Responses API provider.
    pub fn new(
        client: reqwest::Client,
        base_url: &str,
        api_key: &str,
        model_id: &str,
        capabilities: ModelCapabilities,
        pricing: Option<ModelPricing>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model_id: model_id.to_string(),
            capabilities,
            pricing,
        }
    }

    /// Returns the per-model pricing, if available.
    pub fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    fn responses_url(&self) -> String {
        if self.base_url.ends_with("/v1") {
            format!("{}/responses", self.base_url)
        } else {
            format!("{}/v1/responses", self.base_url)
        }
    }

    /// Estimate cost in USD from token usage and pricing.
    pub fn estimate_cost(&self, usage: &Usage) -> f64 {
        let Some(pricing) = &self.pricing else {
            return 0.0;
        };
        (usage.prompt_tokens as f64 / 1_000_000.0) * pricing.input_per_million
            + (usage.completion_tokens as f64 / 1_000_000.0) * pricing.output_per_million
    }

    fn to_input(messages: &[ModelMessage]) -> (Option<String>, Vec<Value>) {
        let mut instructions: Option<String> = None;
        let mut input = Vec::new();

        for msg in messages {
            match msg.role {
                ModelRole::System => {
                    instructions = Some(match instructions.take() {
                        Some(prev) => format!("{prev}\n\n{}", msg.content),
                        None => msg.content.clone(),
                    });
                }
                ModelRole::User => {
                    let mut content: Vec<Value> = msg
                        .extras
                        .images
                        .iter()
                        .map(|img| json!({"type": "input_image", "image_url": img.data_url()}))
                        .collect();
                    content.push(json!({"type": "input_text", "text": msg.content}));
                    input.push(json!({"type": "message", "role": "user", "content": content}));
                }
                ModelRole::Assistant => {
                    for block in msg
                        .extras
                        .reasoning
                        .iter()
                        .filter(|b| b.provider == PROVIDER_TAG)
                    {
                        let (Some(id), Some(enc)) = (&block.id, &block.signature) else {
                            continue;
                        };
                        input.push(json!({
                            "type": "reasoning",
                            "id": id,
                            "summary": [],
                            "encrypted_content": enc,
                        }));
                    }
                    if !msg.content.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": msg.content}],
                        }));
                    }
                    for tc in msg.tool_calls.iter().flatten() {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.name,
                            "arguments": serde_json::to_string(&tc.args).unwrap_or_default(),
                        }));
                    }
                }
                ModelRole::Tool => {
                    if let Some(id) = &msg.tool_call_id {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": id,
                            "output": msg.content,
                        }));
                    }
                }
            }
        }
        (instructions, input)
    }

    fn build_request(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
        stream: bool,
    ) -> Value {
        let (instructions, input) = Self::to_input(messages);
        let mut body = json!({
            "model": self.model_id,
            "input": input,
            "stream": stream,
            "store": false,
        });
        let obj = body.as_object_mut().expect("object");

        if let Some(i) = instructions {
            obj.insert("instructions".into(), json!(i));
        }
        if let Some(max) = params.max_tokens {
            obj.insert("max_output_tokens".into(), json!(max));
        }

        let reasoning_model = is_openai_reasoning_model(&self.model_id);
        if !reasoning_model {
            if let Some(t) = params.temperature
                && self.capabilities.temperature_supported
            {
                obj.insert("temperature".into(), json!(t));
            }
            if let Some(p) = params.top_p {
                obj.insert("top_p".into(), json!(p));
            }
        }
        if self.capabilities.thinking
            && (is_openai_reasoning_model(&self.model_id)
                || params.reasoning.is_some_and(|l| l.effort().is_some()))
        {
            let effort = params
                .reasoning
                .and_then(|l| l.effort())
                .unwrap_or("medium");
            obj.insert(
                "reasoning".into(),
                json!({"effort": effort, "summary": "auto"}),
            );
            // Needed to replay reasoning statelessly.
            obj.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }

        if let Some(specs) = tools.filter(|t| !t.is_empty()) {
            let tools: Vec<Value> = specs
                .iter()
                .map(|s| {
                    json!({
                        "type": "function",
                        "name": s.name,
                        "description": s.description,
                        "parameters": s.input_schema,
                    })
                })
                .collect();
            obj.insert("tools".into(), Value::Array(tools));
            let choice = match &params.tool_choice {
                Some(ToolChoice::None) => json!("none"),
                Some(ToolChoice::Required) => json!("required"),
                Some(ToolChoice::Tool(name)) => json!({"type": "function", "name": name}),
                _ => json!("auto"),
            };
            obj.insert("tool_choice".into(), choice);
        }
        body
    }

    fn reasoning_block(item: &Value) -> Option<ReasoningBlock> {
        let id = item.get("id")?.as_str()?.to_string();
        let text = item
            .get("summary")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();
        Some(ReasoningBlock {
            provider: PROVIDER_TAG.to_string(),
            text,
            signature: item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_string),
            id: Some(id),
            ..Default::default()
        })
    }

    fn function_call(item: &Value) -> Option<ToolCall> {
        let name = item.get("name")?.as_str()?.to_string();
        let id = item
            .get("call_id")
            .or_else(|| item.get("id"))?
            .as_str()?
            .to_string();
        let args = item
            .get("arguments")
            .and_then(Value::as_str)
            .map(|a| {
                if a.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(a).unwrap_or(Value::Null)
                }
            })
            .unwrap_or_else(|| json!({}));
        Some(ToolCall {
            id,
            name,
            args,
            status: ToolCallStatus::Ok,
        })
    }

    fn from_body(body: ResponsesBody) -> ModelResponse {
        let mut content = String::new();
        let mut thinking = String::new();
        let mut reasoning = Vec::new();
        let mut calls = Vec::new();

        for item in &body.output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            content.push_str(t);
                        }
                    }
                }
                Some("reasoning") => {
                    if let Some(block) = Self::reasoning_block(item) {
                        thinking.push_str(&block.text);
                        reasoning.push(block);
                    }
                }
                Some("function_call") => calls.extend(Self::function_call(item)),
                _ => {}
            }
        }

        ModelResponse {
            content,
            tool_calls: (!calls.is_empty()).then_some(calls),
            thinking: (!thinking.is_empty()).then_some(thinking),
            reasoning,
            usage: body.usage.map(|u| u.to_usage()),
        }
    }
}

#[async_trait]
impl Model for OpenAIResponsesModel {
    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn complete(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<ModelResponse, ModelError> {
        let request = self.build_request(messages, params, tools, false);
        let response = self
            .client
            .post(self.responses_url())
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(OpenAIModel::map_reqwest_error)?;

        let status = response.status();
        let retry_after = retry_after_secs(response.headers());
        let body = response
            .text()
            .await
            .map_err(OpenAIModel::map_reqwest_error)?;
        if !status.is_success() {
            return Err(OpenAIModel::map_http_error(status, &body).with_retry_after(retry_after));
        }
        let parsed: ResponsesBody = serde_json::from_str(&body)
            .map_err(|e| ModelError::Provider(format!("JSON parse error: {e}")))?;
        Ok(Self::from_body(parsed))
    }

    async fn stream(
        &self,
        messages: &[ModelMessage],
        params: &GenerationParams,
        tools: Option<&[ToolSpec]>,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<ModelStreamChunk, ModelError>> + Send + 'static>>,
        ModelError,
    > {
        let request = self.build_request(messages, params, tools, true);
        let response = self
            .client
            .post(self.responses_url())
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(OpenAIModel::map_reqwest_error)?;

        let status = response.status();
        let retry_after = retry_after_secs(response.headers());
        if !status.is_success() {
            let body = response
                .text()
                .await
                .map_err(OpenAIModel::map_reqwest_error)?;
            return Err(OpenAIModel::map_http_error(status, &body).with_retry_after(retry_after));
        }
        Ok(Box::pin(SseStream::new(response.bytes_stream())))
    }
}

/// Converts Responses API server-sent events into [`ModelStreamChunk`]s.
pub(crate) struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    finished: bool,
}

impl SseStream {
    fn new(
        byte_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(byte_stream),
            buffer: Vec::new(),
            finished: false,
        }
    }

    fn process_data_line(&mut self, data: &str) -> Option<Result<ModelStreamChunk, ModelError>> {
        let event: Value = serde_json::from_str(data.trim()).ok()?;
        let delta = |event: &Value| {
            event
                .get("delta")
                .and_then(Value::as_str)
                .map(str::to_string)
        };

        match event.get("type")?.as_str()? {
            "response.output_text.delta" => Some(Ok(ModelStreamChunk {
                delta: delta(&event)?,
                ..Default::default()
            })),
            "response.reasoning_summary_text.delta" => Some(Ok(ModelStreamChunk {
                thinking: Some(delta(&event)?),
                ..Default::default()
            })),
            "response.reasoning_summary_part.done" => Some(Ok(ModelStreamChunk {
                // Keep summary paragraphs separated in the live view.
                thinking: Some("\n\n".to_string()),
                ..Default::default()
            })),
            "response.output_item.done" => {
                let item = event.get("item")?;
                match item.get("type")?.as_str()? {
                    "function_call" => {
                        let call = OpenAIResponsesModel::function_call(item)?;
                        Some(Ok(ModelStreamChunk {
                            tool_calls: Some(vec![call]),
                            ..Default::default()
                        }))
                    }
                    "reasoning" => {
                        let block = OpenAIResponsesModel::reasoning_block(item)?;
                        Some(Ok(ModelStreamChunk {
                            reasoning: vec![block],
                            ..Default::default()
                        }))
                    }
                    _ => None,
                }
            }
            kind @ ("response.completed" | "response.incomplete") => {
                self.finished = true;
                let response = event.get("response");
                let usage = response
                    .and_then(|r| r.get("usage"))
                    .and_then(|u| serde_json::from_value::<ResponsesUsage>(u.clone()).ok())
                    .map(|u| u.to_usage());
                let truncated = kind == "response.incomplete"
                    && response
                        .and_then(|r| r.get("incomplete_details"))
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        == Some("max_output_tokens");
                Some(Ok(ModelStreamChunk {
                    finished: true,
                    usage,
                    truncated,
                    ..Default::default()
                }))
            }
            "response.failed" | "error" => {
                self.finished = true;
                let message = event
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| event.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| event.get("message").and_then(Value::as_str))
                    .unwrap_or("response failed")
                    .to_string();
                Some(Err(ModelError::StreamInterrupted(message)))
            }
            _ => None,
        }
    }
}

impl Stream for SseStream {
    type Item = Result<ModelStreamChunk, ModelError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }

        loop {
            if let Some(pos) = this.buffer.iter().position(|b| *b == b'\n') {
                let line_bytes = this.buffer.drain(..=pos).collect::<Vec<u8>>();
                let line = String::from_utf8_lossy(&line_bytes)
                    .trim_end_matches(['\r', '\n'])
                    .to_string();
                if let Some(data) = line.strip_prefix("data:")
                    && let Some(result) = this.process_data_line(data)
                {
                    return Poll::Ready(Some(result));
                }
                continue;
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => this.buffer.extend_from_slice(&bytes),
                Poll::Ready(Some(Err(e))) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(ModelError::StreamInterrupted(e.to_string()))));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    // A stream that ends without `response.completed` was cut off.
                    return Poll::Ready(Some(Err(ModelError::StreamInterrupted(
                        "stream ended before response.completed".to_string(),
                    ))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::model_trait::{ModelTier, ReasoningLevel};
    use crate::tool::builtin::EditFormat;
    use futures::StreamExt;

    fn caps(thinking: bool) -> ModelCapabilities {
        ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking,
            max_context: 400_000,
            max_output: 128_000,
            temperature_supported: true,
            tier: ModelTier::Frontier,
            edit_format: EditFormat::Diff,
        }
    }

    fn model(id: &str, thinking: bool) -> OpenAIResponsesModel {
        OpenAIResponsesModel::new(
            reqwest::Client::new(),
            "https://api.openai.com",
            "k",
            id,
            caps(thinking),
            None,
        )
    }

    #[test]
    fn request_shape_and_reasoning_replay() {
        let m = model("gpt-5", true);
        let messages = vec![
            ModelMessage {
                role: ModelRole::System,
                content: "sys".into(),
                ..Default::default()
            },
            ModelMessage {
                role: ModelRole::User,
                content: "hi".into(),
                ..Default::default()
            },
            ModelMessage {
                role: ModelRole::Assistant,
                content: "".into(),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    args: json!({"p": 1}),
                    status: ToolCallStatus::Ok,
                }]),
                extras: crate::model::MessageExtras {
                    reasoning: vec![
                        ReasoningBlock {
                            provider: "openai".into(),
                            id: Some("rs_1".into()),
                            signature: Some("ENC".into()),
                            ..Default::default()
                        },
                        ReasoningBlock {
                            provider: "anthropic".into(),
                            signature: Some("nope".into()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
            ModelMessage {
                role: ModelRole::Tool,
                content: "contents".into(),
                tool_call_id: Some("call_1".into()),
                ..Default::default()
            },
        ];
        let params = GenerationParams {
            reasoning: Some(ReasoningLevel::High),
            temperature: Some(0.3),
            ..Default::default()
        };
        let specs = vec![ToolSpec::new("read", "d", json!({"type": "object"}))];
        let body = m.build_request(&messages, &params, Some(&specs), true);

        assert_eq!(body["store"], false);
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["tool_choice"], "auto");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], "ENC");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["arguments"], "{\"p\":1}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input.len(), 4);
    }

    #[test]
    fn url_handles_v1_suffix() {
        let m = OpenAIResponsesModel::new(
            reqwest::Client::new(),
            "https://proxy.example/v1/",
            "k",
            "gpt-4.1",
            caps(false),
            None,
        );
        assert_eq!(m.responses_url(), "https://proxy.example/v1/responses");
        assert!(
            m.build_request(&[], &GenerationParams::default(), None, false)
                .get("reasoning")
                .is_none()
        );
    }

    #[test]
    fn parses_non_streaming_output() {
        let body: ResponsesBody = serde_json::from_value(json!({
            "output": [
                {"type": "reasoning", "id": "rs_9", "summary": [{"type": "summary_text", "text": "plan"}], "encrypted_content": "E"},
                {"type": "message", "content": [{"type": "output_text", "text": "done"}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "grep", "arguments": "{\"q\":\"x\"}"}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 30, "total_tokens": 130,
                      "input_tokens_details": {"cached_tokens": 64},
                      "output_tokens_details": {"reasoning_tokens": 20}}
        }))
        .unwrap();
        let r = OpenAIResponsesModel::from_body(body);
        assert_eq!(r.content, "done");
        assert_eq!(r.thinking.as_deref(), Some("plan"));
        assert_eq!(r.reasoning[0].id.as_deref(), Some("rs_9"));
        let calls = r.tool_calls.unwrap();
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].args["q"], "x");
        let u = r.usage.unwrap();
        assert_eq!((u.cache_read_tokens, u.reasoning_tokens), (64, 20));
    }

    const STREAM: &str = concat!(
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thinking\"}],\"encrypted_content\":\"ENC\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"p\\\"\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"{\\\"p\\\":2}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15,\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n",
    );

    async fn run(data: &'static str) -> Vec<Result<ModelStreamChunk, ModelError>> {
        SseStream::new(futures::stream::iter(vec![Ok(Bytes::from_static(
            data.as_bytes(),
        ))]))
        .collect()
        .await
    }

    #[tokio::test]
    async fn streams_text_reasoning_calls_and_usage() {
        let chunks: Vec<_> = run(STREAM).await.into_iter().map(|c| c.unwrap()).collect();
        let text: String = chunks.iter().map(|c| c.delta.as_str()).collect();
        assert_eq!(text, "Hello");
        let thinking: String = chunks.iter().filter_map(|c| c.thinking.clone()).collect();
        assert_eq!(thinking, "thinking");
        let blocks: Vec<_> = chunks.iter().flat_map(|c| c.reasoning.clone()).collect();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].signature.as_deref(), Some("ENC"));
        let calls: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.tool_calls.clone())
            .flatten()
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args["p"], 2);
        let last = chunks.last().unwrap();
        assert!(last.finished && !last.truncated);
        assert_eq!(last.usage.as_ref().unwrap().reasoning_tokens, 3);
    }

    #[tokio::test]
    async fn truncated_stream_without_completion_is_an_error() {
        let out = run("data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n").await;
        assert!(matches!(
            out.last().unwrap(),
            Err(ModelError::StreamInterrupted(_))
        ));
    }

    #[tokio::test]
    async fn incomplete_max_tokens_marks_truncated() {
        let out = run(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        )
        .await;
        assert!(out[0].as_ref().unwrap().truncated);
    }
}
