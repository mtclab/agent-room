"""Live gates X-1 to X-3: what is a line of conversation, and what corrects one.

Three things a real room does that rc.5 got wrong, each driven through a real
`agent-room run` against the real homeserver:

- **X-1** somebody posts a picture, a file or a location. It is not something
  anybody SAID, so the agent says nothing about it - and until rc.6 an image
  addressed to the agent was answered as if its filename were a question.
- **X-2** somebody fixes a typo (`m.replace`). The transcript line is corrected;
  the room hears nothing, because a corrected question is not a new one.
- **X-3** somebody takes a message back (`m.room.redaction`). The text leaves
  the agent's memory of the room, so nothing feeds it back to the brain as
  history until the transcript happens to roll.

X-1 and X-3 assert on the agent's own memory - the live transcript under its
state directory, which is exactly what a turn is handed as history - as well as
on what the room hears. That is the point of all three: the room being quiet is
half the promise, and the other half is what the agent still remembers.

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q tests/live/test_corrections.py

The accounts, the fresh-room fixture, the `agent-room run` wrapper and the
assertion helpers are the live harness section of `tests/conftest.py`.
"""

from __future__ import annotations

import asyncio
import json
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

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
    redact,
    send_content,
    transcript_records,
    wait_for,
    wait_for_join,
)
from nio import AsyncClient

pytestmark = [pytest.mark.live, LIVE_SKIP]

#: X-1 reads the connector's own reason for the silence out of its log, and the
#: reason is a DEBUG line: an operator wondering where a picture went turns this
#: on, so the gate turns it on too. The rest is the shipped default filter.
DEBUG_LOG = (
    "agent_room=debug,matrix_sdk=warn,matrix_sdk_base=warn,matrix_sdk_crypto=warn,"
    "matrix_sdk_sqlite=warn,hyper=warn,reqwest=warn"
)
#: How long a gate waits for silence to prove itself. Long enough to cover the
#: tier-2 back-off (5-40 s would be the other explanation for a quiet room).
SILENCE_S = 45
#: A word nothing else in the room says, so "it is gone" is decidable.
CODE_WORD = "kumquat-nine"


