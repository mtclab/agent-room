# agent-room. The product is one static Rust binary; everything below is either
# the gate that proves it works or the release that ships it.
#
# `tests/live/.venv` is the LIVE HARNESS's own Python: pytest, a Matrix client
# to play the human with and an MCP client to drive `agent-room mcp`. Nothing
# the product needs is in it - see tests/live/README.md.
LIVE_VENV_DIR=tests/live/.venv
LIVE_VENV=$(LIVE_VENV_DIR)/bin
.PHONY: gate fmt lint test build release live-env lint-live live live-mcp live-claude live-e2ee

# The gate: fmt, clippy (pedantic, warnings are errors), unit tests.
gate:
	cargo fmt --check && cargo clippy --all-targets -- -D warnings -W clippy::pedantic && cargo test

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets -- -D warnings -W clippy::pedantic

test:
	cargo test

# The binary the live gates drive.
build:
	cargo build --release

# Static musl builds + tarballs + SHA256SUMS in dist/. Needs zig and
# cargo-zigbuild; scripts/release.sh says how to get them.
release:
	./scripts/release.sh

# The live harness's Python. Never the product's - it has none.
live-env:
	python3 -m venv $(LIVE_VENV_DIR) && \
		$(LIVE_VENV)/pip install --quiet --upgrade pip && \
		$(LIVE_VENV)/pip install --quiet -r tests/live/requirements.txt

lint-live:
	$(LIVE_VENV)/ruff check tests && $(LIVE_VENV)/ruff format --check tests

# The live journeys, against whatever AGENT_ROOM_BIN names (default:
# target/release/agent-room). G1-G12 is everything the connector does, N1-N4
# addressing by name, C-1/C-2/C-3 two agents having a conversation, T1 the
# transcript rolling under it; M1-M5 the MCP server and D1 the doctor. The
# Claude gates spend the owner's quota and have a target of their own.
live:
	AGENT_ROOM_LIVE=1 $(LIVE_VENV)/pytest -q tests/live/test_journeys.py \
		tests/live/test_tier2.py tests/live/test_unprompted.py \
		tests/live/test_addressing.py tests/live/test_conversation.py \
		tests/live/test_rotation.py \
		tests/live/test_mcp.py tests/live/test_onboarding.py

# The two R4 commands on their own: a live session in a room, and doctor.
live-mcp:
	AGENT_ROOM_LIVE=1 $(LIVE_VENV)/pytest -q tests/live/test_mcp.py \
		tests/live/test_onboarding.py

# The Claude Code brain against the real CLI: C1-C3, about $0.10 of haiku.
live-claude:
	AGENT_ROOM_LIVE=1 $(LIVE_VENV)/pytest -q -m claude \
		tests/live/test_claude_brain.py tests/live/test_leak_probe.py

# The encrypted-room gate and its negative control. Rust, because the Python
# harness has no crypto store to decrypt with.
live-e2ee:
	AGENT_ROOM_LIVE=1 cargo test --test e1_encrypted -- --nocapture
