"""Live gate T1: a transcript that rolls under a real connector (issue #13).

The transcript is the agent's memory, and until #13 it was append-only for
ever. Rolling it is only worth anything if the agent keeps working across the
roll, and nothing but a real connector against a real homeserver can show that:
the file turns over WHILE the sync loop is appending to it and turns are
reading it back.

So: one `agent-room run` with `transcript_keep: 20` and
`transcript_archives: 2`, 25 mentions a second apart - about 75 records, six or
seven rolls - and then the questions that matter. Is the live file still
bounded? Is what rolled away in `<room>.jsonl.1`? Was any message dropped on
the way past? And does a mention arriving after all that still get answered in
a thread the agent opened before the first roll?

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q tests/live/test_rotation.py

The accounts, the fresh-room fixture and the `agent-room run` wrapper are the
live harness section of `tests/conftest.py`.
"""

from __future__ import annotations

import asyncio
import re
import time
from pathlib import Path

import pytest
from conftest import (
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

#: The cap this gate runs the connector at, and how many archives it keeps.
#: Tiny on purpose: rolling the shipped 5000-event cap in a test would take an
#: afternoon, and what is under test is the roll, not the number.
KEEP = 20
ARCHIVES = 2
#: Mentions the human fires, a second apart. Each one is three records in the
#: transcript (the message, my reply, my reply coming back through /sync), so
#: this is about 75 records into a 20-line file.
MENTIONS = 25
#: Where a connector's own log says it rolled, and where it says it could not.
ROLLED = "rolled the transcript"
COULD_NOT_ROLL = "cannot roll the transcript"


def transcript_path(connector: Connector, room_id: str) -> Path:
    """The live transcript for `room_id` under this connector's state dir.

    The layout and the sanitisation are `config::room_state_path`'s, pinned by
    `tests/state_compat.rs`: `<state_dir>/rooms/<room id>.jsonl`, with anything
    outside `[A-Za-z0-9_.-]` replaced by an underscore.
    """
    return connector.state_dir / "rooms" / (re.sub(r"[^A-Za-z0-9_.-]", "_", room_id) + ".jsonl")


def archive(live: Path, index: int) -> Path:
    return Path(f"{live}.{index}")


def lines(path: Path) -> list[str]:
    """Non-empty lines in a transcript, or none when it is not there."""
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8", errors="replace")
    return [line for line in text.splitlines() if line.strip()]


def accounted_for(events: list[dict], log: str) -> set[str]:
    """Every human event the agent DEALT with, answered or deliberately not.

    A message that arrives while a turn is running is coalesced into the turn
    that follows it - the connector says so in its log, and marks it consumed -
    so "answered all 25" is "answered or coalesced, all 25". Anything the agent
    dropped on the floor appears in neither set, which is the failure this
    gate is looking for.
    """
    answered = {
        relates_to(reply).get("m.in_reply_to", {}).get("event_id")
        for reply in by_sender(events, S3_BOT_A)
    }
    coalesced = set(re.findall(r"coalesced (\S+) into the turn", log))
    return {event_id for event_id in answered if event_id} | coalesced


@pytest.mark.timeout(300)
async def test_t1_a_rolling_transcript_stays_bounded_and_the_agent_keeps_up(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """T1: 25 mentions into a connector capped at 20 transcript lines. The live
    file stays under the cap, the older records are in `<room>.jsonl.1`, no
    message went unanswered and unlogged, and a mention after all of it is
    still answered in the thread the agent opened before the first roll."""
    bot = make_connector(
        tmp_path,
        tokens,
        S3_BOT_A_NAME,
        room_s3,
        # 26 replies in a few minutes, so the hourly cost guard has to be out of
        # the way: this gate is about the file, and a budget refusal would look
        # exactly like an agent that went blind at a roll.
        policy={
            "budgets": {
                "per_pair_per_minute": 20,
                "pair_cooldown_s": 60,
                "per_thread_max": 12,
                "per_hour_max": 100,
            }
        },
        transcript_keep=KEEP,
        transcript_archives=ARCHIVES,
    )
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    triggers = []
    for index in range(MENTIONS):
        triggers.append(
            await post(
                human,
                room_s3,
                f"{S3_BOT_A} message {index + 1} of {MENTIONS}",
                mentions=[S3_BOT_A],
            )
        )
        await asyncio.sleep(1)

    # Every one of them either answered or coalesced into the answer that
    # followed it. Polled rather than slept through: the last few answers are
    # still being written when the loop above ends.
    handled: set[str] = set()
    deadline = time.monotonic() + 120
    events: list[dict] = []
    while time.monotonic() < deadline:
        events = await messages(human, room_s3)
        handled = accounted_for(events, bot.log_text())
        if set(triggers) <= handled:
            break
        await asyncio.sleep(2)
    missing = [event_id for event_id in triggers if event_id not in handled]
    assert not missing, f"{len(missing)} of {MENTIONS} mentions were neither answered nor coalesced"

    # It was really answering, not coalescing everything into one turn - the
    # transcript has to have grown for any of this to mean anything.
    replies = by_sender(events, S3_BOT_A)
    assert len(replies) >= 5, f"only {len(replies)} replies: the agent was barely alive"

    # THE POINT. The live file is bounded, and what left it is beside it.
    live = transcript_path(bot, room_s3)
    held = lines(live)
    assert len(held) <= KEEP, (
        f"the live transcript holds {len(held)} lines with transcript_keep={KEEP}: it did not roll"
    )
    assert archive(live, 1).exists(), f"nothing was archived to {archive(live, 1)}"
    assert lines(archive(live, 1)), "the archive is empty"
    assert not archive(live, ARCHIVES + 1).exists(), (
        f"archives are kept past transcript_archives={ARCHIVES}"
    )
    log = bot.log_text()
    assert ROLLED in log, "the connector never said it rolled anything"
    assert COULD_NOT_ROLL not in log, f"a roll failed:\n{log}"

    # And the agent is not blind: a mention in the thread it opened on the very
    # first message - long since rolled out of the live file - is still answered
    # there, in that thread, rather than in a new one.
    followup = await post(
        human,
        room_s3,
        f"{S3_BOT_A} still with me?",
        mentions=[S3_BOT_A],
        thread_root=triggers[0],
    )
    answers = []

    def answered_followup(evs: list[dict]) -> bool:
        answers[:] = [
            reply
            for reply in by_sender(evs, S3_BOT_A)
            if relates_to(reply).get("m.in_reply_to", {}).get("event_id") == followup
        ]
        return bool(answers)

    await wait_for(answered_followup, human, room_s3, seconds=60)
    assert answers, "the mention after the rolls went unanswered"
    relation = relates_to(answers[0])
    assert relation["rel_type"] == "m.thread"
    assert relation["event_id"] == triggers[0], (
        "the answer left the thread the question was asked in - thread() did not survive the roll"
    )
