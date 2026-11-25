//! The model client against a local HTTP fixture that records requests and
//! serves scripted provider streams. See
//! `docs/develop/LLMClientGateway.md#verify-with-the-upstream-binary-and-mock-providers`.

mod support;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use alyosha::{
    agent_loop::{SessionDriver, SessionEvent, SessionSetup},
    agent_state::TurnOutcome,
    clock::FixedClock,
    llm_client::{GatewayClient, ModelClient, ModelError, ModelRequest, Timeouts},
};
use alyosha_config::Api;
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use support::{CLOCK_LINE, events_until_turn_ends, files_registry, read_file_definition};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

/// One scripted HTTP response.
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    /// Each chunk is sent after its delay.
    chunks: Vec<(Duration, String)>,
}

impl Reply {
    fn stream(events: &[Value]) -> Self {
        Self::sse(
            events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
        )
    }

    fn sse(text: String) -> Self {
        Self {
            status: 200,
            headers: vec![("x-request-id", "req_1")],
            chunks: vec![(Duration::ZERO, text)],
        }
    }

    fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("retry-after", "7")],
            chunks: vec![(Duration::ZERO, body.to_owned())],
        }
    }

    /// Accepts the request and never answers.
    fn silent() -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            chunks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct Recorded {
    path: String,
    headers: String,
    body: Value,
}

/// A local server answering each connection with the next scripted reply.
struct Fixture {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Fixture {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let request = read_request(&mut socket).await;
                    recorded.lock().unwrap().push(request);
                    if reply.status == 0 {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        return;
                    }
                    let mut head = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: text/event-stream\r\nconnection: close\r\n",
                        reply.status
                    );
                    for (name, value) in &reply.headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    for (delay, chunk) in reply.chunks {
                        tokio::time::sleep(delay).await;
                        if socket.write_all(chunk.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Recorded {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.unwrap();
        buffer.extend_from_slice(&chunk[..read]);
    }
    let path = head.split_whitespace().nth(1).unwrap().to_owned();
    let body =
        serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap_or(Value::Null);
    Recorded {
        path,
        headers: head.to_lowercase(),
        body,
    }
}

fn client(url: &str, api: Api) -> GatewayClient {
    GatewayClient::new(url, "test-key", api, Timeouts::default())
}

/// Runs the two-read turn through the session driver with the real client
/// and returns the recorded HTTP requests.
async fn two_read_turn(api: Api, provider: &str, replies: Vec<Reply>) -> Vec<Recorded> {
    let fixture = Fixture::start(replies).await;
    let reads = Arc::new(AtomicUsize::new(0));
    let (driver, mut events) = SessionDriver::start(SessionSetup::minimal(
        provider,
        "test-model",
        chrono_tz::Europe::Amsterdam,
        Arc::new(client(&fixture.url, api)),
        files_registry(reads.clone()),
        Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 11, 25, 9, 2, 0).unwrap(),
        )),
    ));
    let id = driver.submit("Read a.txt and b.txt.");
    let seen = events_until_turn_ends(&mut events, id).await;
    assert!(
        seen.contains(&SessionEvent::Reply {
            turn_id: id,
            text: "a.txt contains alpha. b.txt was missing.".to_owned()
        }),
        "{seen:?}"
    );
    assert_eq!(driver.snapshot().outcomes[&id], TurnOutcome::Completed);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(request.headers.contains("authorization: bearer test-key"));
    }
    requests
}

fn opening() -> String {
    format!("{CLOCK_LINE}\nRead a.txt and b.txt.")
}

