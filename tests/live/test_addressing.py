"""Live gates for addressing by name (N1, N2, N4).

The gap these close, found the first time the room was used in anger: "Qwen,
why are you so quiet?" got nothing, "@Qwen hello" got an answer. A typed name
is plain text, so it used to fall through to tier 2 - a random back-off and a
model call - while a pill was tier 1.

Same shape as the other S3 gates: a fresh private room, the human account
playing the human, real `agent-room run` processes, and the echo brain, whose
judge says yes if and only if the trigger carries `[[speak]]`. The marker is
what gives these gates teeth: every line here would be ANSWERED by a connector
whose name guards were gone, so silence means the guard decided and not that
the judge happened to say no.

The names come from the environment, like every other account detail
(`tests/live/live.env.example`): nothing in this tree knows which accounts the
gates borrow.

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q tests/live/test_addressing.py
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest
from conftest import (
    LIVE_HUMAN,
    LIVE_SKIP,
    S3_BOT_A,
    S3_BOT_A_NAME,
    S3_BOT_B,
    S3_BOT_B_NAME,
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

#: Deliberately short. A judge call the agent was never supposed to make would
#: then have happened, and been logged, well inside the silence N2 and N4 wait
#: out - so "no judge line" is a measurement and not a race.
FAST_BACKOFF = [1.0, 3.0]
#: Long enough for that back-off, the judge and a post to have happened.
SETTLE_S = 20.0
#: The echo brain's judge says yes to anything carrying this.
SPEAK = "[[speak]]"
#: The reason `policy::unaddressed` gives a line nobody addressed. Its ABSENCE
#: is what says the name guards decided a line rather than tier 2.
TIER_2 = "tier 2 candidate"


def named_policy(**extra: Any) -> dict[str, Any]:
    policy: dict[str, Any] = {"answer_unaddressed": True, "backoff_s": FAST_BACKOFF}
    policy.update(extra)
    return policy


def judged(connector: Connector) -> bool:
    """Whether this connector ever asked its judge anything."""
    return "speak=" in connector.log_text()


# -- N1 ----------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_n1_a_typed_name_is_answered_at_once_and_costs_no_judge(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """N1: the human types the agent's name at the start of a line, with no
    pill, no reply and no thread. It answers as tier 1 - in the thread,
    mentioning the human - and never asks its judge.

    The line carries no `[[speak]]`, so an agent that fell through to tier 2
    would be told "no" and say nothing: the answer can only have come from the
    name guard.
    """
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3, policy=named_policy())
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    trigger = await post(human, room_s3, f"{S3_BOT_A_NAME}, why so quiet?")
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    replies = by_sender(events, S3_BOT_A)
    assert len(replies) == 1, f"expected one answer to a typed name, got {len(replies)}"

    content = replies[0]["content"]
    assert content["msgtype"] == "m.notice"
    assert relates_to(replies[0]).get("event_id") == trigger, "the answer is not in the thread"
    assert LIVE_HUMAN in content["m.mentions"]["user_ids"]

    log = bot.log_text()
    assert "named in the body" in log, "the name guard is not what decided"
    assert "verdict=reply" in log
    assert not judged(bot), "a typed name is tier 1: it must cost no judge call"


# -- N2 ----------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_n2_a_name_that_is_not_mine_is_somebody_elses_turn(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """N2: the human names the OTHER agent. This one stays out of it, and pays
    nothing to do so.

    The line carries `[[speak]]`, so tier 2 would answer it: silence here is
    the "somebody else was addressed" guard and cannot be the judge declining.
    The other agent is not even running - the name is known from
    `bot_user_ids`, which is what makes the gate deterministic before anybody
    has joined or spoken.
    """
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3, policy=named_policy())
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    await post(human, room_s3, f"{S3_BOT_B_NAME}, what do you think? {SPEAK}")
    await asyncio.sleep(SETTLE_S)

    events = await messages(human, room_s3)
    assert by_sender(events, S3_BOT_A) == [], "the agent answered a line addressed to somebody else"
    log = bot.log_text()
    # ", not me" with the comma, never the bare "not me": "did not mention me"
    # contains that, and a gate that matched it would report the bot_to_bot
    # guard as this one (found by the N4 teeth run, 2026-09-04).
    assert "addressed to" in log and ", not me" in log, "some other guard kept it quiet"
    assert TIER_2 not in log, "the line reached tier 2 instead of being decided at once"
    assert not judged(bot), "a line addressed to somebody else must cost nothing"

    # Silence proves nothing unless it was alive to break it.
    await post(human, room_s3, f"{S3_BOT_A_NAME}, and you?")
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    assert len(by_sender(events, S3_BOT_A)) == 1, "the agent was not alive to answer its own name"


# -- N4 ----------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_n4_two_agents_one_name_and_exactly_one_answer(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """N4: both agents in the room, one of them named. Exactly one answers, and
    the other says in its own log that the line was somebody else's.

    `[[speak]]` again, so the un-named agent's own judge would say yes if it
    ever got that far. What it must NOT do is get that far: the room is only
    half the gate here, because a tier-2 back-off followed by the stand-down
    re-read produces the same silence for a completely different reason. The
    log is the other half, and it is what tells "that line was not mine" apart
    from "somebody beat me to it".
    """
    connectors = {}
    for name in (S3_BOT_A_NAME, S3_BOT_B_NAME):
        connector = make_connector(tmp_path, tokens, name, room_s3, policy=named_policy())
        connector.start()
        running.append(connector)
        connector.wait_ready()
        connectors[name] = connector
    await wait_for_join(human, room_s3, [S3_BOT_A, S3_BOT_B])

    await post(human, room_s3, f"{S3_BOT_A_NAME}, how was your night? {SPEAK}")
    await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)

    # Long enough for the other agent's back-off, judge and post, had it wanted
    # one. Silence after this is a decision, not a lag.
    await asyncio.sleep(SETTLE_S)
    events = await messages(human, room_s3)
    assert len(by_sender(events, S3_BOT_A)) == 1, "the named agent answered more than once"
    assert by_sender(events, S3_BOT_B) == [], "both agents answered a line naming one of them"

    quiet = connectors[S3_BOT_B_NAME].log_text()
    assert "addressed to" in quiet and ", not me" in quiet, (
        "the other agent kept quiet for some other reason"
    )
    # The one that gives this gate teeth. Without the guard the other agent
    # reaches tier 2, draws its back-off, re-reads the room, finds the answer
    # that is already there and stands down - so the ROOM looks the same and
    # only the log can tell "that line was not mine" from "somebody beat me to
    # it" (measured 2026-09-04: the first cut of this gate passed its mutant).
    assert TIER_2 not in quiet, "the other agent went to tier 2: 3d did not decide it"
    assert not judged(connectors[S3_BOT_B_NAME]), (
        "the other agent paid for a judge call it did not need"
    )
