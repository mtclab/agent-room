"""Live gate harness: accounts, a fresh room, and a real connector binary.

The gates in `tests/live/` drive the SHIPPED binary named by `AGENT_ROOM_BIN`
(default: the release build at `target/release/agent-room`). They are the only
thing in this repo that talks to a real homeserver, and WHICH homeserver is not
in the source: it comes from `~/.config/agent-room/live.env`, outside the tree.
See `tests/live/README.md` and `tests/live/live.env.example`.

Everything here runs only when AGENT_ROOM_LIVE=1. Every room is created by the
`room` fixture and forgotten at teardown; no pre-existing room is ever touched.
The offline gates are Rust's - `cargo test`.

CLIENT REALISM. The human here posts what a person's client posts. Element
sends typed text as `m.text` with a `body` and NOTHING else: `m.mentions` is
written only when the sender picks a name out of the completion list, and
`m.relates_to` only when they use the reply or thread affordance. So `post()`
sends a body alone, and `post_typed_name()` is the way to address an agent the
way people actually do - by typing its name.

The machine-level signals are the EXCEPTION, and each one is tested on purpose:
`mentions=` for a pill, `thread_root=` for a threaded reply. Passing one is a
deliberate statement that this gate is about that signal. A gate that only ever
posted with `m.mentions` attached would be proving the pill works and nothing
else - which is exactly how "Qwen, why are you so quiet?" reached a real room
unanswered while every gate was green (docs/GATES.md, "Addressing by name").
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import signal
import subprocess
import time
from collections.abc import AsyncIterator, Callable, Iterable, Iterator
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import pytest
import yaml
from nio import AsyncClient, MessageDirection, RoomPreset

# The harness lives in a conftest rather than in `tests/live/harness.py` because
# pytest resolves fixtures through conftest and nothing else.

#: Every live module carries this: the gates need the LAN and the bot tokens.
LIVE_SKIP = pytest.mark.skipif(
    os.environ.get("AGENT_ROOM_LIVE") != "1",
    reason="live gates need AGENT_ROOM_LIVE=1, a homeserver and the bot tokens",
)


LIVE_ENV_DEFAULT = Path.home() / ".config" / "agent-room" / "live.env"


def _load_dotenv() -> None:
    """Read the live-gate deployment values into the environment.

    They live OUTSIDE the repository, at `~/.config/agent-room/live.env`
    (override the path with `AGENT_ROOM_LIVE_ENV`): which homeserver the gates
    run against, where the tokens are and which accounts they borrow are
    deployment details, never source - so nothing in the tree can leak them.
    Anything already exported wins.
    """
    path = Path(os.environ.get("AGENT_ROOM_LIVE_ENV") or LIVE_ENV_DEFAULT).expanduser()
    if not path.exists():
        return
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, _, value = line.partition("=")
        os.environ.setdefault(name.strip(), value.strip().strip("\"'"))


_load_dotenv()


def _live_name(var: str) -> str:
    """An account localpart from the live env; empty when not configured, so the
    LIVE_SKIP marker (not an import error) is what a machine without the env sees."""
    return os.environ.get(var, "")


#: The homeserver the gates run against, and its server name. No default: a
#: gate that quietly points itself at example.com would fail for the wrong
#: reason, half an hour in.
LIVE_HOMESERVER = os.environ.get("AGENT_ROOM_LIVE_HOMESERVER", "")
SERVER_NAME = os.environ.get("AGENT_ROOM_LIVE_SERVER_NAME", "")
HUMAN_NAME = _live_name("AGENT_ROOM_LIVE_HUMAN")
BOT_A_NAME = _live_name("AGENT_ROOM_LIVE_BOT_A")
BOT_B_NAME = _live_name("AGENT_ROOM_LIVE_BOT_B")
#: The S3 gates drive a SECOND pair of accounts. Bot A is not free: a production
#: connector runs on it, and Synapse rate-limits per user, so the bot-to-bot
#: bursts in G5/G6 (and worse, their teeth runs) would land on the same account
#: that is meant to be talking to real people.
S3_BOT_A_NAME = _live_name("AGENT_ROOM_LIVE_S3_BOT_A")
S3_BOT_B_NAME = _live_name("AGENT_ROOM_LIVE_S3_BOT_B")
LIVE_HUMAN = f"@{HUMAN_NAME}:{SERVER_NAME}"
BOT_A = f"@{BOT_A_NAME}:{SERVER_NAME}"
BOT_B = f"@{BOT_B_NAME}:{SERVER_NAME}"
S3_BOT_A = f"@{S3_BOT_A_NAME}:{SERVER_NAME}"
S3_BOT_B = f"@{S3_BOT_B_NAME}:{SERVER_NAME}"
BOT_NAMES = (BOT_A_NAME, BOT_B_NAME, S3_BOT_A_NAME, S3_BOT_B_NAME)
BOT_IDS = (BOT_A, BOT_B, S3_BOT_A, S3_BOT_B)

REPO_ROOT = Path(__file__).resolve().parents[1]


def _agent_room_bin() -> Path:
    """The binary the gates drive.

    `AGENT_ROOM_BIN` wins - the teeth runner points it at a mutant build.
    Without it, the release build. There is no fallback to anything else: the
    binary IS the product, and a gate that quietly drove something else would
    be proving nothing about what ships.
    """
    override = os.environ.get("AGENT_ROOM_BIN")
    if override:
        return Path(override).expanduser().resolve()
    return REPO_ROOT / "target" / "release" / "agent-room"


AGENT_ROOM_BIN = _agent_room_bin()

PAIR_PER_MINUTE = 3


@pytest.fixture(scope="session", autouse=True)
def live_prerequisites() -> None:
    """Fail loudly, at the start, when a live run cannot possibly work.

    Skipping would be worse than failing here: a suite that silently skips
    everything reads as green, and "the gates passed" would then mean nothing.
    """
    if os.environ.get("AGENT_ROOM_LIVE") != "1":
        return
    missing = [
        name
        for name, value in (
            ("AGENT_ROOM_LIVE_HOMESERVER", LIVE_HOMESERVER),
            ("AGENT_ROOM_LIVE_SERVER_NAME", SERVER_NAME),
            ("AGENT_ROOM_LIVE_HUMAN", HUMAN_NAME),
            ("AGENT_ROOM_LIVE_BOT_A", BOT_A_NAME),
            ("AGENT_ROOM_LIVE_BOT_B", BOT_B_NAME),
            ("AGENT_ROOM_LIVE_S3_BOT_A", S3_BOT_A_NAME),
            ("AGENT_ROOM_LIVE_S3_BOT_B", S3_BOT_B_NAME),
        )
        if not value
    ]
    if missing:
        raise RuntimeError(
            f"{missing} not set - write them to {LIVE_ENV_DEFAULT} (see "
            "tests/live/live.env.example and tests/live/README.md)."
        )
    if not AGENT_ROOM_BIN.exists():
        raise RuntimeError(
            f"{AGENT_ROOM_BIN} does not exist - `make build`, or set AGENT_ROOM_BIN."
        )


class Tokens:
    """Access tokens that never render themselves.

    pytest prints fixture values in failure reports, so a plain dict here would
    put live Matrix tokens into logs and scrollback.
    """

    def __init__(self, values: dict[str, str]) -> None:
        self._values = values

    def __getitem__(self, name: str) -> str:
        return self._values[name]

    def __repr__(self) -> str:
        return f"<Tokens for {sorted(self._values)} - redacted>"


@pytest.fixture(scope="session")
def tokens() -> Tokens:
    raw_path = os.environ.get("AGENT_ROOM_TOKENS_FILE", "")
    if not raw_path:
        pytest.skip("AGENT_ROOM_TOKENS_FILE is not set; see tests/live/README.md")
    path = Path(raw_path).expanduser()
    data = json.loads(path.read_text(encoding="utf-8"))
    wanted = (HUMAN_NAME, *BOT_NAMES)
    missing = [n for n in wanted if n not in data]
    if missing:
        pytest.skip(f"{path} has no token for {missing}")
    return Tokens({name: str(data[name]) for name in wanted})


def client_for(token: str) -> AsyncClient:
    client = AsyncClient(LIVE_HOMESERVER)
    client.access_token = token
    return client


@pytest.fixture
async def human(tokens: Tokens) -> AsyncIterator[AsyncClient]:
    client = client_for(tokens[HUMAN_NAME])
    whoami = await client.whoami()
    assert getattr(whoami, "user_id", None) == LIVE_HUMAN, whoami
    client.user_id = LIVE_HUMAN
    try:
        yield client
    finally:
        await client.close()


@asynccontextmanager
async def fresh_room(
    human: AsyncClient, tokens: Tokens, names: tuple[str, ...]
) -> AsyncIterator[str]:
    """One private room for one test, forgotten by everyone at teardown.

    Only the bots a test actually needs are invited: an invitation is visible to
    the account that gets it, and these accounts have work of their own.
    """
    stamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%S%f")
    response = await human.room_create(
        name=f"agent-room-test-{stamp}",
        topic="agent-room live gate; created and forgotten by the test suite",
        preset=RoomPreset.private_chat,
        invite=[f"@{name}:{SERVER_NAME}" for name in names],
        is_direct=False,
    )
    room_id = getattr(response, "room_id", None)
    assert room_id, f"room_create failed: {response}"
    try:
        yield room_id
    finally:
        for name in names:
            bot = client_for(tokens[name])
            await bot.room_leave(room_id)
            await bot.room_forget(room_id)
            await bot.close()
        await human.room_leave(room_id)
        await human.room_forget(room_id)


@pytest.fixture
async def room(human: AsyncClient, tokens: Tokens) -> AsyncIterator[str]:
    """A fresh private room with the S1/S2 pair (bot A, bot B)."""
    async with fresh_room(human, tokens, (BOT_A_NAME, BOT_B_NAME)) as room_id:
        yield room_id


@pytest.fixture
async def room_s3(human: AsyncClient, tokens: Tokens) -> AsyncIterator[str]:
    """A fresh private room with the S3 pair (bot C, bot D)."""
    async with fresh_room(human, tokens, (S3_BOT_A_NAME, S3_BOT_B_NAME)) as room_id:
        yield room_id


@dataclass
class Connector:
    """One real `agent-room run` process."""

    name: str
    user_id: str
    config_path: Path
    state_dir: Path
    log_path: Path
    process: subprocess.Popen[bytes] | None = None

    def start(self, env_extra: dict[str, str] | None = None) -> None:
        env = dict(os.environ)
        env.update(env_extra or {})
        handle = self.log_path.open("ab")
        handle.write(f"\n=== start {datetime.now(UTC).isoformat()} ===\n".encode())
        self.process = subprocess.Popen(
            [str(AGENT_ROOM_BIN), "run", "--config", str(self.config_path)],
            stdout=handle,
            stderr=subprocess.STDOUT,
            env=env,
        )

    def log_text(self) -> str:
        if not self.log_path.exists():
            return ""
        return self.log_path.read_text(encoding="utf-8", errors="replace")

    def wait_ready(self, timeout: float = 60.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if "watching" in self.log_text():
                return
            if self.process is not None and self.process.poll() is not None:
                raise AssertionError(f"{self.name} exited early:\n{self.log_text()}")
            time.sleep(0.5)
        raise AssertionError(f"{self.name} never became ready:\n{self.log_text()}")

    def terminate(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        self.process.send_signal(signal.SIGTERM)
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:  # pragma: no cover - shutdown bug
            self.process.kill()
            self.process.wait(timeout=10)

    def kill(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        self.process.kill()
        self.process.wait(timeout=10)

    def dump(self) -> None:
        print(f"\n----- {self.name} log -----\n{self.log_text()}")


def make_connector(
    tmp_path: Path,
    tokens: Tokens,
    name: str,
    room_id: str,
    policy: dict[str, Any] | None = None,
    brain: dict[str, Any] | None = None,
    persona: str | None = None,
    history_limit: int | None = None,
    transcript_keep: int | None = None,
    transcript_archives: int | None = None,
) -> Connector:
    """Write a connector config (token file 0600, state in tmp) for `name`."""
    home = tmp_path / name
    home.mkdir(parents=True, exist_ok=True)
    token_path = home / "access"
    token_path.write_text(tokens[name], encoding="utf-8")
    token_path.chmod(0o600)
    state_dir = home / "state"
    config: dict[str, Any] = {
        "homeserver": LIVE_HOMESERVER,
        "user_id": f"@{name}:{SERVER_NAME}",
        "access_token_file": str(token_path),
        "rooms": [room_id],
        "state_dir": str(state_dir),
        # The gate accounts' devices are long wedged (fresh store per run for
        # months); the rooms here are unencrypted, so the gates run anyway.
        "allow_wedged_device": True,
        "brain": brain if brain is not None else {"kind": "echo"},
        "policy": {
            "reply_to_mentions": True,
            "reply_in_own_threads": True,
            "answer_unaddressed": True,
            "bot_to_bot": "mentions",
            # The gate accounts share a localpart prefix, so the account
            # playing the human can match a bot-localpart pattern. Name the bots
            # explicitly so the human is treated as one - otherwise the
            # bot_to_bot guard decides everything and the other guards are never
            # reached. (Found 2026-09-02 when the G2 teeth run passed.)
            "bot_user_ids": list(BOT_IDS),
            "bot_localpart_patterns": [],
            "budgets": {
                "per_pair_per_minute": PAIR_PER_MINUTE,
                "pair_cooldown_s": 60,
                "per_thread_max": 12,
                "per_hour_max": 30,
            },
            **(policy or {}),
        },
        "tls": {"enabled": False},
    }
    if history_limit is not None:
        config["history_limit"] = history_limit
    # Left out unless a gate asks: the shipped cap is 5000 events, and a live
    # gate that set it would be measuring its own number rather than the
    # default. T1 sets both, because rolling 5000 events takes an afternoon.
    if transcript_keep is not None:
        config["transcript_keep"] = transcript_keep
    if transcript_archives is not None:
        config["transcript_archives"] = transcript_archives
    if persona is not None:
        persona_path = home / "persona.md"
        persona_path.write_text(persona, encoding="utf-8")
        config["persona_file"] = str(persona_path)
    config_path = home / "config.yaml"
    config_path.write_text(yaml.safe_dump(config, sort_keys=False), encoding="utf-8")
    return Connector(
        name=name,
        user_id=str(config["user_id"]),
        config_path=config_path,
        state_dir=state_dir,
        log_path=home / "connector.log",
    )


@pytest.fixture
def running() -> Iterator[list[Connector]]:
    """Everything started by a test, always stopped and always logged."""
    started: list[Connector] = []
    try:
        yield started
    finally:
        for connector in started:
            connector.terminate()
            connector.dump()


#: How many times a post that Synapse rate-limited is retried before the gate
#: gives up on it. The server says how long to wait; this only bounds the wait.
RATE_LIMIT_RETRIES = 6


async def post(
    human: AsyncClient,
    room_id: str,
    body: str,
    mentions: list[str] | None = None,
    thread_root: str | None = None,
) -> str:
    """Post as the human, riding out the homeserver's own rate limit.

    The default is what a person's client sends for typed text: `m.text` with a
    body, and no `m.mentions` and no `m.relates_to` at all. `mentions` and
    `thread_root` are the machine-level signals a client writes only when the
    sender picks a pill or uses the reply affordance - see CLIENT REALISM at the
    top of this file - and a gate that passes one is saying it is about that
    signal.

    Synapse limits messages per user, and a gate that posts a burst (T1 posts
    25) meets `M_LIMIT_EXCEEDED` before the product has done anything at all.
    Only that one refusal is retried, and only for as long as the server itself
    asks: anything else is still an immediate failure, because a gate that
    retried its way past a real error would be proving nothing.
    """
    content: dict[str, Any] = {"msgtype": "m.text", "body": body}
    if mentions is not None:
        content["m.mentions"] = {"user_ids": mentions}
    if thread_root is not None:
        content["m.relates_to"] = {
            "rel_type": "m.thread",
            "event_id": thread_root,
            "is_falling_back": True,
            "m.in_reply_to": {"event_id": thread_root},
        }
    for _attempt in range(RATE_LIMIT_RETRIES):
        response = await human.room_send(room_id, "m.room.message", content)
        event_id = getattr(response, "event_id", None)
        if event_id:
            return str(event_id)
        if getattr(response, "status_code", "") != "M_LIMIT_EXCEEDED":
            break
        wait_ms = getattr(response, "retry_after_ms", None) or 1000
        await asyncio.sleep(min(wait_ms / 1000, 10.0))
    raise AssertionError(f"the human could not post: {response}")


def localpart(user_id: str) -> str:
    """`@bot-a:example.com` -> `bot-a`, the way the connector reads a name."""
    return user_id.split(":", 1)[0].lstrip("@")


async def addressable_names(human: AsyncClient, room_id: str) -> set[str]:
    """Every name a connector in this room would take for an address.

    The same set the product builds (`src/addressing.rs`): each member's
    localpart, their display name and its first word, and nothing shorter than
    three characters. Lowercased, because the match is case-insensitive.
    """
    response = await human.joined_members(room_id)
    names: set[str] = set()
    for member in getattr(response, "members", []):
        names.add(localpart(member.user_id))
        display = (getattr(member, "display_name", "") or "").strip()
        if display:
            names.add(display)
            names.add(display.split()[0])
    return {name.lower() for name in names if len(name) >= 3}


def named_in(body: str, names: Iterable[str]) -> list[str]:
    """Which of `names` this body NAMES, by the product's own word rule.

    A hyphen is a word character (`src/addressing.rs`), so a room member called
    "gate" is not named by a line about `gate-bot-a`. A plain substring test
    would say it was, and these accounts share a localpart prefix.
    """
    low = body.lower()
    return sorted(
        name for name in names if re.search(rf"(?<![\w-]){re.escape(name)}(?![\w-])", low)
    )


async def post_typed_name(human: AsyncClient, room_id: str, bot_localpart: str, text: str) -> str:
    """Address an agent the way a person does: by TYPING its name.

    A vocative in the body and nothing else - no `m.mentions`, no reply, no
    thread. This is what Element sends when somebody types "qwen, why so
    quiet?", and the agent has only the body to read it out of.

    The gate proves the line does what it says before posting it: the name has
    to be one this room recognises (the account names come from the environment
    and the display names from the homeserver, so nothing in the tree can know
    them), and nobody ELSE'S name may be in it - a second name would hand the
    turn to somebody else and the silence would mean something quite different.
    """
    body = f"{bot_localpart}, {text}"
    names = await addressable_names(human, room_id)
    assert bot_localpart.lower() in names, (
        f"{bot_localpart!r} is not a name this room would recognise ({sorted(names)}), so this "
        "line addresses nobody: the gate would be measuring tier 2."
    )
    others = [name for name in named_in(body, names) if name != bot_localpart.lower()]
    assert not others, (
        f"{body!r} also names {others}, so it is not addressed to {bot_localpart} alone."
    )
    return await post(human, room_id, body)


async def post_unaddressed(human: AsyncClient, room_id: str, body: str, **kwargs: Any) -> str:
    """Post a line that must address NOBODY, having proved that it does not.

    A tier-2 gate says "nobody was addressed" in its body and nowhere else, and
    since the connector reads names out of that body, a body that happens to
    contain somebody's name is a tier-1 gate wearing tier 2's name. The account
    names come from the environment and the display names from the homeserver,
    so neither this file nor the gate can know them: this asks the room.
    """
    named = sorted(name for name in await addressable_names(human, room_id) if name in body.lower())
    assert not named, (
        f"{body!r} names {named}, so it is addressed to somebody and cannot gate "
        "the unaddressed path. Rename the accounts or the body."
    )
    return await post(human, room_id, body, **kwargs)


async def messages(human: AsyncClient, room_id: str) -> list[dict[str, Any]]:
    """Every `m.room.message` in the room, newest first, via /messages.

    Deliberately no `from` token. Paging back from a sync token means paging
    back from whatever Synapse's sync cache last handed us, which can predate
    the traffic under test - that is how a G3 run saw zero of the 12 messages
    the log proves were posted (2026-09-02). With no `from`, /messages always
    starts at the room as it is now.
    """
    response = await human.room_messages(
        room_id, start=None, direction=MessageDirection.back, limit=200
    )
    chunk = getattr(response, "chunk", None)
    assert chunk is not None, f"/messages failed: {response}"
    out = []
    for event in chunk:
        source = getattr(event, "source", None)
        if isinstance(source, dict) and source.get("type") == "m.room.message":
            out.append(source)
    return out


def by_sender(events: list[dict[str, Any]], sender: str) -> list[dict[str, Any]]:
    return [e for e in events if e.get("sender") == sender]


def in_thread(events: list[dict[str, Any]], root: str) -> list[dict[str, Any]]:
    """Every message belonging to thread `root`, root included."""
    return [
        event
        for event in events
        if event.get("event_id") == root or relates_to(event).get("event_id") == root
    ]


async def wait_for(
    predicate: Callable[[list[dict[str, Any]]], bool],
    human: AsyncClient,
    room_id: str,
    seconds: float,
    interval: float = 2.0,
) -> list[dict[str, Any]]:
    """Poll /messages until `predicate` holds, or the deadline passes."""
    deadline = time.monotonic() + seconds
    events: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        events = await messages(human, room_id)
        if predicate(events):
            return events
        await asyncio.sleep(interval)
    return events


async def wait_for_join(human: AsyncClient, room_id: str, user_ids: list[str]) -> None:
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        response = await human.joined_members(room_id)
        members = {m.user_id for m in getattr(response, "members", [])}
        if all(user_id in members for user_id in user_ids):
            return
        await asyncio.sleep(1)
    raise AssertionError(f"{user_ids} never joined {room_id}")


def relates_to(event: dict[str, Any]) -> dict[str, Any]:
    relation = event.get("content", {}).get("m.relates_to")
    return relation if isinstance(relation, dict) else {}


# ---------------------------------------------------------------------------
# Live harness: the MCP server (S4)
# ---------------------------------------------------------------------------
#
# A live session is a REAL `agent-room mcp` subprocess driven over stdio by the
# MCP SDK's own client, with its own Matrix account. Bot C plays the session;
# bot B is the daemon connector it talks to. The production bots are never
# used here.

#: The account an MCP session runs as in the gates.
MCP_SESSION_NAME = S3_BOT_A_NAME
MCP_SESSION = S3_BOT_A
#: The account running a daemon connector beside it.
MCP_PEER_NAME = BOT_B_NAME
MCP_PEER = BOT_B


@pytest.fixture
async def room_session(human: AsyncClient, tokens: Tokens) -> AsyncIterator[str]:
    """A fresh private room with just the MCP session's account."""
    async with fresh_room(human, tokens, (MCP_SESSION_NAME,)) as room_id:
        yield room_id


