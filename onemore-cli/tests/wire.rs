//! 两种 API 的"导线级"集成测试:
//! 本地起一个脚本化的 mock SSE 服务器,让 Agent 完整跑一轮
//!   用户输入 → 模型(mock)请求 read_file → 真实执行工具 → 结果回填 → 最终回答
//! 并校验:
//! 1. 事件流正确(文本、工具执行、结束);
//! 2. 第二次请求体里,工具调用/工具结果按各 API 的正确格式编码。
//!
//! 这组测试不碰真实网络,却覆盖了适配器最容易写错的两个方向
//! (SSE 解码、请求体编码),改动 provider 层后必跑。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use onemore::config::Config;
use onemore::event::{AgentCommand, AgentEvent};
use onemore::runtime::Agent;
use onemore::workspace::Workspace;

// ---------------------------------------------------------------------------
// mock SSE 服务器
// ---------------------------------------------------------------------------

struct MockServer {
    port: u16,
    /// 收到的请求体(JSON 文本),按到达顺序。
    bodies: Arc<Mutex<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    /// 起服务:依次用 `responses` 里的 SSE 文本应答收到的每个请求。
    fn start(responses: Vec<String>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies2 = bodies.clone();

        let handle = std::thread::spawn(move || {
            let total = responses.len();
            let mut idx = 0usize;
            while idx < total {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                // 同一连接上可能有多个请求(HTTP keep-alive)
                while idx < total {
                    let Some(body) = read_http_request(&mut reader) else {
                        break; // 连接关闭,回去 accept 新连接
                    };
                    bodies2.lock().unwrap().push(body);
                    let sse = &responses[idx];
                    idx += 1;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                        sse.len(),
                        sse
                    );
                    writer.write_all(resp.as_bytes()).unwrap();
                    writer.flush().unwrap();
                }
            }
            // 留出时间让客户端读完最后的响应再关闭
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        MockServer {
            port,
            bodies,
            handle: Some(handle),
        }
    }

    fn body(&self, i: usize) -> serde_json::Value {
        let bodies = self.bodies.lock().unwrap();
        serde_json::from_str(&bodies[i])
            .unwrap_or_else(|e| panic!("请求体 {} 不是合法 JSON: {}\n{}", i, e, bodies[i]))
    }

    fn finish(mut self) {
        if let Some(h) = self.handle.take() {
            h.join().unwrap();
        }
    }
}

/// 读一个 HTTP 请求(请求行 + 头 + 按 Content-Length 的体),返回体。
fn read_http_request(reader: &mut BufReader<impl Read>) -> Option<String> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

/// 准备一个临时工作目录(内含 hello.txt)+ 指向 mock 的配置,跑一轮对话。
fn run_agent_against(api: &str, port: u16) -> Vec<AgentEvent> {
    run_agent_against_profile(api, None, port)
}

fn run_agent_against_profile(api: &str, profile: Option<&str>, port: u16) -> Vec<AgentEvent> {
    // 用户环境里的代理会劫持 127.0.0.1 请求,测试内清掉
    for k in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
    ] {
        std::env::remove_var(k);
    }

    let dir = std::env::temp_dir().join(format!("onemore-wire-{}-{}", api, port));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "hello from onemore\n").unwrap();

    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[agent]
provider = "mock"

[providers.mock]
api = "{}"
{}
base_url = "http://127.0.0.1:{}"
model = "test-model"
api_key = "test-key"
"#,
            api,
            profile
                .map(|value| format!("profile = {:?}", value))
                .unwrap_or_default(),
            port
        ),
    )
    .unwrap();

    let cfg = Config::load(&config_path).unwrap();
    let mut agent = Agent::new_with_data_dir(
        cfg,
        Workspace::new(PathBuf::from(&dir)),
        dir.join(".onemore-test"),
    )
    .unwrap();

    let mut events: Vec<AgentEvent> = Vec::new();
    let cancel = AtomicBool::new(false);
    {
        let mut emit = |e: AgentEvent| events.push(e);
        agent.handle_command(
            AgentCommand::UserInput("请读取 hello.txt".into()),
            &mut emit,
            &cancel,
        );
    }
    events
}

fn last_cache_usage(events: &[AgentEvent]) -> Option<onemore::message::CacheUsage> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::Usage { cache, .. } => *cache,
        _ => None,
    })
}

