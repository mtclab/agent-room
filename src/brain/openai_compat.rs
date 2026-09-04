//! OpenAI-compatible chat-completions brain.
//!
//! Written for the owner's local vLLM (Qwen3.8-27B) behind llama-swap, which
//! boots the model on the first request - hence `cold_start_timeout_s` as the
//! *total* request timeout rather than a connect timeout.
//!
//! `extra_body` is merged into the request body. That is how
//! `chat_template_kwargs: {enable_thinking: false}` reaches vLLM, which is
//! mandatory for Qwen3.8: without it the model spends its whole token budget
//! thinking and returns nothing.
//!
//! Two knobs exist for an ON-DEMAND model, and they are independent - which one
//! is right depends on somebody else's GPU:
//!
//! - `warm_on_intent`: [`Brain::warm`] fires ONE `max_tokens: 1` completion,
//!   fire and forget, when a human starts typing. The load happens while nobody
//!   is waiting, and `warm_cooldown_s` keeps a typed paragraph from becoming a
//!   request per keystroke burst.
//! - `judge_base_url` / `judge_model` / `judge_api_key` / `judge_extra_body`:
//!   the judge runs somewhere else entirely - a small resident model - so the
//!   big one is only ever loaded to actually speak. Such a judge gets its own,
//!   much shorter timeout (`judge_timeout_s`, or 30 s worked out from the shape
//!   of the config): waiting a cold start for "is there anything here worth
//!   saying?" is minutes of silence over a question the answer to which is
//!   usually no.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};
use tracing::{debug, error, info, warn};

use crate::brain::judging::{judge_prompt, judge_system_prompt, parse_judgement};
use crate::brain::rendering::{render_conversation, render_task};
use crate::brain::{Brain, BrainContext, Judgement};
use crate::config::OpenAiCompatBrainConfig;

/// The whole point of a warm-up is that nobody waits for it. If the endpoint
/// has not even accepted the request in this long, the turn it was meant to
/// help has happened anyway.
const WARM_TIMEOUT_S: u64 = 30;

static THINK_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<think\b[^>]*>.*?</think\s*>").expect("the think pattern is a literal")
});
static THINK_OPEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<think\b[^>]*>.*$").expect("the think-open pattern is a literal")
});
static THINK_CLOSE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)^.*</think\s*>").expect("the think-close pattern is a literal")
});

/// Remove reasoning blocks a model leaked into its answer.
///
/// Handles complete `<think>...</think>` blocks, an unterminated opener (the
/// model ran out of budget mid-thought) and a stray closer (chat templates that
/// open the block themselves).
#[must_use]
pub fn strip_thinking(text: &str) -> String {
    let mut cleaned = THINK_BLOCK.replace_all(text, "").into_owned();
    if cleaned.to_lowercase().contains("</think") {
        cleaned = THINK_CLOSE.replace(&cleaned, "").into_owned();
    }
    if cleaned.to_lowercase().contains("<think") {
        cleaned = THINK_OPEN.replace(&cleaned, "").into_owned();
    }
    cleaned.trim().to_owned()
}

/// System persona plus the rendered conversation ending on the trigger.
#[must_use]
pub fn build_messages(ctx: &BrainContext) -> Vec<Value> {
    let frame = format!(
        "You are {} in the Matrix room {}. Below is the recent conversation, one line per \
         message, written as 'name: message'. {} Reply with your message only: no name prefix, \
         no quoting, no stage directions.",
        ctx.me,
        ctx.room_id,
        render_task(ctx)
    );
    let persona = ctx.persona.trim();
    let system = if persona.is_empty() {
        frame
    } else {
        format!("{persona}\n\n{frame}")
    };
    vec![
        json!({ "role": "system", "content": system }),
        json!({ "role": "user", "content": render_conversation(ctx, None) }),
    ]
}