#[tokio::test]
async fn responses_route_replays_reasoning_and_results() {
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "sealed"});
    let first = Reply::stream(&[
        json!({"type": "response.created"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "content": [{"type": "output_text", "text": "I'll read both."}]}}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "function_call", "call_id": "call_a", "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}}),
        json!({"type": "response.output_item.done", "output_index": 3, "item": {"type": "function_call", "call_id": "call_b", "name": "read_file", "arguments": "{\"path\":\"b.txt\"}"}}),
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 5}}}),
    ]);
    let second = Reply::stream(&[
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "content": [{"type": "output_text", "text": "a.txt contains alpha. b.txt was missing."}]}}),
        json!({"type": "response.completed", "response": {}}),
    ]);
    let requests = two_read_turn(Api::Codex, "chatgpt", vec![first, second]).await;

    let body = &requests[1].body;
    assert_eq!(requests[1].path, "/chatgpt/v1/responses");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["instructions"],
        "You are Alyosha, a personal assistant."
    );
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(
        body["tools"][0]["parameters"],
        read_file_definition().input_schema
    );
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": opening()}]},
            reasoning,
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I'll read both."}]},
            {"type": "function_call", "call_id": "call_a", "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"},
            {"type": "function_call", "call_id": "call_b", "name": "read_file", "arguments": "{\"path\":\"b.txt\"}"},
            {"type": "function_call_output", "call_id": "call_a", "output": "alpha"},
            {"type": "function_call_output", "call_id": "call_b", "output": "Error: file not found"},
        ])
    );
}

#[tokio::test]
async fn claude_route_sends_results_in_one_user_message() {
    let first = Reply::stream(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "I'll read "}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "both."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_a", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": " \"a.txt\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_b", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"b.txt\"}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 30}}),
        json!({"type": "message_stop"}),
    ]);
    let second = Reply::stream(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 50, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "a.txt contains alpha. b.txt was missing."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
        json!({"type": "message_stop"}),
    ]);
    let requests = two_read_turn(Api::Claudeapi, "work-claude", vec![first, second]).await;

    let request = &requests[1];
    assert_eq!(request.path, "/work-claude/v1/messages");
    assert!(request.headers.contains("anthropic-version: 2023-06-01"));
    assert_eq!(
        request.body["system"],
        "You are Alyosha, a personal assistant."
    );
    assert_eq!(request.body["max_tokens"], 32000);
    assert_eq!(
        request.body["tools"][0]["input_schema"],
        read_file_definition().input_schema
    );
    assert_eq!(
        request.body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": opening()}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "I'll read both."},
                {"type": "tool_use", "id": "toolu_a", "name": "read_file", "input": {"path": "a.txt"}},
                {"type": "tool_use", "id": "toolu_b", "name": "read_file", "input": {"path": "b.txt"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": "alpha", "is_error": false},
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": "Error: file not found", "is_error": true},
            ]},
        ])
    );
}

#[tokio::test]
async fn chat_route_returns_reasoning_content() {
    let first = Reply::stream(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "Two files, "}}]}),
        json!({"choices": [{"index": 0, "delta": {"reasoning_content": "two reads."}}]}),
        json!({"choices": [{"index": 0, "delta": {"content": "I'll read both."}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_a", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\""}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": ": \"a.txt\"}"}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 1, "id": "call_b", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\": \"b.txt\"}"}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 8}}),
    ]);
    let first = Reply::sse(first.chunks[0].1.clone() + "data: [DONE]\n\n");
    let second = Reply::stream(&[
        json!({"choices": [{"index": 0, "delta": {"content": "a.txt contains alpha. b.txt was missing."}, "finish_reason": "stop"}]}),
    ]);
    let requests = two_read_turn(Api::Deepseek, "deepseek", vec![first, second]).await;

    let request = &requests[1];
    assert_eq!(request.path, "/deepseek/v1/chat/completions");
    assert_eq!(
        request.body["messages"],
        json!([
            {"role": "system", "content": "You are Alyosha, a personal assistant."},
            {"role": "user", "content": opening()},
            {"role": "assistant", "content": "I'll read both.", "reasoning_content": "Two files, two reads.", "tool_calls": [
                {"id": "call_a", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}},
                {"id": "call_b", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"b.txt\"}"}},
            ]},
            {"role": "tool", "tool_call_id": "call_a", "content": "alpha"},
            {"role": "tool", "tool_call_id": "call_b", "content": "Error: file not found"},
        ])
    );
}

