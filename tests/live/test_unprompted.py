"""Live gates for S6: speaking because something happened, not because asked.

Same shape as the tier-2 gates - a fresh private room, the human account
playing the human, real `agent-room run` processes, the echo brain so no model
is in the loop - with two new levers on the outside of the connector: the
human's Matrix PRESENCE (`PUT /_matrix/client/v3/presence/{user}/status`,
driven through nio's `set_presence`) and the real `agent-room impulse`
command.

    AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q -m live tests/live/test_unprompted.py
"""

from __future__ import annotations

import asyncio
import subprocess
import time
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest
from aiohttp import web
from conftest import (
    AGENT_ROOM_BIN,
    LIVE_SKIP,
    S3_BOT_A,
    S3_BOT_A_NAME,
    Connector,
    Tokens,
    by_sender,
    make_connector,
    messages,
    post,
    relates_to,
    wait_for,
    wait_for_join,
)
from nio import AsyncClient

pytestmark = [pytest.mark.live, LIVE_SKIP]

# The connector polls its inlet and its loops every 5 s (UNPROMPTED_POLL_S), so
# every deadline below is that, plus the gate's own back-off, plus a margin.


def unprompted_policy(**extra: Any) -> dict[str, Any]:
    """Tier 2 OFF, so the only thing that can post is the path under test."""
    policy: dict[str, Any] = {
        "answer_unaddressed": False,
        "backoff_s": [1.0, 3.0],
        "presence_window_min": 1,
    }
    policy.update(extra)
    return policy


