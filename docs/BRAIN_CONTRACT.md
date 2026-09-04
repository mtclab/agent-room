# Writing your own brain

A brain answers two questions and nothing else:

- **`reply(ctx)`** - given what happened in the room, what should I say?
- **`judge(ctx)`** - nobody addressed me; how much would I add by speaking
  here at all?

Everything else - when to speak, back-offs, budgets, threading, mentions, read
receipts, restarts - belongs to the connector and is deliberately not your
problem. A brain is a function from a conversation to a string, plus a way to
close whatever it opened.

**You probably do not need one.** Anything that speaks the OpenAI chat
completions API already works through the shipped `openai_compat` adapter:
ollama, LM Studio, vLLM, llama.cpp's server, most hosted endpoints. Write an
adapter when your thing is genuinely a different shape - a local process, an
agent framework with its own session, an HTTP API that is not OpenAI's.

A brain is Rust and lives in the binary. There is no plugin loader and no
dynamic loading: a new brain is a file in `src/brain/`, two small edits, and a
build. If you would rather not fork, the other honest option is to put your
thing behind a small OpenAI-compatible HTTP shim and use the shipped adapter.

## The contract

`src/brain/mod.rs`, in full:

```rust
#[async_trait]
pub trait Brain: Send + Sync {
    /// Return the message to post, or None to stay quiet.
    async fn reply(&self, ctx: &BrainContext) -> Option<String>;

    /// Cheap, fresh-context "do I add anything nobody has said?".
    async fn judge(&self, _ctx: &BrainContext) -> Judgement {
        Judgement::no("this brain has no judge, so it never speaks unprompted")
    }

    /// A turn is probably coming. Get ready if that means anything to you.
    async fn warm(&self, _reason: &str) {}

    /// Release any resources (HTTP clients, subprocesses).
    async fn close(&self) {}
}
```

Three rules that are worth more than the signatures:

1. **`None` is a legitimate answer, and it is the safe one.** Every failure -
   a timeout, an HTTP 500, an empty completion, a missing binary - returns
   `None` and the connector carries on. There is nothing to catch here and
   nothing that should try: `reply` returns an `Option`, not a `Result`, so a
   failure is a value rather than something that could take a turn down with
   it. A brain that invents an apology puts noise in a room full of people.
2. **`judge` may be skipped entirely.** The default implementation always says
   no, which means your agent simply never speaks unprompted. That is a fine
   agent. Implement it when you want tier 2, and make it CHEAPER than `reply` -
   it runs on lines nobody addressed to you.
3. **Do not post, do not read the room, do not sleep.** You are handed
   everything you get and what you return is what is posted.

### `BrainContext`

| Field | What it is |
|---|---|
| `persona: String` | the text of the persona file, already stripped |
| `me: String` | your agent's user id, as the homeserver confirmed it |
| `room_id: String` | the room this turn belongs to |
| `trigger: RoomEvent` | the event being answered - or, when nothing is being answered, an ANCHOR: where the conversation stands |
| `history: Vec<RoomEvent>` | recent room events, oldest first, including the trigger |
| `thread: Vec<RoomEvent>` | the trigger's thread, oldest first; empty when it is not in one |
| `occasion: Occasion` | why you are being asked (below) |
| `note: String` | what the occasion is about when the room is not: the impulse, the loop, the thought. Empty for `Reply` and `Unaddressed` |
| `want_urgency: bool` | the connector is collecting inner thoughts; see `Judgement` |
| `speak_threshold: u8` | the score at which YOUR judgement is a yes here (`policy.speak_threshold` shifted by `policy.chattiness`). The operator's, not yours |
| `participants: usize` | how many people and agents are joined to this room; 0 before the member list has arrived |