async def eventually(check: Callable[[], bool], seconds: float, what: str) -> None:
    """Poll a file-on-disk condition until it holds, or fail saying what did not."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if check():
            return
        await asyncio.sleep(1.0)
    raise AssertionError(what)


def bodies(records: list[dict[str, Any]]) -> list[str]:
    return [record["event"]["body"] for record in records]


def records_of(records: list[dict[str, Any]], event_id: str) -> list[dict[str, Any]]:
    return [record for record in records if record["event"]["event_id"] == event_id]


def still_says(events: list[dict[str, Any]], sender: str) -> list[dict[str, Any]]:
    """What `sender` has said and NOT taken back.

    A redacted event stays in the timeline with an empty content, so counting
    `by_sender` alone would count a husk as a message - and X-3 redacts one of
    the agent's own posts on purpose.
    """
    return [event for event in by_sender(events, sender) if event.get("content", {}).get("body")]


# -- X-1 ---------------------------------------------------------------------


@pytest.mark.timeout(240)
async def test_x1_a_picture_is_not_something_somebody_said(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """X-1: an image, a file and a location, every one of them addressed to the
    agent by a pill. The room hears nothing, the agent's memory holds nothing,
    and its log says why - while the same filename typed as text is answered."""
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3)
    bot.start({"RUST_LOG": DEBUG_LOG})
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    # `m.mentions` on purpose, and it is what makes this gate bite: an image
    # nobody addressed would be left alone by tier 2 anyway, so the gate would
    # pass on the wrong reason. Addressed, it is tier 1 - the fast path with no
    # judge in front of it.
    posted = []
    for content in (
        {
            "msgtype": "m.image",
            "body": "IMG_4021.png",
            "url": "mxc://example.com/notarealupload",
            "m.mentions": {"user_ids": [S3_BOT_A]},
        },
        {
            "msgtype": "m.file",
            "body": "the-numbers.xlsx",
            "url": "mxc://example.com/notarealupload",
            "m.mentions": {"user_ids": [S3_BOT_A]},
        },
        {
            "msgtype": "m.location",
            "body": "where I am right now",
            "geo_uri": "geo:0.0,0.0",
            "m.mentions": {"user_ids": [S3_BOT_A]},
        },
    ):
        posted.append(await send_content(human, room_s3, content))

    await asyncio.sleep(SILENCE_S)
    events = await messages(human, room_s3)
    assert by_sender(events, S3_BOT_A) == [], "the agent answered a picture"

    remembered = transcript_records(bot, room_s3)
    assert remembered == [], f"a picture is in the agent's memory of the room: {bodies(remembered)}"
    log = bot.log_text()
    assert "m.image is not a line of conversation" in log, (
        "the agent was quiet, but not because of the message filter"
    )
    for msgtype in ("m.file", "m.location"):
        assert f"{msgtype} is not a line of conversation" in log

    # Alive, and the same filename typed as TEXT is answered: the silence above
    # is the filter rather than a connector that does nothing.
    await post(human, room_s3, f"{S3_BOT_A} IMG_4021.png", mentions=[S3_BOT_A])
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    replies = by_sender(events, S3_BOT_A)
    assert len(replies) == 1, "the agent never answered anything, picture or not"
    assert "IMG_4021.png" in replies[0]["content"]["body"]
    assert not records_of(transcript_records(bot, room_s3), posted[0])


# -- X-2 ---------------------------------------------------------------------


@pytest.mark.timeout(240)
async def test_x2_an_edit_corrects_the_line_and_is_never_a_new_one(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """X-2: the human asks with a typo, the agent answers, the human fixes the
    typo. The transcript line is corrected in place, the `* corrected text`
    fallback never becomes history, and the agent does not answer again."""
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3)
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    typo = await post(human, room_s3, f"{S3_BOT_A} what di you thnik?", mentions=[S3_BOT_A])
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    assert len(by_sender(events, S3_BOT_A)) == 1, "the typo itself was never answered"

    # What a client sends when you fix a message: the `* ...` fallback for
    # clients that cannot render an edit, the real text in `m.new_content`, and
    # the pill repeated inside it (an edit must not re-notify, so the top-level
    # mentions are empty - MSC3952).
    await send_content(
        human,
        room_s3,
        {
            "msgtype": "m.text",
            "body": f"* {S3_BOT_A} what do you think?",
            "m.mentions": {"user_ids": []},
            "m.new_content": {
                "msgtype": "m.text",
                "body": f"{S3_BOT_A} what do you think?",
                "m.mentions": {"user_ids": [S3_BOT_A]},
            },
            "m.relates_to": {"rel_type": "m.replace", "event_id": typo},
        },
    )

    def corrected() -> bool:
        remembered = records_of(transcript_records(bot, room_s3), typo)
        return "what do you think?" in " ".join(bodies(remembered))

    await eventually(corrected, 30, "the edit never reached the agent's memory of the line")
    await asyncio.sleep(SILENCE_S)

    events = await messages(human, room_s3)
    assert len(by_sender(events, S3_BOT_A)) == 1, (
        "the agent answered the correction as if it were a new question"
    )
    remembered = transcript_records(bot, room_s3)
    for_the_line = records_of(remembered, typo)
    assert len(for_the_line) == 1, f"the edit became a second line: {bodies(for_the_line)}"
    assert for_the_line[0]["event"]["body"] == f"{S3_BOT_A} what do you think?"
    assert not [body for body in bodies(remembered) if body.startswith("*")], (
        f"the `* corrected text` fallback is in the agent's history: {bodies(remembered)}"
    )

    # Still alive: a fresh mention is still answered.
    await post(human, room_s3, f"{S3_BOT_A} are you still there?", mentions=[S3_BOT_A])
    events = await wait_for(
        lambda evs: len(by_sender(evs, S3_BOT_A)) >= 2, human, room_s3, seconds=30
    )
    assert len(by_sender(events, S3_BOT_A)) == 2, "the connector went quiet for good after an edit"


# -- X-3 ---------------------------------------------------------------------


@pytest.mark.timeout(240)
async def test_x3_a_redaction_takes_the_text_out_of_the_agents_history(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    """X-3: the human says something and takes it back. Every record of the
    redacted event leaves the live transcript, so nothing feeds it back to the
    brain as history - and redacting the agent's own answer takes that too."""
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3)
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await wait_for_join(human, room_s3, [S3_BOT_A])

    said = await post(
        human, room_s3, f"{S3_BOT_A} the code word is {CODE_WORD}", mentions=[S3_BOT_A]
    )
    events = await wait_for(lambda evs: bool(by_sender(evs, S3_BOT_A)), human, room_s3, seconds=30)
    answers = by_sender(events, S3_BOT_A)
    assert len(answers) == 1
    # The echo brain quotes what it answers, so the agent's own reply carries
    # the word as well. That is the honest shape of a redaction: taking one
    # message back does not take back what somebody else said about it.
    answer = answers[0]["event_id"]
    assert CODE_WORD in answers[0]["content"]["body"]

    await redact(human, room_s3, said, "wrong room")
    await eventually(
        lambda: not records_of(transcript_records(bot, room_s3), said),
        30,
        "the redacted message is still in the agent's memory of the room",
    )
    assert "redacted" in bot.log_text()

    await redact(human, room_s3, answer, "and the answer to it")
    await eventually(
        lambda: CODE_WORD not in json.dumps(transcript_records(bot, room_s3)),
        30,
        "the redacted text is still what the agent would hand its brain as history",
    )

    # A redaction is not something to answer, and it did not kill the connector.
    await asyncio.sleep(SILENCE_S)
    events = await messages(human, room_s3)
    assert still_says(events, S3_BOT_A) == [], "the agent said something about a redaction"
    await post(human, room_s3, f"{S3_BOT_A} still there?", mentions=[S3_BOT_A])
    events = await wait_for(lambda evs: bool(still_says(evs, S3_BOT_A)), human, room_s3, seconds=30)
    assert len(still_says(events, S3_BOT_A)) == 1, (
        "the connector went quiet for good after a redaction"
    )