#[tokio::test]
async fn gemini_route_returns_thought_signatures_on_their_parts() {
    let first = Reply::stream(&[
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "I'll read "}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "both."}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}, "thoughtSignature": "sig-a"},
            {"functionCall": {"name": "read_file", "args": {"path": "b.txt"}}},
        ]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 7}}),
    ]);
    let second = Reply::stream(&[
        json!({"candidates": [{"content": {"role": "model", "parts": [
        {"text": "a.txt contains alpha. b.txt was missing."}
    ]}, "finishReason": "STOP"}]}),
    ]);
    let requests = two_read_turn(Api::Aistudio, "gemini", vec![first, second]).await;

    let request = &requests[1];
    assert_eq!(
        request.path,
        "/gemini/v1beta/models/test-model:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        request.body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        read_file_definition().input_schema
    );
    assert_eq!(
        request.body["contents"],
        json!([
            {"role": "user", "parts": [{"text": opening()}]},
            {"role": "model", "parts": [
                {"text": "I'll read both."},
                {"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}, "thoughtSignature": "sig-a"},
                {"functionCall": {"name": "read_file", "args": {"path": "b.txt"}}},
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "read_file", "response": {"output": "alpha"}}},
                {"functionResponse": {"name": "read_file", "response": {"error": "Error: file not found"}}},
            ]},
        ])
    );
}

fn request(provider: &str) -> ModelRequest {
    ModelRequest {
        provider: provider.to_owned(),
        model: "test-model".to_owned(),
        system_instruction: "Be brief.".to_owned(),
        summary: None,
        history: Vec::new(),
        tools: vec![read_file_definition()],
        max_output_tokens: None,
    }
}

async fn generate_once(
    api: Api,
    reply: Reply,
) -> Result<alyosha::llm_client::ModelReply, ModelError> {
    let fixture = Fixture::start(vec![reply]).await;
    client(&fixture.url, api)
        .generate(request("p"), CancellationToken::new())
        .await
}

#[tokio::test]
async fn a_stream_cut_after_a_tool_call_is_incomplete() {
    let cut = Reply::stream(&[
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "read_file", "arguments": "{}"}}),
    ]);
    let error = generate_once(Api::Codex, cut).await.unwrap_err();
    assert!(matches!(error, ModelError::Incomplete(_)), "{error}");

    let cut = Reply::stream(&[
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "read_file", "input": {}}}),
        json!({"type": "content_block_stop", "index": 0}),
    ]);
    let error = generate_once(Api::Claudeapi, cut).await.unwrap_err();
    assert!(matches!(error, ModelError::Incomplete(_)), "{error}");

    let cut = Reply::stream(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c", "function": {"name": "read_file", "arguments": "{}"}}]}}]}),
    ]);
    let error = generate_once(Api::Deepseek, cut).await.unwrap_err();
    assert!(matches!(error, ModelError::Incomplete(_)), "{error}");

    let cut = Reply::stream(&[
        json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "read_file", "args": {}}}]}}]}),
    ]);
    let error = generate_once(Api::Aistudio, cut).await.unwrap_err();
    assert!(matches!(error, ModelError::Incomplete(_)), "{error}");
}

#[tokio::test]
async fn the_output_limit_is_incomplete() {
    let cases = [
        (
            Api::Openai,
            vec![
                json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}}}),
            ],
        ),
        (
            Api::Claudeapi,
            vec![
                json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
                json!({"type": "message_stop"}),
            ],
        ),
        (
            Api::Deepseek,
            vec![json!({"choices": [{"delta": {"content": "x"}, "finish_reason": "length"}]})],
        ),
        (
            Api::Aistudio,
            vec![
                json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "MAX_TOKENS"}]}),
            ],
        ),
    ];
    for (api, events) in cases {
        let error = generate_once(api, Reply::stream(&events))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ModelError::Incomplete(_)),
            "{api:?}: {error}"
        );
    }
}