/// 事件流的共同断言:助手说了话、read_file 真的执行了且拿到文件内容、正常收尾。
fn assert_common_events(events: &[AgentEvent]) {
    let mut saw_tool_ok = false;
    let mut final_texts: Vec<String> = Vec::new();
    let mut finished_ok = false;
    for e in events {
        match e {
            AgentEvent::ToolCallFinished {
                name,
                output,
                error,
                ..
            } => {
                assert_eq!(name, "read_file");
                assert!(error.is_none(), "read_file 不应报错: {:?}", error);
                assert!(
                    output.model_text.contains("hello from onemore"),
                    "工具输出应包含文件内容: {}",
                    output.model_text
                );
                saw_tool_ok = true;
            }
            AgentEvent::AssistantMessage(t) => final_texts.push(t.clone()),
            AgentEvent::TurnFinished { cancelled } => {
                assert!(!cancelled);
                finished_ok = true;
            }
            AgentEvent::Error(msg) => panic!("不应出现错误事件: {}", msg),
            _ => {}
        }
    }
    assert!(saw_tool_ok, "缺少成功的 ToolCallFinished 事件");
    assert!(finished_ok, "缺少 TurnFinished 事件");
    assert!(
        final_texts.iter().any(|t| t.contains("读取完成")),
        "缺少最终回答: {:?}",
        final_texts
    );
}

// ---------------------------------------------------------------------------
// Anthropic Messages
// ---------------------------------------------------------------------------

#[test]
fn anthropic_full_tool_roundtrip() {
    let turn1 = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1,"cache_read_input_tokens":6,"cache_creation_input_tokens":4}}}"#, "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#, "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"我读一下。"}}"#, "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#, "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"read_file","input":{}}}"#, "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"hello.txt\"}"}}"#, "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":1}"#, "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}"#, "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#, "\n\n",
    ).to_string();
    let turn2 = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":60,"output_tokens":1,"cache_read_input_tokens":30,"cache_creation_input_tokens":0}}}"#, "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#, "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"读取完成。"}}"#, "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#, "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}"#, "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#, "\n\n",
    ).to_string();

    let server = MockServer::start(vec![turn1, turn2]);
    let events = run_agent_against("messages", server.port);
    assert_common_events(&events);
    let cache = last_cache_usage(&events).expect("Anthropic 应上报缓存用量");
    assert_eq!(cache.read_tokens, 36);
    assert_eq!(cache.write_tokens, 4);

    // 第二次请求体:历史编码是否符合 Messages API
    let body = server.body(1);
    let msgs = body["messages"].as_array().unwrap();
    // [0] user 提问, [1] assistant(text+tool_use), [2] user(tool_result)
    assert_eq!(msgs.len(), 3, "应为 3 条消息: {}", body["messages"]);
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"][1]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][1]["id"], "toolu_01");
    assert_eq!(msgs[1]["content"][1]["input"]["path"], "hello.txt");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "toolu_01");
    assert!(msgs[2]["content"][0]["content"]
        .as_str()
        .unwrap()
        .contains("hello from onemore"));
    // 工具声明与系统提示
    assert!(body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "read_file" && t["input_schema"]["type"] == "object"));
    assert!(body["system"].as_str().unwrap().contains("Onemore"));
    server.finish();
}

// ---------------------------------------------------------------------------
// OpenAI Responses
// ---------------------------------------------------------------------------

