"""W1/W2: a store that cannot own the token's device.

The gate accounts' devices have published keys from crypto stores that no
longer exist (every live run since R1 used a fresh state dir), so ANY fresh
store on their tokens is the wedge (docs/READINESS.md F4). That makes them the
perfect fixture: W1 proves `run` refuses such a store with exit 3 and one
line, W2 proves `allow_wedged_device: true` runs anyway and keeps the SDK's
one-time-key storm out of the log. The clean control (a store that matches)
is the owner's two production bots, checked at every swap; the accounts here
cannot produce one until the owner issues them new devices.
"""

from __future__ import annotations

import asyncio
import contextlib
import re
import subprocess
from pathlib import Path

import pytest
import yaml
from conftest import (
    LIVE_SKIP,
    S3_BOT_A_NAME,
    Connector,
    Tokens,
    make_connector,
)
from nio import AsyncClient

pytestmark = [pytest.mark.live, LIVE_SKIP]

ANSI = re.compile(r"\x1b\[[0-9;]*m")
CURE = "already published encryption keys from a different state directory"


def _set_allow(bot: Connector, allow: bool) -> None:
    config = yaml.safe_load(bot.config_path.read_text(encoding="utf-8"))
    config["allow_wedged_device"] = allow
    bot.config_path.write_text(yaml.safe_dump(config, sort_keys=False), encoding="utf-8")


def _errors(bot: Connector) -> list[str]:
    return [line for line in ANSI.sub("", bot.log_text()).splitlines() if " ERROR " in line]


@pytest.mark.timeout(120)
async def test_w1_a_fresh_store_on_an_old_token_is_refused_with_exit_3(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3)
    _set_allow(bot, False)
    bot.start()
    running.append(bot)
    assert bot.process is not None
    # Wait for the process to end on its own, up to 60 s, without polling.
    loop = asyncio.get_running_loop()
    with contextlib.suppress(subprocess.TimeoutExpired):
        await loop.run_in_executor(None, bot.process.wait, 60)
    assert bot.process.poll() == 3, f"expected exit 3, got {bot.process.poll()}:\n{bot.log_text()}"
    log = ANSI.sub("", bot.log_text())
    assert CURE in log, log
    assert len(_errors(bot)) <= 3, "one line, not a storm:\n" + "\n".join(_errors(bot))


@pytest.mark.timeout(120)
async def test_w2_allow_wedged_device_runs_and_keeps_the_storm_out_of_the_log(
    tmp_path: Path, tokens: Tokens, human: AsyncClient, room_s3: str, running: list[Connector]
) -> None:
    bot = make_connector(tmp_path, tokens, S3_BOT_A_NAME, room_s3)
    _set_allow(bot, True)
    bot.start()
    running.append(bot)
    bot.wait_ready()
    await asyncio.sleep(20)
    assert bot.process is not None
    assert bot.process.poll() is None, "it must keep running"
    log = ANSI.sub("", bot.log_text())
    assert "allow_wedged_device is set" in log, log
    storm = [
        line for line in log.splitlines() if "One time key" in line and "already exists" in line
    ]
    assert storm == [], "the one-time-key storm must not reach the log:\n" + "\n".join(storm[:5])