#[tokio::test]
async fn error_statuses_map_to_their_kinds() {
    let error = generate_once(
        Api::Codex,
        Reply::status(429, r#"{"error":{"message":"pool exhausted"}}"#),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        ModelError::RateLimited {
            message: "pool exhausted".to_owned(),
            retry_after: Some(Duration::from_secs(7)),
        }
    );
    for status in [401, 403, 404] {
        let error = generate_once(Api::Codex, Reply::status(status, "{}"))
            .await
            .unwrap_err();
        assert_eq!(error, ModelError::NotSignedIn, "{status}");
        assert!(error.to_string().contains("alyosha gateway login"));
    }
    let error = generate_once(
        Api::Openai,
        Reply::status(
            400,
            r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(error, ModelError::ContextOverflow("too long".to_owned()));
    let error = generate_once(Api::Codex, Reply::status(502, "upstream down"))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Provider("HTTP 502: upstream down".to_owned())
    );
}

#[tokio::test]
async fn a_stream_error_event_is_classified() {
    let failed = Reply::stream(&[
        json!({"type": "response.failed", "response": {"error": {"code": "context_length_exceeded", "message": "Your input exceeds the context window."}}}),
    ]);
    let error = generate_once(Api::Codex, failed).await.unwrap_err();
    assert!(matches!(error, ModelError::ContextOverflow(_)), "{error}");
}

#[tokio::test]
async fn usage_and_request_id_are_returned() {
    let reply = generate_once(
        Api::Codex,
        Reply::stream(&[
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 3, "output_tokens": 1}}}),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(reply.entry.text(), "hi");
    assert_eq!(reply.request_id.as_deref(), Some("req_1"));
    assert_eq!(reply.usage.unwrap().input_tokens, 3);
}

#[tokio::test]
async fn silence_times_out_but_a_slow_stream_completes() {
    let timeouts = Timeouts {
        connect: Duration::from_secs(10),
        idle: Duration::from_millis(400),
    };
    let fixture = Fixture::start(vec![Reply::silent()]).await;
    let error = GatewayClient::new(&fixture.url, "k", Api::Codex, timeouts)
        .generate(request("p"), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Timeout(_)), "{error}");

    // One event every 200 ms for over a second: longer than the idle limit
    // in total, never silent for that long.
    let mut slow = Reply::sse(String::new());
    slow.chunks = (0..6)
        .map(|_| (Duration::from_millis(200), ": keep-alive\n\n".to_owned()))
        .chain([(
            Duration::from_millis(200),
            format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "content": [{"type": "output_text", "text": "done"}]}}),
                json!({"type": "response.completed", "response": {}})
            ),
        )])
        .collect();
    let fixture = Fixture::start(vec![slow]).await;
    let reply = GatewayClient::new(&fixture.url, "k", Api::Codex, timeouts)
        .generate(request("p"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(reply.entry.text(), "done");
}

#[tokio::test]
async fn stop_cancels_a_waiting_request() {
    let fixture = Fixture::start(vec![Reply::silent()]).await;
    let client = client(&fixture.url, Api::Codex);
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stopper.cancel();
    });
    let error = client.generate(request("p"), cancel).await.unwrap_err();
    assert_eq!(error, ModelError::Cancelled);
}

#[tokio::test]
async fn nothing_listening_names_the_address() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = client(&url, Api::Codex)
        .generate(request("p"), CancellationToken::new())
        .await
        .unwrap_err();
    let ModelError::Unreachable(message) = &error else {
        panic!("{error}")
    };
    assert!(message.contains(&url) && message.contains("systemctl status gproxy"));
}