def drop_impulse(connector: Connector, room_id: str, text: str, kind: str = "note") -> None:
    """The REAL `agent-room impulse` command, the way a hook would run it."""
    result = subprocess.run(
        [
            str(AGENT_ROOM_BIN),
            "impulse",
            "--config",
            str(connector.config_path),
            "--room",
            room_id,
            "--kind",
            kind,
            text,
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, f"agent-room impulse failed: {result.stderr}"
    print(f"\n----- impulse -----\n{result.stdout}")


async def set_presence(human: AsyncClient, state: str) -> None:
    response = await human.set_presence(state)
    assert getattr(response, "transport_response", None) is not None, response
    # Presence reaches the other members through their /sync; give the
    # connector's long poll a moment to be woken by it.
    await asyncio.sleep(3)


def bot_posts(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return by_sender(events, S3_BOT_A)


# -- G9 ----------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_g9_an_impulse_waits_until_somebody_is_there_to_hear_it(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G9: something happened to the agent, and whether it says so depends on
    whether anybody is in the room.

    Three phases, one connector:

    1. the human is `online` and has never posted - so presence, and only
       presence, is what makes the agent speak. One unthreaded notice.
    2. the human goes `offline` and (with `presence_window_min: 1`) has not
       posted inside the window: a second impulse stays unspoken.
    3. the human posts. That is the other half of "somebody is here", and the
       impulse that was waiting is spoken.
    """
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3, policy=unprompted_policy())
    # Online BEFORE the connector starts, so the state is in its first sync as
    # well as in the delta that follows.
    await set_presence(human, "online")
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])
    await set_presence(human, "online")

    # -- 1. a human is online, and nobody has said a word ---------------------
    drop_impulse(bot, room_s3, "[[speak]] the overnight render finished", kind="render")
    events = await wait_for(lambda evs: bool(bot_posts(evs)), human, room_s3, seconds=40)
    spoken = bot_posts(events)
    assert len(spoken) == 1, f"expected one impulse, got {len(spoken)}"
    content = spoken[0]["content"]
    assert content["msgtype"] == "m.notice"
    assert "the overnight render finished" in content["body"]
    assert "m.relates_to" not in content, "an impulse is not a reply to anything"
    assert "m.mentions" not in content, "an impulse pings nobody"
    assert "is online" in bot.log_text(), "the log must say why it thought anybody was there"

    # -- 2. nobody is here ----------------------------------------------------
    await set_presence(human, "offline")
    drop_impulse(bot, room_s3, "[[speak]] and the second render finished too", kind="render")
    await asyncio.sleep(30)
    assert len(bot_posts(await messages(human, room_s3))) == 1, (
        "the agent announced something into an empty room"
    )
    assert "impulse queued, waiting for somebody to be here" in bot.log_text(), (
        "the impulse was never queued - was it dropped instead of waiting?"
    )

    # -- 3. a human turns up --------------------------------------------------
    await post(human, room_s3, "back at my desk")
    events = await wait_for(lambda evs: len(bot_posts(evs)) > 1, human, room_s3, seconds=60)
    spoken = bot_posts(events)
    assert len(spoken) == 2, f"the waiting impulse was never spoken ({len(spoken)} posts)"
    assert "the second render finished too" in spoken[0]["content"]["body"]


# -- G10 ---------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_g10_a_question_nobody_answers_gets_exactly_one_follow_up(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G10: the agent asks something, nobody answers, and later it comes back to
    it - once.

    The echo brain's `ask_back` is what leaves the question hanging, so the loop
    is opened by the shipped rule ("my message ended with a question mark") and
    not by anything the test reaches into. The delay range is [5, 8] s here
    instead of the shipped 20 min - 3 h.
    """
    bot = make_connector(
        tmp_path,
        tokens,
        S3_BOT_A_NAME,
        room_s3,
        policy=unprompted_policy(followup_delay_s=[5.0, 8.0]),
        brain={"kind": "echo", "echo": {"ask_back": "did anyone try it?"}},
    )
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    # -- the loop nobody closes ----------------------------------------------
    # `[[speak]]` travels from the human's line into the echo reply, and from
    # there into the loop's text - which is what the follow-up's judge reads.
    root = await post(
        human, room_s3, f"{S3_BOT_A} [[speak]] what is the state of the build?", [S3_BOT_A]
    )
    events = await wait_for(lambda evs: bool(bot_posts(evs)), human, room_s3, seconds=40)
    assert len(bot_posts(events)) == 1, "the agent never answered at all"

    events = await wait_for(lambda evs: len(bot_posts(evs)) > 1, human, room_s3, seconds=60)
    posts = bot_posts(events)
    assert len(posts) == 2, f"expected the answer and one follow-up, got {len(posts)}"
    follow_up = posts[0]  # /messages comes back newest first
    assert relates_to(follow_up).get("event_id") == root, "a follow-up stays in its thread"
    assert follow_up["content"]["msgtype"] == "m.notice"
    assert "m.mentions" not in follow_up["content"], "a follow-up is not a ping"
    assert "left a loop open" in bot.log_text()

    # And never again, even though the follow-up itself ends in a question mark.
    await asyncio.sleep(25)
    assert len(bot_posts(await messages(human, room_s3))) == 2, "the agent followed up twice"

    # -- the loop somebody closes --------------------------------------------
    before = len(bot_posts(await messages(human, room_s3)))
    second = await post(human, room_s3, f"{S3_BOT_A} [[speak]] and the deploy?", [S3_BOT_A])
    events = await wait_for(lambda evs: len(bot_posts(evs)) > before, human, room_s3, seconds=40)
    assert len(bot_posts(events)) == before + 1, "the agent never answered the second question"

    # `[[silent]]` so the answer itself produces no reply and therefore no new
    # loop: what is under test is that ANSWERING closes the loop.
    await post(human, room_s3, "[[silent]] yes, I tried it, it works", thread_root=second)
    await asyncio.sleep(25)
    assert len(bot_posts(await messages(human, room_s3))) == before + 1, (
        "the agent followed up on a question that had been answered"
    )
    assert "open loop closed by" in bot.log_text()


# -- G11 ---------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_g11_inner_thoughts_add_up_until_the_agent_speaks_once(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G11: the judge says no every time and keeps saying it wants to speak. At
    2 + 2 the wanting stops being a no, and the agent says one thing.

    Tier 2 is ON here (it is where the urgency is collected), and the echo judge
    refuses every message - no `[[speak]]` anywhere - so the ONLY thing that can
    produce a post is the accumulator.
    """
    bot = make_connector(
        tmp_path,
        tokens,
        S3_BOT_A_NAME,
        room_s3,
        policy=unprompted_policy(
            answer_unaddressed=True, inner_thoughts=True, inner_thoughts_threshold=4
        ),
        brain={"kind": "echo", "echo": {"urgency": 2}},
    )
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    await post(human, room_s3, "thinking aloud about the deploy")
    await asyncio.sleep(12)  # its back-off, its judge, and its silence
    assert bot_posts(await messages(human, room_s3)) == [], "the first message should be a no"

    await post(human, room_s3, "and about the migration as well")
    events = await wait_for(lambda evs: bool(bot_posts(evs)), human, room_s3, seconds=60)
    posts = bot_posts(events)
    assert len(posts) == 1, f"expected exactly one inner thought, got {len(posts)}"
    assert posts[0]["content"]["msgtype"] == "m.notice"
    assert "m.mentions" not in posts[0]["content"], "an inner thought pings nobody"
    log = bot.log_text()
    assert "inner thoughts at 2/4" in log, "the accumulator never showed its working"
    assert "reached 4/4" in log

    # Speaking empties it, so the next message starts from nothing.
    await post(human, room_s3, "and one more thing")
    await asyncio.sleep(20)
    assert len(bot_posts(await messages(human, room_s3))) == 1, (
        "the agent spoke twice off one accumulation"
    )


# -- G12 ---------------------------------------------------------------------


@dataclass
class WarmCounter:
    """A model endpoint that counts what it is asked for."""

    base_url: str = ""
    requests: list[dict[str, Any]] = field(default_factory=list)

    @property
    def warm_ups(self) -> list[dict[str, Any]]:
        """The throwaway completions: one token, and nothing else is."""
        return [body for body in self.requests if body.get("max_tokens") == 1]


@pytest.fixture
async def warm_endpoint() -> AsyncIterator[WarmCounter]:
    """A real HTTP endpoint on this box, which the connector subprocess calls."""
    state = WarmCounter()

    async def handler(request: web.Request) -> web.Response:
        state.requests.append(await request.json())
        return web.json_response({"choices": [{"message": {"role": "assistant", "content": "ok"}}]})

    app = web.Application()
    app.router.add_post("/v1/chat/completions", handler)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    state.base_url = f"http://127.0.0.1:{runner.addresses[0][1]}/v1"
    try:
        yield state
    finally:
        await runner.cleanup()


def warm_brain(base_url: str, warm: bool) -> dict[str, Any]:
    return {
        "kind": "openai_compat",
        "openai_compat": {
            "base_url": base_url,
            "model": "test-model",
            "cold_start_timeout_s": 30,
            "warm_on_intent": warm,
            "warm_cooldown_s": 120,
        },
    }


async def warm_hits_after_typing(
    counter: WarmCounter, human: AsyncClient, room_id: str, seconds: float = 5.0
) -> int:
    await human.room_typing(room_id, True, timeout=20_000)
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if counter.warm_ups:
            break
        await asyncio.sleep(0.5)
    return len(counter.warm_ups)


@pytest.mark.timeout(300)
async def test_g12_a_typing_human_warms_an_on_demand_model_once(
    tmp_path: Path,
    tokens: Tokens,
    human: AsyncClient,
    room_s3: str,
    running: list[Connector],
    warm_endpoint: WarmCounter,
) -> None:
    """G12: a human starting to type is a reason to load the model, not a
    reason to say anything - and only once per cooldown, because typing a
    paragraph produces a notice every few seconds.

    The endpoint is a real HTTP server started by this test, so what is counted
    is what the shipped connector actually sent.
    """
    bot = make_connector(
        tmp_path,
        tokens,
        S3_BOT_A_NAME,
        room_s3,
        policy=unprompted_policy(),
        brain=warm_brain(warm_endpoint.base_url, warm=True),
    )
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])
    assert warm_endpoint.warm_ups == [], "something was warmed before anybody typed"

    assert await warm_hits_after_typing(warm_endpoint, human, room_s3) == 1

    # Still typing. Synapse only emits a typing event when the typing SET
    # changes, so a repeated `typing: true` is not a second notice - stopping
    # and starting again is, which is what a person pausing mid-sentence does.
    for _ in range(3):
        await human.room_typing(room_s3, False)
        await asyncio.sleep(0.5)
        await human.room_typing(room_s3, True, timeout=20_000)
        await asyncio.sleep(1.5)
    await asyncio.sleep(3)
    assert len(warm_endpoint.warm_ups) == 1, (
        f"the cooldown let {len(warm_endpoint.warm_ups)} warm-ups through"
    )
    assert "warm-up skipped" in bot.log_text(), (
        "the cooldown was never reached - did the later typing notices arrive at all?"
    )
    body = warm_endpoint.warm_ups[0]
    assert body["model"] == "test-model"
    assert body["messages"] == [{"role": "user", "content": "."}], (
        "a warm-up must not carry the room's conversation anywhere"
    )
    assert bot_posts(await messages(human, room_s3)) == [], "warming is not speaking"

    # -- and with the knob off, nothing is sent at all -------------------------
    bot.terminate()
    warm_endpoint.requests.clear()
    quiet = make_connector(
        tmp_path / "off",
        tokens,
        S3_BOT_A_NAME,
        room_s3,
        policy=unprompted_policy(),
        brain=warm_brain(warm_endpoint.base_url, warm=False),
    )
    quiet.start()
    running.append(quiet)
    quiet.wait_ready()

    assert await warm_hits_after_typing(warm_endpoint, human, room_s3) == 0
    assert warm_endpoint.requests == [], (
        "an always-on endpoint was sent a request that buys nothing"
    )
