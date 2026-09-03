"""Live gates for the MCP server: a session in the room, on the shipped path.

Nothing here is mocked and nothing reaches into the module. Each test creates a
FRESH private room with the human account (playing the human), launches the real
`agent-room mcp` console script as a stdio subprocess, and drives it with the
MCP SDK's own client - the same way `claude mcp add` launches it and the same
way Claude Code would talk to it.

The session account is bot C and the daemon connector beside it is bot B.
The accounts the production connectors run on are never touched.

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q -m live tests/live/test_mcp.py
"""

from __future__ import annotations

import asyncio
import time
from pathlib import Path

import pytest
from conftest import (
    LIVE_HUMAN,
    LIVE_SKIP,
    MCP_PEER,
    MCP_PEER_NAME,
    MCP_SESSION,
    MCP_SESSION_NAME,
    Connector,
    Tokens,
    by_sender,
    make_connector,
    make_session,
    mcp_client,
    messages,
    post,
    relates_to,
    tool_error,
    tool_json,
    wait_for,
    wait_for_join,
)
from nio import AsyncClient

pytestmark = [pytest.mark.live, LIVE_SKIP]


# -- M1 ----------------------------------------------------------------------


@pytest.mark.timeout(180)
async def test_m1_the_session_reads_what_the_human_just_said(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_session: str
) -> None:
    """M1: `room_read` sees the human's message, with the right sender and ts."""
    session = make_session(tmp_path, tokens, MCP_SESSION_NAME, room_session)
    said = "hello from the human, this is M1"
    posted = await post(human, room_session, said)

    try:
        async with mcp_client(session) as client:
            rooms = tool_json(await client.call_tool("room_list", {}))
            assert [r["room_id"] for r in rooms] == [room_session]
            assert rooms[0]["members"] >= 2

            read = tool_json(
                await client.call_tool("room_read", {"room_id": room_session, "limit": 10})
            )
    finally:
        session.dump()

    mine = [m for m in read if m["event_id"] == posted]
    assert len(mine) == 1, f"the session did not see {posted}: {read}"
    message = mine[0]
    assert message["sender"] == LIVE_HUMAN
    assert message["body"] == said
    assert message["is_bot"] is False, "the human account is the human here"
    assert abs(message["ts"] - time.time()) < 300, f"implausible ts {message['ts']}"


# -- M2 ----------------------------------------------------------------------


@pytest.mark.timeout(180)
async def test_m2_the_session_answers_in_the_thread_and_mentions_the_human(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_session: str
) -> None:
    """M2: `room_post` with a thread_root lands as an m.notice in that thread,
    mentioning the human - the same shape a connector's reply has."""
    session = make_session(tmp_path, tokens, MCP_SESSION_NAME, room_session)
    trigger = await post(human, room_session, "M2: are you there, session?")

    try:
        async with mcp_client(session) as client:
            result = tool_json(
                await client.call_tool(
                    "room_post",
                    {
                        "room_id": room_session,
                        "body": "I am here, and I am answering in your thread.",
                        "thread_root": trigger,
                        "mention": [LIVE_HUMAN],
                    },
                )
            )
    finally:
        session.dump()

    assert result["msgtype"] == "m.notice"
    events = await wait_for(
        lambda evs: bool(by_sender(evs, MCP_SESSION)), human, room_session, seconds=30
    )
    posts = by_sender(events, MCP_SESSION)
    assert len(posts) == 1, f"expected exactly one post from the session, got {len(posts)}"
    content = posts[0]["content"]
    assert content["msgtype"] == "m.notice"
    assert LIVE_HUMAN in content["m.mentions"]["user_ids"]
    relation = relates_to(posts[0])
    assert relation["rel_type"] == "m.thread", "the answer must be in the thread it was asked in"
    assert relation["event_id"] == trigger


# -- M3 ----------------------------------------------------------------------