/// The same endpoint, asked for a yes/no instead of a message.
#[must_use]
pub fn build_judge_messages(ctx: &BrainContext) -> Vec<Value> {
    vec![
        json!({ "role": "system", "content": judge_system_prompt(ctx) }),
        json!({ "role": "user", "content": judge_prompt(ctx) }),
    ]
}

fn merge(
    mut payload: Map<String, Value>,
    extra: &std::collections::BTreeMap<String, Value>,
) -> Value {
    for (key, value) in extra {
        payload.insert(key.clone(), value.clone());
    }
    Value::Object(payload)
}

/// Chat-completions adapter (vLLM, ollama, anything speaking the same API).
pub struct OpenAiCompatBrain {
    pub cfg: OpenAiCompatBrainConfig,
    http: reqwest::Client,
    url: String,
    judge_url: String,
    /// Monotonic deadline (in whole seconds since the process started) before
    /// which `warm` does nothing at all.
    warm_until: AtomicU64,
    started: std::time::Instant,
}

impl OpenAiCompatBrain {
    #[must_use]
    pub fn new(cfg: OpenAiCompatBrainConfig, http: reqwest::Client) -> Self {
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let judge_url = cfg.resolved_judge_url();
        Self {
            cfg,
            http,
            url,
            judge_url,
            warm_until: AtomicU64::new(0),
            started: std::time::Instant::now(),
        }
    }