`ctx.i_took_part()` answers "am I already part of this exchange?" off the thread
(or the room's recent lines when there is no thread). It is one of the cues the
shipped judge prompt states rather than makes the model infer.

A `RoomEvent` (`src/events.rs`) carries `event_id`, `room_id`, `sender`,
`sender_display`, `body`, `msgtype`, `ts` (epoch seconds), `thread_root`,
`reply_to`, `mentions` and `is_bot`, plus `display()` (the best name to show a
model) and `thread_root_or_self()`.

`occasion` is the field to read, because half of them are not answers:

| `Occasion` | What happened |
|---|---|
| `Reply` | somebody addressed you; this answer is going to be posted |
| `Unaddressed` | tier 2: nobody addressed you; would you add anything? |
| `Impulse` | something happened to YOU (`note`); is it worth telling them? |
| `Followup` | you left something open (`note`) and nobody came back to it |
| `InnerThought` | you kept wanting to say something (`note`) and it added up |
| `Heartbeat` | the room has been quiet on a timer (off by default) |

`Occasion::is_answering()` is the test that matters: for everything but `Reply`
and `Unaddressed`, "answer the last line" is the wrong instruction to give your
model, because nobody said anything to you. The shipped `render_task(ctx)`
writes the right line for each, and `render_conversation(ctx, limit)` already
leaves the anchor where it happened instead of putting it last as if it were a
question.

### `Judgement`

```rust
pub struct Judgement {
    pub speak: bool,
    /// 0-9: how much this brain would add here. 0 whenever nothing usable
    /// came back.
    pub score: u8,
    pub why: String,
    /// 0-3: how much this brain wanted to say something, whatever it decided.
    pub urgency: i32,
}
```

**A score, not a verdict.** The judge is asked how much it would add, 0 to 9,
and the operator's configuration is what turns that into speech - which is why
`Judgement::scored(score, ctx.speak_threshold, why, urgency)` is the
constructor, and why the threshold comes in on the context. A binary "should
you speak?" is biased to silence: the first room to hold two agents got "no:
the conversation has naturally settled" from a judge that had just been told to
talk (see `docs/DESIGN.md`, "The judge scores, the connector decides").

| score | what it means |
|---|---|
| 9-7 | I clearly should: invited, asked, or squarely my subject |
| 6-4 | I could add something |
| 3-0 | nothing to add, the thread is closed, or it is somebody else's |

`Judgement::no(why)` is the other constructor most adapters need: score 0,
speak false, whatever the threshold. The `why` is logged, so write it for the
person reading the log at midnight.

`urgency` is 0-3 (`brain::MAX_URGENCY`) and only matters when `ctx.want_urgency`
is set (the operator turned on `policy.inner_thoughts`). It is "how much did you
want to say something, whatever you scored" - and it is what lets a run of nos
add up to one message. `parse_judgement` reads it off the end of the score line
(`score: 2 - they have it covered | urgency 2`), and a mangled suffix costs the
urgency and never the score.

### Promising to come back to something

A brain returns text, not metadata, with exactly one exception: writing

    [[followup: check whether the deploy actually went out]]

anywhere in a message opens an OPEN LOOP. The marker is stripped before the
message is posted - the room never sees it - and twenty minutes to three hours
later, if a human is around and nobody has come back to that thread, the
connector may ask you to follow it up (`Occasion::Followup`, the text in
`ctx.note`). It happens at most once per loop.

Use it sparingly, for something you actually said you would check. A message
that ends in a question mark already opens a loop without any marker, so you do
not need one to ask a question.

The shipped frame tells the model this in one sentence
(`brain::rendering::FOLLOWUP_HINT`), because no model writes a marker it has
never been told about - and it is left OFF a `Followup` turn, where the
connector refuses to open another loop anyway.

### `warm(reason)`

Optional, and a no-op by default. The connector calls it when a human starts
typing in a watched room and when a back-off begins: "a turn is probably
coming". Implement it only if that means something to you - the shipped
`openai_compat` adapter uses it to preload an on-demand model. It must return
immediately: spawn the work and let the call come back, because it is a hint
about the future, not a step in a turn.

## The helpers you should reuse

Both shipped adapters use these, which is why they cannot drift apart. They are
in `crate::brain`:

- **`render_conversation(ctx, limit)`** - the conversation as `name: message`
  lines, oldest first, always ending on the trigger (the thread when there is
  one, otherwise the room history). `limit` caps how many earlier lines come
  with it; the trigger is always kept.
- **`render_event(ev)`** - one such line, if you are building something else.
- **`rendering::render_task(ctx)`** - the one line that says why you are being
  asked (per occasion), including the follow-up hint. Put it in your frame and
  your adapter cannot drift from the shipped two.
- **`judge_system_prompt(ctx)` / `judge_prompt(ctx)`** - the persona plus the
  judging frame, and the room plus the question. Same question for every brain.
- **`parse_judgement(text, ctx.speak_threshold)`** - reads `score: N - reason`
  and treats anything else as 0, with a reason saying so. Use it rather than
  writing your own parser: "anything unparseable is silence" is a rule of the
  project, not a detail of one adapter. It takes an `Option<&str>`, so "the
  endpoint returned nothing at all" goes through the same door.

## Adding a kind

Three edits, all in a clone of this repository.

**1. `src/config.rs`** - the kind and its section. `BrainKind` is what a config
file's `brain.kind` parses into, and the section is `deny_unknown_fields` so a
typo in a config key is an error at start-up rather than a setting that silently
does nothing:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainKind {
    OpenaiCompat,
    ClaudeCode,
    Echo,
    MyBrain,          // new
}

/// A brain of my own: an HTTP endpoint that answers with the next message.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MyBrainConfig {
    pub url: String,
    #[serde(default = "default_my_brain_timeout")]
    pub timeout_s: f64,
}

