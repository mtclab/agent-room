"""Live gates for two agents having a conversation (C-1, C-2, C-3).

NOT the Claude-brain gates C1-C3 (`test_claude_brain.py`, `test_leak_probe.py`):
these are the hyphenated conversation gates, they spend no quota, and they run
on the echo brain like every other S3 journey.

The gap they close is the room log of 2026-09-04, two agents and one human:

1. our agent refused every line the friend's agent wrote, with
   `bot_to_bot=mentions: bot ... did not mention me` - because a model writes
   "@Qwen" and that is TEXT. No agent can make a Matrix mention, so `mentions`
   reading `m.mentions` alone made every other agent unreachable (C-2);
2. "tier 2 never triggers on a bot" then sealed the two of them off from each
   other completely, so nothing either one said could ever be joined in on
   (C-3, and `bot_to_bot: conversational` in C-1);
3. and the human's "you should just talk amongst yourselves" reached the judge
   as an ordinary unaddressed line, which answered "no, the conversation has
   naturally settled".

C-4 is the same failure one day later and one layer down (2026-09-05): with the
judge asked for a score instead of a verdict, "so, anyone here got an opinion
on whether weekends should be three days long?" still scored a 2 - *"it's a
general opinion question not directed at me"*. A line handed to the room and
asked something is turn ALLOCATION, so it no longer reaches the judge at all.

The echo brain's judge scores 9 for a trigger carrying `[[speak]]`, N for
`[[score: N]]`, and `brain.echo.score` otherwise - and the markers are stripped
out of what it echoes, so a marker steers ONE hop and never the whole chain.
That is what lets one gate hold both "they get talking" and "they stop".

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q tests/live/test_conversation.py
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
    addressable_names,
    agent_account,
    by_sender,
    in_thread,
    make_connector,
    messages,
    named_in,
    post_as_agent,
    post_unaddressed,
    wait_for,
    wait_for_join,
)
from nio import AsyncClient

pytestmark = [pytest.mark.live, LIVE_SKIP]

#: The invitation the room log was written around. It addresses nobody by name
#: and everybody by shape, which is exactly the line that used to be answered
#: with silence.
INVITATION = "[[speak]] you two, talk amongst yourselves about the weather"
#: Long enough for a back-off, a judge and a post to have happened. Silence
#: after this is a decision, not a lag.
SETTLE_S = 20.0
#: `policy::unaddressed`'s own words for a line that reached tier 2. Its
#: presence and its absence are both assertions here.
TIER_2 = "tier 2 candidate"
#: The line C-4 is built on, from the room log of 2026-09-05. It names nobody,
#: hands the turn to the room and asks it something - and it carries NO
#: `[[speak]]`, so an agent that asked its judge would be told 0 and say
#: nothing. The answer can only be the invitation path.
ROOM_QUESTION = "So, anyone here got an opinion on whether weekends should be three days long?"
#: The other half of C-4: unaddressed, and nobody waiting on it. Same tier, same
#: back-off, and the judge still decides it - which is G7's silence, measured
#: here with two agents watching.
PLAIN_LINE = "nice weather today"


def budgets(**extra: Any) -> dict[str, Any]:
    """Loop bounds with the pair budget out of the way.

    The pair budget would stop a bot-to-bot exchange long before the energy
    decay did, and C-1 is about the decay: raised to 20/min so that what stops
    the thread is the thread running out of things to say. Everything else is
    the shipped default.
    """
    values: dict[str, Any] = {
        "per_pair_per_minute": 20,
        "pair_cooldown_s": 60,
        "per_thread_max": 12,
        "per_hour_max": 30,
        "tier2_per_hour_max": 10,
        "bot_only_turns_before_decay": 6,
    }
    values.update(extra)
    return values


def localpart_of(user_id: str) -> str:
    return user_id.split(":", 1)[0].lstrip("@")


def bot_posts(events: list[dict[str, Any]], root: str) -> list[dict[str, Any]]:
    """Everything either agent posted into one thread, oldest first."""
    threaded = in_thread(events, root)
    both = by_sender(threaded, S3_BOT_A) + by_sender(threaded, S3_BOT_B)
    return sorted(both, key=lambda event: event["origin_server_ts"])


def agent_posts(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Everything either agent posted anywhere in the room."""
    return by_sender(events, S3_BOT_A) + by_sender(events, S3_BOT_B)


def mentions_of(event: dict[str, Any]) -> list[str]:
    return list(event.get("content", {}).get("m.mentions", {}).get("user_ids", []))