    fn elapsed(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    #[must_use]
    pub fn build_payload(&self, ctx: &BrainContext) -> Value {
        let mut payload = Map::new();
        payload.insert("model".to_owned(), Value::String(self.cfg.model.clone()));
        payload.insert("messages".to_owned(), Value::Array(build_messages(ctx)));
        payload.insert("max_tokens".to_owned(), Value::from(self.cfg.max_tokens));
        merge(payload, &self.cfg.extra_body)
    }

    /// The judge call: a cheap model, no room for prose, its own endpoint.
    ///
    /// `temperature: 0` because "should I speak" is not a creative question and
    /// a sampled judge would answer differently on every identical room.
    #[must_use]
    pub fn build_judge_payload(&self, ctx: &BrainContext) -> Value {
        let model = if self.cfg.judge_model.is_empty() {
            &self.cfg.model
        } else {
            &self.cfg.judge_model
        };
        let mut payload = Map::new();
        payload.insert("model".to_owned(), Value::String(model.clone()));
        payload.insert(
            "messages".to_owned(),
            Value::Array(build_judge_messages(ctx)),
        );
        payload.insert(
            "max_tokens".to_owned(),
            Value::from(self.cfg.judge_max_tokens),
        );
        payload.insert("temperature".to_owned(), Value::from(0));
        merge(payload, &self.cfg.resolved_judge_body())
    }

    /// The cheapest completion that still loads the model: one token.
    ///
    /// Nothing about the room is in it. A warm-up is a request to an endpoint,
    /// not a turn, and the room's conversation has no business being sent
    /// somewhere just to make a GPU allocate memory.
    #[must_use]
    pub fn build_warm_payload(&self) -> Value {
        let mut payload = Map::new();
        payload.insert("model".to_owned(), Value::String(self.cfg.model.clone()));
        payload.insert(
            "messages".to_owned(),
            json!([{ "role": "user", "content": "." }]),
        );
        payload.insert("max_tokens".to_owned(), Value::from(1));
        payload.insert("temperature".to_owned(), Value::from(0));
        merge(payload, &self.cfg.extra_body)
    }

    /// One request, with the timeout the CALLER decides.
    ///
    /// The timeout is a parameter rather than one number on the config because
    /// the two calls are not the same call: a reply is worth waiting a cold
    /// start for, and a judge asking "is there anything here worth saying?" is
    /// not - see [`OpenAiCompatBrainConfig::resolved_judge_timeout`].
    async fn post(
        &self,
        payload: &Value,
        url: &str,
        judge: bool,
        timeout_s: f64,
    ) -> Option<String> {
        let api_key = if judge {
            self.cfg.resolved_judge_api_key()
        } else {
            self.cfg.resolved_api_key()
        };
        let mut request = self
            .http
            .post(url)
            .timeout(Duration::from_secs_f64(timeout_s))
            .json(payload);
        if !api_key.is_empty() {
            request = request.bearer_auth(api_key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(exc) if exc.is_timeout() => {
                error!("brain {url} timed out after {timeout_s:.0} s (cold start?)");
                return None;
            }
            Err(exc) => {
                error!("brain {url} failed: {exc}");
                return None;
            }
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let head: String = body.chars().take(500).collect();
            error!("brain {url} returned HTTP {} : {head}", status.as_u16());
            return None;
        }
        let data: Value = match response.json().await {
            Ok(data) => data,
            Err(exc) => {
                error!("brain {url} returned a body that is not JSON: {exc}");
                return None;
            }
        };
        Self::extract(&data, url)
    }

    fn extract(data: &Value, url: &str) -> Option<String> {
        let Some(choices) = data.get("choices").and_then(Value::as_array) else {
            error!("brain {url} returned no choices");
            return None;
        };
        let Some(content) = choices
            .first()
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
        else {
            error!("brain {url} returned no message content");
            return None;
        };
        let text = strip_thinking(content);
        if text.is_empty() {
            warn!("brain {url} returned an empty answer; staying quiet");
            return None;
        }
        Some(text)
    }

    /// One warm-up request, whose answer is thrown away.
    ///
    /// Spawned rather than awaited: the caller is a typing notice on the sync
    /// loop or the start of a back-off, and neither may be made to wait on a
    /// model that is, by definition, not loaded yet. Everything it needs is
    /// cloned out first (a `reqwest::Client` clone shares one connection pool),
    /// so nothing borrows the brain.
    fn warm_request(&self) {
        let mut request = self
            .http
            .post(&self.url)
            .timeout(Duration::from_secs(WARM_TIMEOUT_S))
            .json(&self.build_warm_payload());
        let api_key = self.cfg.resolved_api_key();
        if !api_key.is_empty() {
            request = request.bearer_auth(api_key);
        }
        let url = self.url.clone();
        tokio::spawn(async move {
            match request.send().await {
                // A warm-up that fails costs one cold start later, nothing else.
                Ok(response) => debug!("warm-up answered HTTP {}", response.status().as_u16()),
                Err(exc) => debug!("warm-up of {url} failed: {exc}"),
            }
        });
    }
}

#[async_trait]
impl Brain for OpenAiCompatBrain {
    async fn reply(&self, ctx: &BrainContext) -> Option<String> {
        self.post(
            &self.build_payload(ctx),
            &self.url,
            false,
            self.cfg.cold_start_timeout_s,
        )
        .await
    }

    async fn judge(&self, ctx: &BrainContext) -> Judgement {
        let answer = self
            .post(
                &self.build_judge_payload(ctx),
                &self.judge_url,
                true,
                self.cfg.resolved_judge_timeout(),
            )
            .await;
        let judgement = parse_judgement(answer.as_deref());
        info!(
            "judge ({} via {}) says {}: {} (urgency {})",
            ctx.room_id, self.judge_url, judgement.speak, judgement.why, judgement.urgency
        );
        judgement
    }

    /// Fire one throwaway completion so an on-demand model is up in time.
    ///
    /// Fire and forget on purpose: the caller is a typing notice or the start
    /// of a back-off, and neither may be made to wait on a model that is, by
    /// definition, not loaded yet.
    async fn warm(&self, reason: &str) {
        if !self.cfg.warm_on_intent {
            return;
        }
        let now = self.elapsed();
        let until = self.warm_until.load(Ordering::Relaxed);
        if now < until {
            // INFO, not DEBUG: this is a decision not to send a request, and
            // every other decision in this project is in the log at INFO.
            info!(
                "warm-up skipped, {} s of cooldown left: {reason}",
                until - now
            );
            return;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cooldown = self.cfg.warm_cooldown_s.max(0.0) as u64;
        self.warm_until.store(now + cooldown, Ordering::Relaxed);
        info!("warming {} ({}): {reason}", self.cfg.model, self.url);
        self.warm_request();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::brain::Occasion;
    use crate::events::RoomEvent;

    const ME: &str = "@bot-a:example.com";
    const ROOM_ID: &str = "!room:example.com";

    /// A real chat-completions endpoint on localhost, recording what it was
    /// sent. Deliberately a socket rather than a mock: the assertion that
    /// matters is what actually went over the wire.
    struct FakeEndpoint {
        base_url: String,
        requests: Arc<Mutex<Vec<Value>>>,
        headers: Arc<Mutex<Vec<String>>>,
        shutdown: tokio::sync::oneshot::Sender<()>,
    }

    #[derive(Clone)]
    struct Answer {
        status: u16,
        body: String,
    }

    impl FakeEndpoint {
        async fn start(answer: Answer) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
            let port = listener.local_addr().expect("bound").port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let headers = Arc::new(Mutex::new(Vec::new()));
            let (shutdown, mut stop) = tokio::sync::oneshot::channel();
            let seen = Arc::clone(&requests);
            let seen_headers = Arc::clone(&headers);
            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        () = async { (&mut stop).await.ok(); } => break,
                        accepted = listener.accept() => accepted,
                    };
                    let Ok((mut socket, _)) = accepted else { break };
                    let mut raw = Vec::new();
                    let mut buffer = [0u8; 4096];
                    while let Ok(read) = socket.read(&mut buffer).await {
                        if read == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&raw).into_owned();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let length: usize = head
                                .lines()
                                .find_map(|line| {
                                    line.strip_prefix("content-length: ")
                                        .or_else(|| line.strip_prefix("Content-Length: "))
                                })
                                .and_then(|value| value.trim().parse().ok())
                                .unwrap_or(0);
                            if body.len() >= length {
                                if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                                    seen.lock().expect("never poisoned").push(parsed);
                                }
                                seen_headers
                                    .lock()
                                    .expect("never poisoned")
                                    .push(head.to_owned());
                                break;
                            }
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: \
                         {}\r\nconnection: close\r\n\r\n{}",
                        answer.status,
                        answer.body.len(),
                        answer.body
                    );
                    let _ignored = socket.write_all(response.as_bytes()).await;
                    let _ignored = socket.shutdown().await;
                }
            });
            Self {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                requests,
                headers,
                shutdown,
            }
        }

        fn answering(content: &str) -> Answer {
            Answer {
                status: 200,
                body: json!({ "choices": [{ "message": { "role": "assistant", "content": content } }] })
                    .to_string(),
            }
        }

        fn requests(&self) -> Vec<Value> {
            self.requests.lock().expect("never poisoned").clone()
        }

        fn headers(&self) -> Vec<String> {
            self.headers.lock().expect("never poisoned").clone()
        }

        fn stop(self) {
            let _ignored = self.shutdown.send(());
        }
    }

    fn context(body: &str) -> BrainContext {
        let trigger = RoomEvent {
            event_id: "$trigger".to_owned(),
            room_id: ROOM_ID.to_owned(),
            sender: "@human:example.com".to_owned(),
            sender_display: Some("Alex".to_owned()),
            body: body.to_owned(),
            formatted_body: None,
            msgtype: "m.text".to_owned(),
            ts: 1.0,
            thread_root: None,
            reply_to: None,
            reply_is_fallback: false,
            mentions: BTreeSet::new(),
            is_bot: false,
        };
        let earlier = RoomEvent {
            event_id: "$earlier".to_owned(),
            body: "earlier line".to_owned(),
            ..trigger.clone()
        };
        BrainContext {
            persona: "You are bot-a.".to_owned(),
            me: ME.to_owned(),
            room_id: ROOM_ID.to_owned(),
            trigger: trigger.clone(),
            history: vec![earlier, trigger],
            thread: Vec::new(),
            occasion: Occasion::Reply,
            note: String::new(),
            want_urgency: false,
        }
    }

    fn config(base_url: &str) -> OpenAiCompatBrainConfig {
        OpenAiCompatBrainConfig {
            base_url: base_url.to_owned(),
            model: "qwen3.8-27b".to_owned(),
            api_key: String::new(),
            cold_start_timeout_s: 30.0,
            extra_body: BTreeMap::from([(
                "chat_template_kwargs".to_owned(),
                json!({ "enable_thinking": false }),
            )]),
            max_tokens: 600,
            judge_model: String::new(),
            judge_max_tokens: 60,
            judge_base_url: String::new(),
            judge_api_key: String::new(),
            judge_extra_body: BTreeMap::new(),
            judge_timeout_s: 0,
            warm_on_intent: false,
            warm_cooldown_s: 120.0,
        }
    }

    fn brain(cfg: OpenAiCompatBrainConfig) -> OpenAiCompatBrain {
        OpenAiCompatBrain::new(cfg, reqwest::Client::new())
    }

    #[tokio::test]
    async fn extra_body_reaches_the_endpoint() {
        // Without chat_template_kwargs Qwen3.8 thinks its whole budget away, so
        // this is the assertion that keeps the local model usable.
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering("hi")).await;
        let brain = brain(config(&endpoint.base_url));
        assert_eq!(brain.reply(&context("ping")).await.as_deref(), Some("hi"));
        let body = endpoint.requests().first().cloned().expect("one request");
        assert_eq!(
            body["chat_template_kwargs"],
            json!({ "enable_thinking": false })
        );
        assert_eq!(body["model"], json!("qwen3.8-27b"));
        assert_eq!(body["max_tokens"], json!(600));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .expect("a system prompt")
                .contains("You are bot-a.")
        );
        assert_eq!(
            body["messages"][1]["content"],
            json!("Alex: earlier line\nAlex: ping")
        );
        endpoint.stop();
    }

    #[tokio::test]
    async fn the_api_key_is_sent_as_a_bearer_token() {
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering("hi")).await;
        let mut cfg = config(&endpoint.base_url);
        cfg.api_key = "sekrit".to_owned();
        brain(cfg).reply(&context("ping")).await;
        let head = endpoint.headers().first().cloned().expect("one request");
        assert!(
            head.to_lowercase().contains("authorization: bearer sekrit"),
            "{head}"
        );
        endpoint.stop();
    }

    #[tokio::test]
    async fn a_thinking_block_is_stripped() {
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering(
            "<think>weighing the options</think>\n\nThe answer is 42.",
        ))
        .await;
        assert_eq!(
            brain(config(&endpoint.base_url))
                .reply(&context("ping"))
                .await
                .as_deref(),
            Some("The answer is 42.")
        );
        endpoint.stop();
    }

    #[tokio::test]
    async fn every_kind_of_useless_answer_is_silence() {
        for content in ["   \n ", "<think>still thinking and out of tokens"] {
            let endpoint = FakeEndpoint::start(FakeEndpoint::answering(content)).await;
            assert!(
                brain(config(&endpoint.base_url))
                    .reply(&context("ping"))
                    .await
                    .is_none(),
                "{content:?} should have been silence"
            );
            endpoint.stop();
        }
        let endpoint = FakeEndpoint::start(Answer {
            status: 500,
            body: json!({ "error": "model not loaded" }).to_string(),
        })
        .await;
        assert!(
            brain(config(&endpoint.base_url))
                .reply(&context("ping"))
                .await
                .is_none()
        );
        endpoint.stop();

        let endpoint = FakeEndpoint::start(Answer {
            status: 200,
            body: json!({ "choices": [] }).to_string(),
        })
        .await;
        assert!(
            brain(config(&endpoint.base_url))
                .reply(&context("ping"))
                .await
                .is_none()
        );
        endpoint.stop();
    }

    #[tokio::test]
    async fn an_endpoint_that_is_not_there_is_silence() {
        let mut cfg = config("http://127.0.0.1:1/v1");
        cfg.cold_start_timeout_s = 2.0;
        assert!(brain(cfg).reply(&context("ping")).await.is_none());
    }

    #[test]
    fn strip_thinking_handles_the_shapes_models_emit() {
        assert_eq!(strip_thinking("plain answer"), "plain answer");
        assert_eq!(strip_thinking("<think>a</think>b"), "b");
        assert_eq!(strip_thinking("<think>a</think>b<think>c</think>d"), "bd");
        assert_eq!(
            strip_thinking("thought so far</think>\nreal answer"),
            "real answer"
        );
        assert_eq!(strip_thinking("<think>never closed"), "");
    }

    #[tokio::test]
    async fn the_judge_asks_the_cheap_model_for_a_verdict() {
        // Same endpoint, different question: a cheap model, a token budget that
        // only fits a verdict, and temperature 0 so an identical room does not
        // get a different answer every time it is asked.
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering(
            "yes: nobody has answered the question about the deploy",
        ))
        .await;
        let mut cfg = config(&endpoint.base_url);
        cfg.judge_model = "qwen3.8-0.5b".to_owned();
        let judgement = brain(cfg)
            .judge(&context("does anyone know about the deploy?"))
            .await;
        assert!(judgement.speak);
        assert_eq!(
            judgement.why,
            "nobody has answered the question about the deploy"
        );
        let request = endpoint.requests().first().cloned().expect("one request");
        assert_eq!(request["model"], json!("qwen3.8-0.5b"));
        assert_eq!(request["max_tokens"], json!(60));
        assert_eq!(request["temperature"], json!(0));
        // `extra_body` still applies: without enable_thinking=false Qwen3.8
        // spends the whole 60-token budget thinking and answers nothing at all.
        assert_eq!(
            request["chat_template_kwargs"],
            json!({ "enable_thinking": false })
        );
        let system = request["messages"][0]["content"]
            .as_str()
            .expect("a system prompt");
        assert!(system.contains("You are bot-a."));
        assert!(system.contains("deciding whether to speak at all"));
        let user = request["messages"][1]["content"]
            .as_str()
            .expect("a user prompt");
        assert!(user.contains("does anyone know about the deploy?"));
        assert!(user.contains("Answer exactly `yes: <one line>` or `no: <one line>`."));
        endpoint.stop();
    }

    #[tokio::test]
    async fn the_judge_falls_back_to_the_reply_model() {
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering("no: nothing to add")).await;
        assert!(
            !brain(config(&endpoint.base_url))
                .judge(&context("x"))
                .await
                .speak
        );
        assert_eq!(
            endpoint.requests().first().expect("one request")["model"],
            json!("qwen3.8-27b")
        );
        endpoint.stop();
    }

    #[tokio::test]
    async fn a_thinking_judge_is_still_parsed_and_a_broken_one_is_a_no() {
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering(
            "<think>they asked about ports</think>\nyes: I know the port",
        ))
        .await;
        let judgement = brain(config(&endpoint.base_url)).judge(&context("x")).await;
        assert!(judgement.speak);
        assert_eq!(judgement.why, "I know the port");
        endpoint.stop();

        let endpoint = FakeEndpoint::start(Answer {
            status: 500,
            body: "{}".to_owned(),
        })
        .await;
        let judgement = brain(config(&endpoint.base_url)).judge(&context("x")).await;
        assert!(!judgement.speak);
        assert!(
            judgement.why.contains("answered nothing"),
            "{}",
            judgement.why
        );
        endpoint.stop();
    }

    #[tokio::test]
    async fn the_judge_can_live_on_a_different_endpoint_entirely() {
        // The owner's setup: a small resident model judges, and the 27B is only
        // ever loaded to speak.
        let big = FakeEndpoint::start(FakeEndpoint::answering("hi")).await;
        let small = FakeEndpoint::start(FakeEndpoint::answering("no: they have it covered")).await;
        let mut cfg = config(&big.base_url);
        cfg.api_key = "big-key".to_owned();
        cfg.judge_base_url = small.base_url.clone();
        cfg.judge_model = "qwen3:4b".to_owned();
        cfg.judge_api_key = "small-key".to_owned();
        let brain = brain(cfg);
        assert!(!brain.judge(&context("x")).await.speak);
        brain.reply(&context("x")).await;

        assert_eq!(
            small.requests().len(),
            1,
            "the judge went to the wrong endpoint"
        );
        assert_eq!(small.requests()[0]["model"], json!("qwen3:4b"));
        assert!(
            small.headers()[0]
                .to_lowercase()
                .contains("bearer small-key")
        );
        assert_eq!(
            big.requests().len(),
            1,
            "the big model was loaded for a yes/no question"
        );
        assert!(big.headers()[0].to_lowercase().contains("bearer big-key"));
        big.stop();
        small.stop();
    }

    /// The warm-ups that have reached the endpoint, waited for.
    ///
    /// One second is a hundred times what a loopback request takes and still
    /// long enough that a genuinely missing request fails the test rather than
    /// flaking it.
    async fn wait_for_warm_ups(endpoint: &FakeEndpoint) -> Vec<Value> {
        for _ in 0..100 {
            let warm: Vec<Value> = endpoint
                .requests()
                .into_iter()
                .filter(|body| body["max_tokens"] == json!(1))
                .collect();
            if !warm.is_empty() {
                // Give any extra warm-up the cooldown should have refused the
                // same chance to arrive.
                tokio::time::sleep(Duration::from_millis(100)).await;
                return endpoint
                    .requests()
                    .into_iter()
                    .filter(|body| body["max_tokens"] == json!(1))
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Vec::new()
    }

    #[tokio::test]
    async fn nothing_is_warmed_unless_the_endpoint_is_on_demand() {
        // On an always-on model a warm-up is a request that buys nothing.
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering("hi")).await;
        brain(config(&endpoint.base_url))
            .warm("a human is typing")
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(endpoint.requests().is_empty());
        endpoint.stop();
    }

    #[tokio::test]
    async fn warming_fires_one_throwaway_token_and_carries_no_conversation() {
        let endpoint = FakeEndpoint::start(FakeEndpoint::answering("hi")).await;
        let mut cfg = config(&endpoint.base_url);
        cfg.warm_on_intent = true;
        let brain = brain(cfg);
        brain.warm("a human is typing").await;
        // The cooldown turns a typed paragraph into one request: somebody
        // typing produces a notice every few seconds.
        for _ in 0..4 {
            brain.warm("still typing").await;
        }
        // The request is spawned, not awaited - the whole point is that nobody
        // waits for it - so the assertion does the waiting instead.
        let warm = wait_for_warm_ups(&endpoint).await;
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0]["model"], json!("qwen3.8-27b"));
        assert_eq!(
            warm[0]["messages"],
            json!([{ "role": "user", "content": "." }])
        );
        // `extra_body` still applies: it is what makes the endpoint answer.
        assert_eq!(
            warm[0]["chat_template_kwargs"],
            json!({ "enable_thinking": false })
        );
        endpoint.stop();
    }

    #[tokio::test]
    async fn a_warm_up_that_fails_costs_nothing_but_a_cold_start() {
        let mut cfg = config("http://127.0.0.1:1/v1");
        cfg.warm_on_intent = true;
        cfg.cold_start_timeout_s = 2.0;
        // It is a hint about the future, never a step in a turn: it returns at
        // once, and the request it spawned fails in the background.
        brain(cfg).warm("a human is typing").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