pub struct BrainConfig {
    pub kind: BrainKind,
    // ...
    #[serde(default)]
    pub my_brain: Option<MyBrainConfig>,   // new
}
```

**2. `src/brain/mod.rs`** - declare the module and add one arm to
`build_brain`. The arm is where "you named a kind but wrote no section for it"
becomes a config error rather than a panic:

```rust
pub mod my_brain;
pub use my_brain::MyBrain;

// ... in build_brain:
    BrainKind::MyBrain => {
        let section = cfg.my_brain.as_ref().ok_or_else(|| {
            ConfigError::Invalid("brain.my_brain section is missing".to_owned())
        })?;
        Ok(Arc::new(MyBrain::new(section.clone(), http)))
    }
```

Take the `http` client that is handed in rather than building your own: it is
the one the `tls:` block configured, so a friend behind mTLS gets their client
certificate on your endpoint too, for free.

**3. `src/brain/my_brain.rs`** - the adapter itself.

Then `cargo build --release` and point `brain.kind` at it. `agent-room doctor`
will not check a brain kind it does not know; add a row for it in
`src/doctor.rs` if you want one.

## A worked example: any HTTP endpoint

A real one - it posts the conversation as JSON and expects `{"text": "..."}`
back. Everything about it is the pattern: reuse the rendering, return `None` on
every failure, put a timeout on the request, and hold one client.

```rust
//! A brain that asks an HTTP endpoint of your own for the next message.

use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::brain::judging::{judge_prompt, judge_system_prompt, parse_judgement};
use crate::brain::rendering::render_task;
use crate::brain::{Brain, BrainContext, Judgement, render_conversation};
use crate::config::MyBrainConfig;

pub struct MyBrain {
    cfg: MyBrainConfig,
    http: reqwest::Client,
}

impl MyBrain {
    #[must_use]
    pub fn new(cfg: MyBrainConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    /// One request, or None. Every failure ends here and nowhere else.
    async fn ask(&self, body: Value) -> Option<String> {
        let response = self
            .http
            .post(&self.cfg.url)
            .json(&body)
            // Never leave this off. A brain with no timeout is a room with one
            // agent permanently mid-sentence.
            .timeout(std::time::Duration::from_secs_f64(self.cfg.timeout_s))
            .send()
            .await
            .inspect_err(|exc| error!("brain {} failed: {exc}", self.cfg.url))
            .ok()?;
        if !response.status().is_success() {
            error!("brain {} answered HTTP {}", self.cfg.url, response.status());
            return None;
        }
        let data: Value = response
            .json()
            .await
            .inspect_err(|exc| error!("brain {} returned no JSON: {exc}", self.cfg.url))
            .ok()?;
        let text = data.get("text").and_then(Value::as_str).unwrap_or("").trim();
        if text.is_empty() {
            warn!("brain {} returned an empty answer; staying quiet", self.cfg.url);
            return None;
        }
        Some(text.to_owned())
    }
}

#[async_trait]
impl Brain for MyBrain {
    async fn reply(&self, ctx: &BrainContext) -> Option<String> {
        self.ask(json!({
            "persona": ctx.persona,
            "me": ctx.me,
            "room": ctx.room_id,
            "task": render_task(ctx),
            "conversation": render_conversation(ctx, None),
        }))
        .await
    }

    async fn judge(&self, ctx: &BrainContext) -> Judgement {
        let answer = self
            .ask(json!({
                "system": judge_system_prompt(ctx),
                "prompt": judge_prompt(ctx),
            }))
            .await;
        // The shared parser, not one of your own: anything that is not a
        // score is a 0, and that rule belongs to the project. The threshold
        // comes from the operator's config, through the context.
        parse_judgement(answer.as_deref(), ctx.speak_threshold)
    }
}
```

That agent answers when it is addressed and joins tier 2. Drop the `judge` and
it answers only when addressed, which is a perfectly good agent - the default
implementation says no for ever.

## Before you point it at the room

- `agent-room doctor --config ...` will not check a brain kind it does not know;
  add a row for it in `src/doctor.rs` if you want one.
- Run it against your own room first: mention your agent from your own account
  and read the connector's log. It prints the rule that decided, the budget it
  spent, and what the brain returned.
- Watch the first bot-to-bot exchange. Budgets stop a flood; they cannot stop
  noise, and an adapter that always finds something to add is exactly what this
  project exists to avoid.
- Write the unit tests the shipped adapters have. `echo` is 190 lines including
  its tests and is the smallest complete example in the tree.
