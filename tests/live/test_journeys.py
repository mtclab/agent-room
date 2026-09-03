"""Live journey gates: the real connector process against the real homeserver.

Nothing here is mocked. Each test creates a FRESH private room with the
account that plays the human, invites the two bot accounts, starts real
`agent-room run` processes, and asserts what the human sees through
`/messages`. Teardown leaves and forgets the room with all three accounts.

The binary under test is whatever `AGENT_ROOM_BIN` names (`target/release/agent-room`
by default), so the teeth runner can point the same four journeys at a mutant
build.

The bots are the S3 pair (bot C, bot D) rather than the S1 pair: production
connectors run on some of the accounts from this box, and Synapse rate-limits
per user - a gate must never share an account with something that is really
talking to somebody.

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q tests/live/test_journeys.py

The accounts, the fresh-room fixture, the `agent-room run` wrapper and the
assertion helpers are the live harness section of `tests/conftest.py`; this
module holds only the gates. Pre-existing rooms are never touched, and the room
id in matrix_room.json is never read.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from conftest import (
    LIVE_HUMAN,
    LIVE_SKIP,
    PAIR_PER_MINUTE,
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


# -- G1 ----------------------------------------------------------------------


@pytest.mark.timeout(180)
async def test_g1_a_mention_is_answered_in_thread(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G1: the human mentions bot C; bot C answers within 30 s, in a thread on
    the human's event, as an m.notice, mentioning the human back."""
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3)
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    trigger = await post(human, room_s3, f"{S3_BOT_A} hello there", mentions=[S3_BOT_A])

    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    replies = by_sender(events, S3_BOT_A)
    assert len(replies) == 1, f"expected exactly one reply, got {len(replies)}"

    reply = replies[0]
    content = reply["content"]
    assert content["msgtype"] == "m.notice"
    assert "echo: " in content["body"]
    assert LIVE_HUMAN in content["m.mentions"]["user_ids"]
    relation = relates_to(reply)
    assert relation["rel_type"] == "m.thread"
    assert relation["event_id"] == trigger
    assert relation["m.in_reply_to"]["event_id"] == trigger


# -- G2 ----------------------------------------------------------------------