@pytest.fixture
async def room_session_pair(human: AsyncClient, tokens: Tokens) -> AsyncIterator[str]:
    """A fresh private room with the MCP session and a daemon connector."""
    async with fresh_room(human, tokens, (MCP_SESSION_NAME, MCP_PEER_NAME)) as room_id:
        yield room_id


@dataclass
class Session:
    """Where one `agent-room mcp` process is configured and what it logged."""

    name: str
    user_id: str
    config_path: Path
    state_dir: Path
    log_path: Path

    def log_text(self) -> str:
        if not self.log_path.exists():
            return ""
        return self.log_path.read_text(encoding="utf-8", errors="replace")

    def dump(self) -> None:
        print(f"\n----- {self.name} mcp server log -----\n{self.log_text()}")


def make_session(
    tmp_path: Path,
    tokens: Tokens,
    name: str,
    room_id: str,
    policy: dict[str, Any] | None = None,
    mcp: dict[str, Any] | None = None,
) -> Session:
    """Write a live-session config: the connector's format, minus the brain."""
    home = tmp_path / f"{name}-session"
    home.mkdir(parents=True, exist_ok=True)
    token_path = home / "access"
    token_path.write_text(tokens[name], encoding="utf-8")
    token_path.chmod(0o600)
    state_dir = home / "state"
    config: dict[str, Any] = {
        "homeserver": LIVE_HOMESERVER,
        "user_id": f"@{name}:{SERVER_NAME}",
        "access_token_file": str(token_path),
        "rooms": [room_id],
        "state_dir": str(state_dir),
        # The gate accounts' devices are long wedged (fresh store per run for
        # months); the rooms here are unencrypted, so the gates run anyway.
        "allow_wedged_device": True,
        "policy": {
            "bot_user_ids": list(BOT_IDS),
            "bot_localpart_patterns": [],
            **(policy or {}),
        },
        "mcp": mcp or {},
        "tls": {"enabled": False},
    }
    config_path = home / "config.yaml"
    config_path.write_text(yaml.safe_dump(config, sort_keys=False), encoding="utf-8")
    return Session(
        name=name,
        user_id=str(config["user_id"]),
        config_path=config_path,
        state_dir=state_dir,
        log_path=home / "mcp.log",
    )


@asynccontextmanager
async def mcp_client(session: Session) -> AsyncIterator[Any]:
    """Drive a REAL `agent-room mcp` subprocess over stdio, as a client would.

    Nothing here is a shortcut into the module: the server is the installed
    console script, launched the same way `claude mcp add` launches it, and
    everything the test asserts came back over the wire.
    """
    from mcp import Client, StdioServerParameters
    from mcp.client.stdio import stdio_client

    params = StdioServerParameters(
        command=str(AGENT_ROOM_BIN), args=["mcp", "--config", str(session.config_path)]
    )
    with session.log_path.open("a", encoding="utf-8") as errlog:
        errlog.write(f"\n=== start {datetime.now(UTC).isoformat()} ===\n")
        errlog.flush()
        async with Client(stdio_client(params, errlog=errlog)) as client:
            yield client


def tool_json(result: Any) -> Any:
    """What a tool returned, or an assertion naming the error it returned."""
    assert not result.is_error, tool_error(result)
    structured = result.structured_content
    assert structured is not None, f"no structured content in {result}"
    return structured.get("result", structured)


def tool_error(result: Any) -> str:
    return "\n".join(getattr(block, "text", "") for block in result.content)
