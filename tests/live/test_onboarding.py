"""Live gates for onboarding: `agent-room doctor` against the real homeserver.

The module is named for the slice rather than for the command because pytest
resolves test modules by BASENAME - `tests/live/test_doctor.py` beside
`tests/test_doctor.py` collides, the same way a second `conftest.py` under
`tests/live/` would.

D1 is the whole point of the command: a friend who has just run `init` types one
thing and is told whether it will work. So this gate runs the installed console
script against a config built from a real bot token and a real room the account
has just been invited to, and requires every row to PASS - then breaks the one
thing a friend most often gets wrong (the token) and requires that row, and only
that row, to fail.

No quota is spent: the `claude_code` brain row runs `claude --version`, never a
turn. The gate is skipped if the CLI is not installed.

Run with:  AGENT_ROOM_LIVE=1 tests/live/.venv/bin/pytest -q -m live tests/live/test_onboarding.py
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest
from conftest import (
    AGENT_ROOM_BIN,
    LIVE_SKIP,
    MCP_SESSION,
    MCP_SESSION_NAME,
    Tokens,
    make_connector,
)

pytestmark = [pytest.mark.live, LIVE_SKIP]

DOCTOR_TIMEOUT_S = 120


def claude_bin() -> str:
    path = shutil.which("claude")
    if path is None:  # pragma: no cover - the gate is skipped, not failed
        pytest.skip("the claude CLI is not on PATH")
    return path


def run_doctor(config_path: Path) -> tuple[int, str]:
    """The shipped path: the console script, as a person would type it."""
    result = subprocess.run(
        [str(AGENT_ROOM_BIN), "doctor", "--config", str(config_path)],
        capture_output=True,
        text=True,
        timeout=DOCTOR_TIMEOUT_S,
        check=False,
    )
    return result.returncode, result.stdout + result.stderr


def rows(output: str) -> dict[str, str]:
    """`{row name: status}` from the printed table."""
    table: dict[str, str] = {}
    for line in output.splitlines():
        parts = line.split(maxsplit=1)
        if len(parts) == 2 and parts[0] in {"PASS", "FAIL", "SKIP"}:
            name = parts[1].split("  ")[0].strip()
            table[name] = parts[0]
    return table


@pytest.mark.timeout(240)
async def test_d1_doctor_passes_a_real_config_and_fails_a_wrong_token(
    tmp_path: Path, tokens: Tokens, room_session: str
) -> None:
    """D1: every row PASSes; then the token is wrong and exactly that row fails."""
    connector = make_connector(
        tmp_path,
        tokens,
        MCP_SESSION_NAME,
        room_session,
        brain={"kind": "claude_code", "claude_code": {"claude_bin": claude_bin()}},
    )

    code, output = run_doctor(connector.config_path)
    print(f"\n----- doctor, good config -----\n{output}")
    table = rows(output)
    assert code == 0, f"doctor failed on a config that works:\n{output}"
    assert set(table) == {
        "token file",
        "homeserver",
        "token",
        "device",
        f"room {room_session}",
        "brain",
    }
    # No store exists before the first run, so the device row can only SKIP.
    assert table["device"] == "SKIP", table
    assert {name: status for name, status in table.items() if name != "device"} == {
        name: "PASS" for name in table if name != "device"
    }, table
    assert MCP_SESSION in output, "the report names the account it checked"

    token_path = connector.config_path.parent / "access"
    token_path.write_text("syt_this_token_was_never_issued", encoding="utf-8")
    token_path.chmod(0o600)

    code, output = run_doctor(connector.config_path)
    print(f"\n----- doctor, wrong token -----\n{output}")
    table = rows(output)
    assert code == 1, f"a rejected token has to be a failure:\n{output}"
    assert table["token"] == "FAIL"
    assert table["homeserver"] == "PASS", "the server is fine; it is the token that is not"
    assert table["token file"] == "PASS", "0600 is about the file, not about the token"
    assert table[f"room {room_session}"] == "SKIP", "nothing can be said about rooms without auth"
