"""Live gates for tier 2: speaking when nobody asked.

Same shape as the S1 journeys - a fresh private room, the human account
playing the human, real `agent-room run` processes - with the echo brain, whose
judge says yes if and only if the trigger carries `[[speak]]`. No model is
involved, so these gates measure the connector's machinery (back-off,
stand-down, decay) and never a coin flip inside a model.

They run on bot C and bot D rather than the S1 pair: a production connector
runs on bot A, and a bot-to-bot burst on a shared account would eat its Synapse
rate limit.

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q -m "live and not claude"
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
    in_thread,
    make_connector,
    messages,
    post,
    relates_to,
    wait_for,
    wait_for_join,
)
from nio import AsyncClient

pytestmark = [pytest.mark.live, LIVE_SKIP]

#: G5's slower agent draws from here, and the gate waits this long plus a
#: margin before deciding that it really did stand down.
SLOW_BACKOFF = (6.0, 9.0)
SETTLE_S = 14.0


def tier2_policy(backoff: tuple[float, float], **extra: Any) -> dict[str, Any]:
    policy: dict[str, Any] = {"answer_unaddressed": True, "backoff_s": list(backoff)}
    policy.update(extra)
    return policy


def bot_posts(events: list[dict[str, Any]], root: str) -> list[dict[str, Any]]:
    """Everything either agent posted into one thread."""
    threaded = in_thread(events, root)
    return by_sender(threaded, S3_BOT_A) + by_sender(threaded, S3_BOT_B)


def all_replies(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return by_sender(events, S3_BOT_A) + by_sender(events, S3_BOT_B)


def answers_to(events: list[dict[str, Any]], trigger: str) -> list[dict[str, Any]]:
    """Agent messages threaded on one question."""
    return [event for event in all_replies(events) if relates_to(event).get("event_id") == trigger]


# -- G5 ----------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_g5_exactly_one_of_two_bots_answers_an_unaddressed_question(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G5: the human asks the room, not a person. Both agents want to answer
    (the judge says yes to both), one of them gets there first, and the other
    re-reads the room and stands down. Three rounds, because "exactly one" has
    to hold every time, not on average.

    The two back-off ranges are deliberately DISJOINT ([1,3] and [6,9]) and the
    gate asserts the loser's own stand-down log line as well as the count. With
    a shared range this gate would be a coin flip: two agents that draw within
    ~250 ms of each other - the time it takes a posted message to come back
    through /sync - both answer, by construction and by design (people talk over
    each other too). What is testable is the mechanism, and that is what this
    asserts; the draw itself is a unit test
    (`test_the_backoff_is_drawn_from_the_configured_range`).
    """
    first = make_connector(
        tmp_path, tokens, S3_BOT_A_NAME, room_s3, policy=tier2_policy((1.0, 3.0))
    )
    second = make_connector(
        tmp_path, tokens, S3_BOT_B_NAME, room_s3, policy=tier2_policy(SLOW_BACKOFF)
    )
    for connector in (first, second):
        connector.start()
        running.append(connector)
        connector.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A, S3_BOT_B])

    for round_number in (1, 2, 3):
        before = len(all_replies(await messages(human, room_s3)))
        trigger = await post(human, room_s3, f"[[speak]] anyone around? (round {round_number})")
        await wait_for(
            lambda evs, t=trigger: bool(answers_to(evs, t)),
            human,
            room_s3,
            seconds=30,
        )

        # Long enough for the slower agent's back-off to expire and for it to
        # post if it were going to. Silence after this is a decision, not a lag.
        await asyncio.sleep(SETTLE_S)
        settled = await messages(human, room_s3)
        answers = answers_to(settled, trigger)
        assert len(answers) == 1, (
            f"round {round_number}: {len(answers)} agents answered one question"
        )
        assert len(all_replies(settled)) == before + 1, (
            f"round {round_number}: an agent answered somewhere else as well"
        )

        answer = answers[0]
        assert answer["content"]["msgtype"] == "m.notice"
        assert LIVE_HUMAN in answer["content"]["m.mentions"]["user_ids"]

    # The count alone would also be satisfied by a dead connector. This is the
    # loser saying, in its own log, why it kept quiet.
    log = second.log_text()
    assert "standing down" in log and "someone answered first" in log, (
        "the second agent never reached the stand-down check"
    )
    assert log.count("standing down") >= 3, "one stand-down per round, three rounds"