# -- C-1 ---------------------------------------------------------------------


@pytest.mark.timeout(420)
async def test_c1_two_agents_take_up_an_invitation_and_run_out_of_things_to_say(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """C-1: the human hands the turn to the room; both agents end up talking;
    they carry it on by NAME; and it stops on its own.

    The two back-offs are disjoint ([1,3] and [8,11]) so that which agent takes
    the invitation up is not a coin flip: the first one answers the human, the
    second stands down on that same line and is drawn in by the name in the
    answer. That second half is the one that used to be impossible - a bot's
    line naming a bot, with no `m.mentions` anywhere near it.

    Then the phase the whole slice is named after: with connector A stopped, its
    ACCOUNT posts a line naming nobody. Under `mentions` that line is refused
    before any guard reads it (C-3); under `conversational` it reaches tier 2,
    which is the only difference between the two modes.
    """
    connectors = {}
    for name, other, backoff in (
        (S3_BOT_A_NAME, S3_BOT_B_NAME, [1.0, 3.0]),
        (S3_BOT_B_NAME, S3_BOT_A_NAME, [8.0, 11.0]),
    ):
        connector = make_connector(
            tmp_path,
            tokens,
            name,
            room_s3,
            policy={
                "answer_unaddressed": True,
                "bot_to_bot": "conversational",
                "backoff_s": backoff,
                "budgets": budgets(),
            },
            # The typed name, not the user id: `mention_back` would put an
            # `m.mentions` on every hop and the gate would be measuring pills.
            brain={"kind": "echo", "echo": {"name_back": other}},
        )
        connector.start()
        running.append(connector)
        connector.wait_ready()
        connectors[name] = connector
    await wait_for_join(human, room_s3, [S3_BOT_A, S3_BOT_B])

    # -- the invitation ------------------------------------------------------
    root = await post_unaddressed(human, room_s3, INVITATION)
    events = await wait_for(
        lambda evs: (
            bool(by_sender(in_thread(evs, root), S3_BOT_A))
            and bool(by_sender(in_thread(evs, root), S3_BOT_B))
        ),
        human,
        room_s3,
        seconds=30,
    )
    posts = bot_posts(events, root)
    assert by_sender(posts, S3_BOT_A) and by_sender(posts, S3_BOT_B), (
        f"both agents had to be talking within 30 s; got {len(posts)} posts from "
        f"{sorted({p['sender'] for p in posts})}"
    )

    # The first hop is the one that could not happen before: A's answer to the
    # HUMAN names B in its body and mentions only the human, and B answered it.
    opening = posts[0]
    assert opening["sender"] == S3_BOT_A, "the faster back-off did not take the invitation up"
    assert mentions_of(opening) == [LIVE_HUMAN], (
        f"A's answer to the human must ping nobody else: {mentions_of(opening)}"
    )
    assert localpart_of(S3_BOT_B) in opening["content"]["body"], (
        "A's answer does not name B, so B cannot have been addressed by name"
    )
    assert "named in the body" in connectors[S3_BOT_B_NAME].log_text(), (
        "B answered a bot's line for some other reason than the name in it"
    )

    # -- and it keeps going, by name, until it runs out ----------------------
    events = await wait_for(lambda evs: len(bot_posts(evs, root)) >= 4, human, room_s3, seconds=60)
    posts = bot_posts(events, root)
    assert len(posts) >= 4, f"the two agents exchanged only {len(posts)} posts"
    for post in posts:
        peer = S3_BOT_B if post["sender"] == S3_BOT_A else S3_BOT_A
        assert localpart_of(peer) in post["content"]["body"], (
            f"{post['event_id']} does not address the other agent by name"
        )

    ceiling = budgets()["bot_only_turns_before_decay"] + 2
    assert len(posts) <= ceiling, (
        f"the bot-only thread ran to {len(posts)} posts, past the {ceiling} the decay allows"
    )
    # And it STAYS stopped: no timer and no budget window reopens it.
    await asyncio.sleep(30)
    still = bot_posts(await messages(human, room_s3), root)
    assert len(still) == len(posts), f"the thread restarted itself: {len(posts)} -> {len(still)}"
    assert any("energy decay" in c.log_text() for c in connectors.values()), (
        "the thread stopped for some other reason than the decay"
    )
    # B's own back-off on the human's line has long expired by now: it woke up,
    # found the room had been answered - by itself, in the exchange above - and
    # stood down. Nobody answered the invitation twice.
    assert "standing down" in connectors[S3_BOT_B_NAME].log_text(), (
        "B never reached the stand-down check on the human's line"
    )

    # -- conversational: a bot's unaddressed line reaches tier 2 -------------
    connectors[S3_BOT_A_NAME].terminate()
    async with agent_account(tokens, S3_BOT_A_NAME, room_s3) as agent:
        aside = "[[speak]] the weather has not improved since then"
        names = await addressable_names(human, room_s3)
        assert not named_in(aside, names), f"{aside!r} names somebody, so it is not unaddressed"
        before = len(by_sender(await messages(human, room_s3), S3_BOT_B))
        await post_as_agent(agent, room_s3, aside)
        events = await wait_for(
            lambda evs: len(by_sender(evs, S3_BOT_B)) > before, human, room_s3, seconds=40
        )

    assert len(by_sender(events, S3_BOT_B)) == before + 1, (
        "B never joined in on another agent's unaddressed line - which is the whole "
        "of what `bot_to_bot: conversational` is for"
    )
    log = connectors[S3_BOT_B_NAME].log_text()
    assert f"from {S3_BOT_A}: verdict=consider" in log, (
        "the line from the other agent never reached tier 2"
    )
    assert TIER_2 in log


# -- C-2 ---------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_c2_a_bot_that_types_a_name_is_answered_under_mentions(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """C-2: another agent says "Hello <name>, nice to meet you", with no
    `m.mentions` at all, and the shipped `bot_to_bot: mentions` answers it.

    This is the room log's first line, reproduced exactly: the sender is a real
    Matrix account posting a real `m.notice`, and the address is a typed name,
    because that is the only kind of address a model can write. No connector
    runs on that account - what it says is fixed, which is what makes the gate
    deterministic.

    The line carries no `[[speak]]`, so an agent that fell through to tier 2
    would be scored 0 and say nothing: the answer can only be the name.
    """
    bot = make_connector(
        tmp_path,
        tokens,
        S3_BOT_B_NAME,
        room_s3,
        policy={"answer_unaddressed": True, "bot_to_bot": "mentions", "backoff_s": [1.0, 3.0]},
    )
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_B])

    async with agent_account(tokens, S3_BOT_A_NAME, room_s3) as agent:
        greeting = f"Hello {localpart_of(S3_BOT_B)}, nice to meet you"
        names = await addressable_names(human, room_s3)
        assert named_in(greeting, names) == [localpart_of(S3_BOT_B).lower()], (
            f"{greeting!r} has to name the agent under test and nobody else"
        )
        await post_as_agent(agent, room_s3, greeting)
        events = await wait_for(
            lambda evs: bool(by_sender(evs, S3_BOT_B)), human, room_s3, seconds=40
        )

    replies = by_sender(events, S3_BOT_B)
    assert len(replies) == 1, f"expected one answer to a typed name from a bot, got {len(replies)}"
    log = bot.log_text()
    assert f"bot_to_bot=mentions: bot {S3_BOT_A} named me" in log, (
        "the bot_to_bot guard is not what let the line in"
    )
    assert "named in the body" in log, "and the name guard is not what answered it"
    assert TIER_2 not in log, "the line went to tier 2 instead of being answered at once"


# -- C-3 ---------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_c3_under_mentions_a_bots_unaddressed_line_never_reaches_tier_2(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """C-3: the other half of C-2. The same agent account says something that
    names nobody, and `bot_to_bot: mentions` refuses it at the switch - no
    back-off, no judge, no tier-2 candidate in the log.

    The line carries `[[speak]]`, so a connector that let it through to tier 2
    would answer it: silence here is the switch, and cannot be the judge
    declining. The human's own unaddressed line afterwards is the liveness half
    - the same shape of line, from a person, still reaches tier 2.
    """
    bot = make_connector(
        tmp_path,
        tokens,
        S3_BOT_B_NAME,
        room_s3,
        policy={"answer_unaddressed": True, "bot_to_bot": "mentions", "backoff_s": [1.0, 3.0]},
    )
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_B])

    async with agent_account(tokens, S3_BOT_A_NAME, room_s3) as agent:
        aside = "[[speak]] just thinking out loud about the weather"
        names = await addressable_names(human, room_s3)
        assert not named_in(aside, names), f"{aside!r} names somebody and cannot gate this"
        await post_as_agent(agent, room_s3, aside)
        await asyncio.sleep(SETTLE_S)

    events = await messages(human, room_s3)
    assert by_sender(events, S3_BOT_B) == [], "the agent joined in on another bot's aside"
    log = bot.log_text()
    assert "did not mention me" in log, "some other guard kept it quiet"
    assert TIER_2 not in log, "a bot's unaddressed line reached tier 2 under `mentions`"

    # Liveness: the same shape of line from a PERSON still does.
    await post_unaddressed(human, room_s3, "[[speak]] and now something worth answering")
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_B)), human, room_s3, seconds=40)
    assert len(by_sender(events, S3_BOT_B)) == 1, "tier 2 was not alive at all"
    assert TIER_2 in bot.log_text()


# -- C-4 ---------------------------------------------------------------------


@pytest.mark.timeout(300)
async def test_c4_a_room_question_is_answered_without_the_judge(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """C-4: the human asks the ROOM a question. Exactly one agent answers it,
    without asking a judge - and the plain line afterwards still needs one.

    Both judges are wired to refuse everything (`brain.echo.score: 0`, and no
    `[[speak]]` in either line), so nothing here can be answered by a judge
    happening to agree: the only thing that can post is the invitation path.
    The second half is the control, and it is G7's semantics with two agents
    watching - an unaddressed line nobody is waiting on reaches the judge, is
    told 0, and the room hears nothing.

    The back-offs are disjoint, as in G5 and C-1, so which agent takes the
    invitation up is not a coin flip: a pre-scored line draws from
    `backoff_s[0] .. +5`, which is 1-6 s for A and 8-13 s for B.

    `followup_window_s: 0` because of what the two halves are: the second line
    lands seconds after A's answer, and the follow-up arm would hand it to A as
    tier 1 - correctly, and with its own gate (N3). This gate is about tier 2,
    so the tier-1 arm that would decide the control line is switched off rather
    than worked around.
    """
    connectors = {}
    for name, backoff in ((S3_BOT_A_NAME, [1.0, 3.0]), (S3_BOT_B_NAME, [8.0, 11.0])):
        connector = make_connector(
            tmp_path,
            tokens,
            name,
            room_s3,
            policy={
                "answer_unaddressed": True,
                "backoff_s": backoff,
                "followup_window_s": 0,
            },
            brain={"kind": "echo", "echo": {"score": 0}},
        )
        connector.start()
        running.append(connector)
        connector.wait_ready()
        connectors[name] = connector
    await wait_for_join(human, room_s3, [S3_BOT_A, S3_BOT_B])

    # -- the room is asked something -----------------------------------------
    root = await post_unaddressed(human, room_s3, ROOM_QUESTION)
    await wait_for(lambda evs: bool(bot_posts(evs, root)), human, room_s3, seconds=SETTLE_S)
    # Long enough for the slower agent's back-off to have expired as well:
    # silence from it after this is a stand-down, not a lag.
    await asyncio.sleep(SETTLE_S)
    answers = bot_posts(await messages(human, room_s3), root)
    assert len(answers) == 1, (
        f"{len(answers)} agents answered one question put to the room: "
        f"{sorted(a['sender'] for a in answers)}"
    )
    assert answers[0]["sender"] == S3_BOT_A, "the faster back-off did not take the invitation up"

    winner = connectors[S3_BOT_A_NAME].log_text()
    loser = connectors[S3_BOT_B_NAME].log_text()
    assert "room invitation" in winner and "answering without the judge" in winner, (
        "the answer came from somewhere other than the invitation path"
    )
    assert "standing down" in loser, "the second agent never reached the stand-down check"
    # The whole point, in both logs: nothing was asked of a model but the
    # message itself. One skipped the judge; the other never got that far.
    for name, log in (("A", winner), ("B", loser)):
        assert "judge on" not in log, f"agent {name} asked its judge about a room invitation"

    # -- and a line nobody is waiting on still goes to the judge -------------
    before = len(agent_posts(await messages(human, room_s3)))
    plain = await post_unaddressed(human, room_s3, PLAIN_LINE)
    await asyncio.sleep(SETTLE_S)
    settled = await messages(human, room_s3)
    assert bot_posts(settled, plain) == [], "an agent answered a line both judges scored 0"
    assert len(agent_posts(settled)) == before, "an agent spoke somewhere else instead"
    for name in (S3_BOT_A_NAME, S3_BOT_B_NAME):
        log = connectors[name].log_text()
        assert "judge on" in log and "(< " in log, (
            f"agent {name} never asked the judge about the plain line, or it did not decline"
        )