#[test]
fn responses_full_tool_roundtrip() {
    let turn1 = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_1"}}"#, "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#, "\n\n",
        "event: response.output_item.done\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC_BLOB"}}"#, "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_r1","name":"read_file","arguments":""}}"#, "\n\n",
        "event: response.function_call_arguments.delta\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":"}"#, "\n\n",
        "event: response.function_call_arguments.delta\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"hello.txt\"}"}"#, "\n\n",
        "event: response.output_item.done\n",
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_r1","name":"read_file","arguments":"{\"path\":\"hello.txt\"}","status":"completed"}}"#, "\n\n",
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":20,"output_tokens":18,"input_tokens_details":{"cached_tokens":8,"cache_write_tokens":2}}}}"#, "\n\n",
    ).to_string();
    let turn2 = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_2"}}"#, "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#, "\n\n",
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","output_index":0,"delta":"读取"}"#, "\n\n",
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","output_index":0,"delta":"完成。"}"#, "\n\n",
        "event: response.output_item.done\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"读取完成。"}]}}"#, "\n\n",
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":{"input_tokens":90,"output_tokens":7,"input_tokens_details":{"cached_tokens":60,"cache_write_tokens":0}}}}"#, "\n\n",
    ).to_string();

    let server = MockServer::start(vec![turn1, turn2]);
    let events = run_agent_against("responses", server.port);
    assert_common_events(&events);
    let cache = last_cache_usage(&events).expect("OpenAI 应上报缓存用量");
    assert_eq!(cache.read_tokens, 68);
    assert_eq!(cache.write_tokens, 2);

    let first_body = server.body(0);
    let body = server.body(1);
    assert_eq!(first_body["prompt_cache_key"], body["prompt_cache_key"]);
    assert!(body["prompt_cache_key"]
        .as_str()
        .unwrap()
        .starts_with("onemore:v1:openai-responses:"));
    assert_eq!(body["store"], false);
    assert!(body["instructions"].as_str().unwrap().contains("Onemore"));
    let input = body["input"].as_array().unwrap();
    // [0] user 消息, [1] reasoning(原样回传), [2] function_call, [3] function_call_output
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["encrypted_content"], "ENC_BLOB");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_r1");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_r1");
    assert!(input[3]["output"]
        .as_str()
        .unwrap()
        .contains("hello from onemore"));
    // reasoning 必须排在它的 function_call 之前(API 硬性要求)
    // 工具声明是平铺的
    let read_file = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read_file")
        .unwrap();
    assert_eq!(read_file["type"], "function");
    assert!(read_file.get("function").is_none());
    server.finish();
}

#[test]
fn deepseek_reasoning_text_is_streamed_without_encrypted_replay() {
    let turn1 = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"ds_resp_1"}}"#, "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_ds"}}"#, "\n\n",
        "event: response.reasoning_text.delta\n",
        r#"data: {"type":"response.reasoning_text.delta","output_index":0,"delta":"先看文件。"}"#, "\n\n",
        "event: response.reasoning_text.done\n",
        r#"data: {"type":"response.reasoning_text.done","output_index":0,"text":"先看文件。"}"#, "\n\n",
        "event: response.output_item.done\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_ds","text":"先看文件。"}}"#, "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_ds","call_id":"call_ds","name":"read_file","arguments":""}}"#, "\n\n",
        "event: response.function_call_arguments.delta\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"hello.txt\"}"}"#, "\n\n",
        "event: response.output_item.done\n",
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_ds","call_id":"call_ds","name":"read_file","arguments":"{\"path\":\"hello.txt\"}"}}"#, "\n\n",
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"id":"ds_resp_1","usage":{"input_tokens":10,"output_tokens":12,"input_tokens_details":{"cached_tokens":6}}}}"#, "\n\n",
    ).to_string();
    let turn2 = concat!(
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"ds_msg_2","role":"assistant","content":[]}}"#, "\n\n",
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","output_index":0,"delta":"读取完成。"}"#, "\n\n",
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"id":"ds_resp_2","usage":{"input_tokens":20,"output_tokens":4,"input_tokens_details":{"cached_tokens":15}}}}"#, "\n\n",
    ).to_string();

    let server = MockServer::start(vec![turn1, turn2]);
    let events = run_agent_against_profile("responses", Some("deepseek-responses"), server.port);
    assert_common_events(&events);
    let cache = last_cache_usage(&events).expect("DeepSeek 应上报缓存读取量");
    assert_eq!(cache.read_tokens, 21);
    assert_eq!(cache.write_tokens, 0);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ThinkingDelta(text) if text.contains("先看文件")
    )));

    let body = server.body(1);
    assert!(body.get("store").is_none());
    assert!(body.get("include").is_none());
    assert!(body.get("prompt_cache_key").is_none());
    let input = body["input"].as_array().unwrap();
    assert!(input.iter().all(|item| item["type"] != "reasoning"));
    server.finish();
}

#[test]
fn anthropic_eof_before_message_stop_is_an_error() {
    let response = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#, "\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#, "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#, "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#, "\n\n",
    )
    .to_string();
    let server = MockServer::start(vec![response]);
    let events = run_agent_against("messages", server.port);
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::Error(message) if message.contains("终止事件"))));
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: false })));
    server.finish();
}

#[test]
fn responses_eof_before_terminal_is_an_error() {
    let response = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"eof"}}"#, "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m"}}"#, "\n\n",
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","output_index":0,"delta":"partial"}"#, "\n\n",
    )
    .to_string();
    let server = MockServer::start(vec![response]);
    let events = run_agent_against("responses", server.port);
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::Error(message) if message.contains("terminal"))));
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::TurnFinished { cancelled: false })));
    server.finish();
}