@pytest.mark.timeout(180)
async def test_g2_an_unaddressed_message_is_left_alone(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G2: nobody is addressed and tier 2 is switched off, so both bots stay
    quiet for 20 s.

    `answer_unaddressed: false` is now explicit. Since S3 the shipped default is
    true, and with it on this gate would be measuring the tier-2 judge (which is
    G7's job) instead of the guard it is named after.
    """
    for name in (S3_BOT_A_NAME, S3_BOT_B_NAME):
        connector = make_connector(
            tmp_path, tokens, name, room_s3, policy={"answer_unaddressed": False}
        )
        connector.start()
        running.append(connector)
    for connector in running:
        connector.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A, S3_BOT_B])

    await post(human, room_s3, "just thinking aloud")

    await asyncio.sleep(20)
    events = await messages(human, room_s3)
    assert by_sender(events, S3_BOT_A) == [], "bot C answered an unaddressed message"
    assert by_sender(events, S3_BOT_B) == [], "bot D answered an unaddressed message"

    # Silence proves nothing unless the bots were alive to break it: a dead
    # connector would pass the assertions above. Address them and they must answer.
    await post(
        human,
        room_s3,
        f"{S3_BOT_A} {S3_BOT_B} now I am asking you",
        mentions=[S3_BOT_A, S3_BOT_B],
    )
    events = await wait_for(
        lambda evs: bool(by_sender(evs, S3_BOT_A)) and bool(by_sender(evs, S3_BOT_B)),
        human,
        room_s3,
        seconds=30,
    )
    assert len(by_sender(events, S3_BOT_A)) == 1, "bot C was not alive to answer"
    assert len(by_sender(events, S3_BOT_B)) == 1, "bot D was not alive to answer"


# -- G3 ----------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_g3_bot_to_bot_traffic_is_capped_by_the_pair_budget(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G3: connector A fires 12 mentions at connector B in one minute. B has
    bot_to_bot=mentions and a 3-per-minute pair budget, so it answers at most
    three times, never answers itself, and the chain does not run away."""
    bot_b = make_connector(
        tmp_path,
        tokens,
        S3_BOT_B_NAME,
        room_s3,
        policy={
            "bot_to_bot": "mentions",
            # The decay is put out of the way ON PURPOSE. A bot-only thread of
            # 3 spams and 3 answers is 6 bot-authored messages, which is exactly
            # the shipped `bot_only_turns_before_decay` - so with the pair
            # budget removed the thread wound down after the same three replies
            # and this gate passed while testing nothing. Found by the teeth run
            # on 2026-09-03; the gate has been masked since S3 added the decay.
            "budgets": {
                "per_pair_per_minute": PAIR_PER_MINUTE,
                "pair_cooldown_s": 60,
                "per_thread_max": 12,
                "per_hour_max": 30,
                "bot_only_turns_before_decay": 100,
            },
        },
    )
    bot_b.start()
    running.append(bot_b)
    bot_b.wait_ready()

    # A ignores bots entirely: this gate measures B's budget, not a duet.
    bot_a = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3, policy={"bot_to_bot": "none"})
    bot_a.start({"AGENT_ROOM_TEST_SPAM": S3_BOT_B})
    running.append(bot_a)
    bot_a.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A, S3_BOT_B])

    # 12 spam messages at 1 s intervals, then a full cooldown window.
    await asyncio.sleep(90)
    events = await messages(human, room_s3)

    spam = by_sender(events, S3_BOT_A)
    assert len(spam) >= 10, f"the spam generator only posted {len(spam)} messages"
    spam_ids = {event["event_id"] for event in spam}

    replies = by_sender(events, S3_BOT_B)
    assert len(replies) <= PAIR_PER_MINUTE, (
        f"bot D posted {len(replies)} replies, over the {PAIR_PER_MINUTE}/min pair budget"
    )
    # At least one, or the cap would be indistinguishable from a dead connector.
    # Not exactly three: messages arriving during a turn are coalesced by design,
    # so the guarantee is the ceiling, not the count.
    assert len(replies) >= 1, "bot D answered nothing at all - was it alive?"

    reply_ids = {event["event_id"] for event in replies}
    for reply in replies:
        answered = relates_to(reply).get("m.in_reply_to", {}).get("event_id")
        assert answered in spam_ids, f"bot D answered {answered}, which is not one of A's messages"
        assert answered not in reply_ids, "bot D answered one of its own posts"
        assert reply["content"]["m.mentions"]["user_ids"] == [S3_BOT_A]


# -- G4 ----------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_g4_a_restart_never_answers_twice_or_answers_the_backlog(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """G4: kill -9 mid-thread and restart on the same state_dir. The answered
    event is not answered again, the backlog that arrived while the connector was
    down is not answered either, and the restarted connector is still alive."""
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3)
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    first = await post(human, room_s3, f"{S3_BOT_A} first question", mentions=[S3_BOT_A])
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    assert len(by_sender(events, S3_BOT_A)) == 1, "no reply to answer twice"

    bot.kill()

    # Traffic the connector missed entirely: it must not wake up and answer it.
    backlog = await post(human, room_s3, f"{S3_BOT_A} while you were away", mentions=[S3_BOT_A])

    bot.start()
    bot.wait_ready()
    await asyncio.sleep(20)

    events = await messages(human, room_s3)
    replies = by_sender(events, S3_BOT_A)
    assert len(replies) == 1, f"the restart produced {len(replies)} replies, expected 1"
    answered = relates_to(replies[0]).get("m.in_reply_to", {}).get("event_id")
    assert answered == first
    assert backlog not in {
        relates_to(reply).get("m.in_reply_to", {}).get("event_id") for reply in replies
    }

    # A live connector, not a dead one: a fresh mention is still answered.
    fresh = await post(human, room_s3, f"{S3_BOT_A} are you back?", mentions=[S3_BOT_A])
    events = await wait_for(
        lambda evs: len(by_sender(evs, S3_BOT_A)) >= 2, human, room_s3, seconds=30
    )
    replies = by_sender(events, S3_BOT_A)
    assert len(replies) == 2, "the restarted connector never answered a fresh mention"
    assert fresh in {
        relates_to(reply).get("m.in_reply_to", {}).get("event_id") for reply in replies
    }
