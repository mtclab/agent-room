# Matrix agent-chat redesign - research report

## 1. Python/Node Matrix bot frameworks, 2026

| Project | Latest | Last commit | Maintained | Threads | E2EE | Multi-account in one process | Appservice |
|---|---|---|---|---|---|---|---|
| [matrix-nio](https://github.com/matrix-nio/matrix-nio) | 0.26.0 (2026-07-23) | 2026-07-23 | Low-activity but alive (2 maintainers: poljar, PaarthShah) | Yes - README lists threading; `room_get_threads`, `room_get_event_relations`, `update_receipt_marker(thread_id=)` | Yes via libolm/`[e2e]`; **no cross-signing** | Yes - just N `AsyncClient` objects in one asyncio loop | No (client-only) |
| [mautrix-python](https://github.com/mautrix/python) | 0.21.1 (2026-07-05) | 2026-07-05 | Yes (tulir) | Yes | Yes (medium-level e2ee framework) | Yes - designed for it | **Yes** - AppService + `IntentAPI` per virtual user |
| [maubot](https://github.com/maubot/maubot) | v0.6.0 (2024-11-17) | 2026-07-09 | Slow but alive | Via mautrix-python | Yes (`maubot[encryption]`) | **Yes** - "client" = one Matrix account, "instance" = plugin x client; many clients per server | Appservice token support since v0.4.2 |
| [matrix-bot-sdk](https://github.com/turt2live/matrix-bot-sdk) (Node) | 0.8.0 | 2026-03-27 | Yes | Yes | Yes (Rust crypto storage provider); MSC3202 encrypted appservices | Via `Appservice` class | **Yes** |
| [simplematrixbotlib](https://codeberg.org/imbev/simplematrixbotlib) | 2.13.1 (2026-03-29) | 2026-03-28 | Yes (moved GitHub -> Codeberg) | Thin nio wrapper | Via nio | One bot per `Bot()` | No |
| [opsdroid matrix connector](https://github.com/opsdroid/connector-matrix) | in core | opsdroid core 2025-11-24 | **Standalone repo archived 2019**; core slow | Not documented | Yes | Single mxid per connector | No |

**Recommendation for our shape:** matrix-nio for a per-agent client daemon (we already have it installed, threads + typing + receipts all present), or mautrix-python if we go appservice.

### Application Services - the "14 personas, one daemon" primitive

Spec: [Application Service API v1.16](https://spec.matrix.org/v1.16/application-service-api/) · Synapse: [application_services.html](https://element-hq.github.io/synapse/latest/application_services.html)

- **Registration YAML** (path listed in Synapse's `app_service_config_files`, restart required): `id`, `url`, `as_token` (AS -> HS), `hs_token` (HS -> AS), `sender_localpart`, `namespaces: {users, aliases, rooms}` where each entry is `{exclusive: bool, regex: "@_agent_.*:example.com"}`. Optional `rate_limited: <bool>` and `receive_ephemeral: <bool>` (default false; opts into typing/receipts/presence).
- **Push, not poll.** The homeserver calls `PUT /_matrix/app/v1/transactions/{txnId}` on our `url` with `{"events":[...], "ephemeral":[...]}`, authenticated with `Authorization: Bearer <hs_token>`. Idempotent by `txnId`. **No `/sync` loop at all.** Legacy `/_matrix/app/v1/` fallback exists if the new paths 404.
- **Masquerading:** any C-S API call with the `as_token` plus `?user_id=@_agent_coder:example.com` acts *as* that user - send, join, typing, receipts, profile. The user must match a registered namespace. No per-agent login, no 14 access tokens, no password rotation.
- **Rate limiting:** the spec's `rate_limited` field is "whether requests from masqueraded users are rate-limited. **The sender is excluded**" - i.e. the `sender_localpart` bot is always exempt; set `rate_limited: false` to exempt the virtual users too. (Synapse honoured this incorrectly for joins until [v1.19.1](https://github.com/matrix-org/synapse/issues/8138); room-creation rate limits for appservices are [still an open Synapse issue #19149](https://github.com/element-hq/synapse/issues/19149).)

**Verdict: yes, an appservice is the right primitive** for 14 agent personas. One process, one token, push delivery, no rate limits, users auto-created on first use, no provisioning script. Cost: E2EE for appservices needs [MSC3202](https://github.com/matrix-org/matrix-spec-proposals/pull/3202) (matrix-bot-sdk implements it; nio does not), and you need config-file access + a Synapse restart. If the room stays unencrypted, this is clearly the cleanest design. Related: [MSC4144 per-message profiles](https://github.com/matrix-org/matrix-spec-proposals/pull/4144) (tulir, in review, updated 2026-05-09) would let *one* account render 14 distinct display names/avatars per message - worth watching but not a substitute today.

---

## 2. Matrix features for agent chat

| Feature | How | URL |
|---|---|---|
| Threads (MSC3440, now spec) | `m.relates_to: {rel_type:"m.thread", event_id:<root>, is_falling_back:true, "m.in_reply_to":{event_id:<latest>}}`; read back with `GET /_matrix/client/v1/rooms/{roomId}/relations/{eventId}/m.thread` | [spec #threading](https://spec.matrix.org/v1.16/client-server-api/#threading) |
| Intentional mentions (MSC3952) | Event content carries `m.mentions: {user_ids:[...], room:true}`. **Detect "I was mentioned" by testing membership in `content["m.mentions"]["user_ids"]`**, not by substring-matching the display name. Fall back to `formatted_body` `matrix.to` pill parsing for old clients. | [spec, same page](https://spec.matrix.org/v1.16/client-server-api/#user-and-room-mentions) |
| Typing ("agent is thinking") | `PUT /_matrix/client/v3/rooms/{roomId}/typing/{userId}` with `{typing:true, timeout:30000}`; nio `AsyncClient.room_typing()`. Refresh before timeout for long LLM turns. | nio API docs |
| Read receipts | `POST /_matrix/client/v3/rooms/{roomId}/receipt/m.read/{eventId}`; nio `update_receipt_marker(..., thread_id=)` / `room_read_markers()`. Use as the "I have consumed this" marker so restarts do not re-answer. | [nio API](https://matrix-nio.readthedocs.io/en/latest/nio.html) |
| Reactions | `m.reaction` with `rel_type: "m.annotation"`, `key: "👀"` - cheap ack that an agent picked up a task. | spec, relations |
| `m.notice` | Convention: bots send `msgtype: "m.notice"` so clients suppress notifications and other bots can filter it. opsdroid has a `send_m_notice` config option for exactly this. | [opsdroid matrix connector](https://docs.opsdroid.dev/en/stable/connectors/matrix.html) |
| Rate limiting | `rc_message` defaults `per_second: 0.2, burst_count: 10` - 14 agents replying at once **will** hit this on normal accounts. | [Synapse config manual](https://element-hq.github.io/synapse/latest/usage/configuration/config_documentation.html) |

**Provisioning + rate-limit escape (non-appservice path)** - [Synapse user admin API](https://element-hq.github.io/synapse/latest/admin_api/user_admin_api.html):
- `PUT /_synapse/admin/v2/users/@agent_coder:example.com` - body takes `password`, `displayname`, `avatar_url`, `admin`, `deactivated`, `locked`, and **`user_type`** which accepts `"support"` and **`"bot"`**. 201 on create, 200 on modify.
- `POST /_synapse/admin/v1/users/{userId}/login` -> `{"access_token": "..."}` (optional `valid_until_ms`) - get a token without knowing the password.
- `POST /_synapse/admin/v1/users/{userId}/override_ratelimit` with `{"messages_per_second":0,"burst_count":0}` - **0 means no limit**. `GET`/`DELETE` on the same path. This is the per-user exemption; run it once per agent account.

---

## 3. Prior art: LLM agents on Matrix

| Project | Stack | Reply trigger | Loop prevention | Context |
|---|---|---|---|---|
| [baibot](https://github.com/etkecc/baibot) (etke.cc, Rust, commit 2026-09-01) | matrix-sdk | Group rooms: `!bai` prefix or explicit mention; DMs: allowed users only | Only considers messages from *allowed users*; when mentioned in a thread/reply chain it considers all messages | Auto-trims oldest messages on whole-turn boundaries to fit the model window; can attach sender metadata so the model can tell participants apart |
| [MindRoom](https://www.nijho.lt/post/mindroom/) ([repo](https://github.com/mindroom-ai/mindroom)) | Python + **matrix-nio**, `MultiAgentOrchestrator` boots every entity, provisions a Matrix account per agent, keeps all sync loops alive in one process | Three paths: `@agent_name` mention, thread continuation (same agent keeps the thread), or an LLM **router** picking the best agent for a new message | Not documented | Not documented; all conversation lives in threads |
| [AgentTeams / HiClaw](https://github.com/agentscope-ai/AgentTeams) (v1.2.2, 2026-08-08) | **Matrix AppService** + per-worker containers | Explicit mentions + room context | Central Manager orchestration (no peer-to-peer agent cycles) + human-in-the-loop | Shared room = shared context |
| [matrix-chatgpt-bot](https://github.com/matrixgpt/matrix-chatgpt-bot) | Node | - | - | **Archived 2024-09-16, unmaintained, README points to baibot** |
| [OpenClaw Matrix channel](https://docs.openclaw.ai/channels/matrix) | matrix-js-sdk | `allowBots: true \| "mentions" \| off`; per-room overrides; thread session routing `per-user`/`per-room`; reply threading `off`/`inbound`/`always` | [Bot loop protection](https://docs.openclaw.ai/channels/bot-loop-protection): sliding window **20 events / 60s per (account, room, sender-bot, receiver-bot) pair, then a 60s cooldown** | Thread-bound sessions |
| [MSC4295 Bot bounce limit](https://github.com/matrix-org/matrix-spec-proposals/pull/4295) | spec proposal, **open** as of 2026-05 | - | `m.bounce_limit` integer, TTL-style: decrement on forward, 0 = stop, absent = unlimited. Explicit use case is "notification bots replying to other bots" | - |

**Not found:** no Element or matrix.org first-party "AI agents on Matrix" announcement or MSC in 2024-2026 searching; no official Letta or mem0 Matrix connector.

**Convergent design from the prior art** (all four working systems agree): one Matrix account per persona; reply only on explicit mention or thread continuation, never on every message; put every conversation in a thread so context is naturally scoped; feed the LLM the thread via `/relations` rather than raw room history; and add an explicit bot-to-bot budget (OpenClaw's 20/60s + cooldown is the only battle-tested number found) because `sender != self` is not sufficient loop prevention in a 14-bot room.