# -- G6 ----------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_g6_a_bot_only_thread_winds_down_and_a_human_revives_it(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G6: two agents that answer every mention, each one mentioning the other,
    told to stop by nothing but the conversation running out of energy.

    The pair budget is deliberately raised to 20/min so it cannot be the thing
    that stops them: with the decay removed, this same configuration runs away
    (see the teeth run in docs/GATES.md).
    """
    budgets = {
        "per_pair_per_minute": 20,
        "pair_cooldown_s": 60,
        "per_thread_max": 12,
        "per_hour_max": 30,
        "tier2_per_hour_max": 10,
        "bot_only_turns_before_decay": 4,
    }
    policy = {"answer_unaddressed": False, "bot_to_bot": "mentions", "budgets": budgets}
    connectors = {}
    for name, other in ((S3_BOT_A_NAME, S3_BOT_B), (S3_BOT_B_NAME, S3_BOT_A)):
        connector = make_connector(
            tmp_path,
            tokens,
            name,
            room_s3,
            policy=policy,
            brain={"kind": "echo", "echo": {"mention_back": other}},
        )
        connector.start()
        running.append(connector)
        connector.wait_ready()
        connectors[name] = connector
    await wait_for_join(human, room_s3, [S3_BOT_A, S3_BOT_B])

    # One human message to start it, and then nothing from any human.
    root = await post(
        human, room_s3, f"{S3_BOT_A} please say hello to {S3_BOT_B}", mentions=[S3_BOT_A]
    )
    ceiling = budgets["bot_only_turns_before_decay"] + 2

    await asyncio.sleep(45)
    events = await messages(human, room_s3)
    posts = bot_posts(events, root)
    assert len(posts) >= 2, f"the two agents never got talking: {len(posts)} posts"
    assert len(posts) <= ceiling, (
        f"the bot-only thread ran to {len(posts)} posts, past the {ceiling} the decay allows"
    )

    # And it STAYS stopped: no timer, no budget window reopening it.
    await asyncio.sleep(30)
    still = bot_posts(await messages(human, room_s3), root)
    assert len(still) == len(posts), (
        f"the thread restarted itself: {len(posts)} -> {len(still)} posts"
    )

    # A human in the thread refills it, and the same mention is answered again.
    before_a = len(by_sender(in_thread(still, root), S3_BOT_A))
    await post(
        human,
        room_s3,
        f"{S3_BOT_A} still with us?",
        mentions=[S3_BOT_A],
        thread_root=root,
    )
    events = await wait_for(
        lambda evs: len(by_sender(in_thread(evs, root), S3_BOT_A)) > before_a,
        human,
        room_s3,
        seconds=40,
    )
    assert len(by_sender(in_thread(events, root), S3_BOT_A)) > before_a, (
        "a human post did not bring the thread back to life"
    )
    assert "energy decay" in connectors[S3_BOT_A_NAME].log_text(), (
        "the thread stopped for some other reason than the decay"
    )


# -- G7 ----------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_g7_an_unaddressed_line_the_judge_declines_is_left_alone(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G7: tier 2 is ON, the message is unaddressed, and the judge says no. The
    room hears nothing.

    The liveness half runs through the SAME path: a second unaddressed line that
    the judge does accept. A silence-only gate would also pass with a dead
    connector, and a mention would prove tier 1, not tier 2.
    """
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3, policy=tier2_policy((1.0, 3.0)))
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    await post(human, room_s3, "just thinking aloud about the weather")
    await asyncio.sleep(20)
    events = await messages(human, room_s3)
    assert by_sender(events, S3_BOT_A) == [], "the agent answered a line its judge declined"
    assert "speak=False" in bot.log_text(), "the judge was never asked"

    await post(human, room_s3, "[[speak]] and now something worth answering")
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    assert len(by_sender(events, S3_BOT_A)) == 1, "tier 2 never spoke at all - was it alive?"


# -- G8 ----------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_g8_a_heartbeat_speaks_into_a_quiet_room_and_addresses_nobody(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G8: tier 3 on the shipped path. Nobody has said anything for a minute, so
    the agent brings something up by itself - unthreaded, mentioning nobody.

    The seeding message is posted BEFORE the connector starts, which is also
    what makes the gate quick: it is backlog, so it is never answered, but it is
    in the transcript for the heartbeat to have something to be about, and the
    room is quiet from the moment the connector opens its eyes.
    """
    await post(human, room_s3, "[[speak]] leaving this here before anyone joins")

    bot = make_connector(
        tmp_path,
        tokens,
        S3_BOT_A_NAME,
        room_s3,
        # Tier 2 off, so the only thing that can post here is tier 3.
        policy={"answer_unaddressed": False, "heartbeat_minutes": 1},
    )
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=150)
    posts = by_sender(events, S3_BOT_A)
    assert len(posts) == 1, f"expected one heartbeat, got {len(posts)}"

    content = posts[0]["content"]
    assert content["msgtype"] == "m.notice"
    assert "m.relates_to" not in content, "a heartbeat is not a reply to anything"
    assert "m.mentions" not in content, "a heartbeat pings nobody"
    assert "heartbeat posted" in bot.log_text()