@pytest.mark.timeout(180)
async def test_m3_the_session_waits_and_wakes_when_somebody_speaks(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_session: str
) -> None:
    """M3: `room_wait` returns as soon as the human speaks, and returns nothing
    - after actually waiting - when nobody does. This is how a session listens."""
    session = make_session(tmp_path, tokens, MCP_SESSION_NAME, room_session)
    try:
        async with mcp_client(session) as client:
            # Force the join and the first sync before the wait under test.
            tool_json(await client.call_tool("room_read", {"room_id": room_session}))

            started = time.monotonic()
            waiting = asyncio.create_task(
                client.call_tool("room_wait", {"room_id": room_session, "timeout_s": 30})
            )
            await asyncio.sleep(3)
            said = "M3: waking you up"
            await post(human, room_session, said)
            heard = tool_json(await waiting)
            woke_in = time.monotonic() - started

            assert [m["body"] for m in heard] == [said], f"room_wait returned {heard}"
            assert woke_in < 10, f"the wait took {woke_in:.1f} s to notice a message"

            started = time.monotonic()
            quiet = tool_json(
                await client.call_tool("room_wait", {"room_id": room_session, "timeout_s": 5})
            )
            waited = time.monotonic() - started
    finally:
        session.dump()

    assert quiet == [], f"nobody spoke, but room_wait returned {quiet}"
    assert waited >= 4, f"the 5 s wait returned after {waited:.1f} s without waiting"


# -- M4 ----------------------------------------------------------------------


@pytest.mark.timeout(240)
async def test_m4_a_daemon_connector_answers_the_session_in_a_thread(
    tmp_path: Path,
    tokens: Tokens,
    human: AsyncClient,
    room_session_pair: str,
    running: list[Connector],
) -> None:
    """M4: the whole point - a live session and a daemon connector in one room.

    bot B runs the real connector with the echo brain; the session posts a line
    that mentions it; bot B answers in the thread the session started; and
    `room_read(thread_root=...)` gives the session both messages in order.
    """
    peer = make_connector(
        tmp_path, tokens, MCP_PEER_NAME, room_session_pair, policy={"answer_unaddressed": False}
    )
    peer.start()
    running.append(peer)
    peer.wait_ready()

    session = make_session(tmp_path, tokens, MCP_SESSION_NAME, room_session_pair)
    try:
        async with mcp_client(session) as client:
            tool_json(await client.call_tool("room_list", {}))
            await wait_for_join(human, room_session_pair, [MCP_SESSION, MCP_PEER])

            asked = tool_json(
                await client.call_tool(
                    "room_post",
                    {
                        "room_id": room_session_pair,
                        "body": f"M4: {MCP_PEER} what do you make of this?",
                        "mention": [MCP_PEER],
                    },
                )
            )
            root = asked["event_id"]

            events = await wait_for(
                lambda evs: bool(by_sender(evs, MCP_PEER)), human, room_session_pair, seconds=60
            )
            assert by_sender(events, MCP_PEER), "the connector never answered the session"

            thread = tool_json(
                await client.call_tool(
                    "room_read", {"room_id": room_session_pair, "thread_root": root}
                )
            )
    finally:
        session.dump()

    assert [m["sender"] for m in thread] == [MCP_SESSION, MCP_PEER], (
        f"the thread should read question then answer, got {thread}"
    )
    assert thread[0]["event_id"] == root
    assert "echo: " in thread[1]["body"]
    assert thread[1]["is_bot"] is True
    assert thread[1]["thread_root"] == root


# -- M5 ----------------------------------------------------------------------


@pytest.mark.timeout(180)
async def test_m5_the_budget_stops_a_session_flooding_the_room(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_session: str
) -> None:
    """M5: with an hourly cap of 2, the third post is refused as a tool error
    and never reaches the room."""
    session = make_session(
        tmp_path, tokens, MCP_SESSION_NAME, room_session, policy={"budgets": {"per_hour_max": 2}}
    )
    try:
        async with mcp_client(session) as client:
            for n in (1, 2):
                tool_json(
                    await client.call_tool(
                        "room_post", {"room_id": room_session, "body": f"M5: message {n}"}
                    )
                )
            refused = await client.call_tool(
                "room_post", {"room_id": room_session, "body": "M5: message 3, over the cap"}
            )
    finally:
        session.dump()

    assert refused.is_error, "the third post was not refused"
    assert "budget" in tool_error(refused), tool_error(refused)

    events = await messages(human, room_session)
    posts = by_sender(events, MCP_SESSION)
    assert len(posts) == 2, f"the room saw {len(posts)} posts from the session, expected 2"
    assert all("over the cap" not in event["content"]["body"] for event in posts)
