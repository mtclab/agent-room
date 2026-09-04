"""Teeth check: break one guard in `src/`, prove its gate fails.

House rule: a gate that stays green with its guard removed is worthless. This
script applies one surgical mutation at a time to the shipped source, rebuilds
the release binary, runs only the gate that guard protects, records the
outcome, and restores the file with `git checkout` (verified clean afterwards).

Two kinds of gate. G1-G12, N1/N2/N4, C1, C2, M2/M3/M5, D1 and T1 are LIVE journeys: they
need `AGENT_ROOM_LIVE=1`, a homeserver in `~/.config/agent-room/live.env` and
the bot tokens, and C1/C2 additionally spend the owner's Claude quota.
U8-U12 are OFFLINE and are cargo's own tests, so they need nothing but the
toolchain.
C3's teeth are not a mutation here: `AGENT_ROOM_LEAK_TEETH=1` is how that gate
is stripped, and the run is recorded in `docs/GATES.md`.

Usage:  tests/live/.venv/bin/python tests/live/teeth.py [gate ...]  # offline only
        AGENT_ROOM_LIVE=1 tests/live/.venv/bin/python tests/live/teeth.py
"""

from __future__ import annotations

import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
#: The product. Every mutation is applied here and compiled before a gate sees it.
RUST_SRC = REPO / "src"
RUST_BIN = REPO / "target" / "release" / "agent-room"
#: The live gates are pytest's; the runner environment is the live harness's own
#: venv (`make live-env`), never a venv the product needs - it has none.
PYTEST = REPO / "tests" / "live" / ".venv" / "bin" / "pytest"


@dataclass
class Mutation:
    gate: str
    guard: str
    path: Path
    old: str
    new: str
    test: str
    #: pytest marker expression; offline gates run without the LAN.
    marker: str = "live"
    #: Extra environment for the mutant run. The leak probe needs it: removing
    #: the brain's fixed rule only proves anything if the persona is not
    #: quietly holding the same line.
    env: dict[str, str] = field(default_factory=dict)
    #: An OFFLINE gate: the name of a `cargo test --test <binary>` target, with
    #: `test` as the filter. The offline gates are cargo's, not pytest's.
    cargo_test: str = ""
    #: An OFFLINE gate that lives in the crate's own unit tests rather than in
    #: an integration binary: `cargo test --lib`, with `test` as the filter.
    cargo_lib: bool = False

    @property
    def needs_live(self) -> bool:
        return self.marker == "live"


# -- the mutations ----------------------------------------------------------
#
# Each entry breaks exactly one guard in `src/`, rebuilds the release binary and
# runs only the gate that guard protects. G1-G12, C1-C3, M2/M3/M5, D1 and T1 are
# live journeys driven through the rebuilt binary; U8-U12 are cargo's own
# offline gates and need no homeserver.

#: The `Relation` literal `room_post` builds. Kept out of the table because a
#: multi-line Rust literal inside a dataclass argument is unreadable.
#: Guard 3d, as `policy::read_names` writes it: the vocative of another member's
#: name that makes the line theirs. Out of the table because two mutations use
#: it and a multi-line Rust literal inside a dataclass argument is unreadable.
OTHER_NAME_GUARD = """    if let Some((user_id, address)) = cues
        .names
        .addresses_other(&ev.body, cfg.bare_name_addresses)
    {"""

THREAD_RELATION = """            &Relation {
                thread_root,
                reply_to,
                thread_fallback: None,
            },"""

MUTATIONS = [
    Mutation(
        gate="G1",
        guard="policy: the mention branch",
        path=RUST_SRC / "policy.rs",
        old="    let addressed: Option<String> = if cfg.reply_to_mentions "
        "&& ev.mentions.contains(me) {",
        new="    let addressed: Option<String> = if false && cfg.reply_to_mentions "
        "&& ev.mentions.contains(me) {  // TEETH",
        test="tests/live/test_journeys.py::test_g1_a_mention_is_answered_in_thread",
    ),
    Mutation(
        gate="G2",
        guard="policy: the unaddressed guard",
        path=RUST_SRC / "policy.rs",
        old="    let Some(addressed) = addressed else {\n"
        "        return unaddressed(ev, ledger, cfg);\n"
        "    };",
        new="    let addressed = addressed"
        '.unwrap_or_else(|| "unaddressed (TEETH: guard removed)".to_owned());',
        test="tests/live/test_journeys.py::test_g2_an_unaddressed_message_is_left_alone",
    ),
    Mutation(
        gate="G3",
        guard="policy: the per-pair budget",
        path=RUST_SRC / "policy.rs",
        old="        let pair = ledger.pair_allows(&ev.sender, now);\n"
        "        if !pair.allowed {\n"
        "            return Decision::new(Verdict::Silent, pair.reason, false);\n"
        "        }",
        new="        let _pair = ledger.pair_allows(&ev.sender, now);  // TEETH: verdict ignored",
        test="tests/live/test_journeys.py::test_g3_bot_to_bot_traffic_is_capped_by_the_pair_budget",
    ),
    # The mutation is "handle the startup sync like any other", because that is
    # the naive implementation G4 exists to forbid - and because the belts here
    # are not the reference's. The Rust client persists its sync token in the
    # store, so a restart RESUMES: the traffic missed while it was down arrives
    # in the sweep's OWN first sync. Removing the /messages snapshot alone, and
    # even removing the whole recording, therefore changes nothing (both
    # measured PASSED on 2026-09-03) - the sweep consumes those events either
    # way. What has to break for a restart to answer old traffic is the
    # separation between the startup sync and the live path.
    Mutation(
        gate="G4",
        guard="connector: the startup sweep, rather than the live path",
        path=RUST_SRC / "connector" / "mod.rs",
        old="        let mut total = self.record_backlog(&response).await;\n"
        "        for room_id in self.workers.keys() {\n"
        "            total += self.snapshot_room(room_id).await;\n"
        "        }\n"
        "        self.live.store(true, Ordering::SeqCst);",
        new="        let total = 0;  // TEETH: the startup sync is handled as live traffic\n"
        "        self.live.store(true, Ordering::SeqCst);\n"
        "        self.handle_sync(&response).await;",
        test="tests/live/test_journeys.py::"
        "test_g4_a_restart_never_answers_twice_or_answers_the_backlog",
    ),
    # -- R2: the Claude Code brain (live, spends the owner's Claude quota) ---
    Mutation(
        gate="C1",
        guard="claude_code: resume the room's session (live memory across messages)",
        path=RUST_SRC / "brain" / "claude_code.rs",
        old='        argv.push(if resume { "--resume" } else { "--session-id" }.to_owned());',
        new="        let _ = resume;  // TEETH: never resumes\n"
        '        argv.push("--session-id".to_owned());',
        test="tests/live/test_claude_brain.py::"
        "test_c1_the_room_session_remembers_the_previous_message",
    ),
    Mutation(
        gate="C2",
        guard="claude_code: the allowlist is exactly what the config says (live shell refusal)",
        path=RUST_SRC / "brain" / "claude_code.rs",
        old="        if !self.cfg.allowed_tools.is_empty() {\n"
        '            argv.push("--allowedTools".to_owned());\n'
        "            argv.extend(self.cfg.allowed_tools.iter().cloned());\n"
        "        }",
        # Merely DROPPING the flag proves nothing: `claude -p` has nobody to ask,
        # so Bash stays unapproved and the gate passes anyway (measured
        # 2026-09-02 on the reference). The mutation that matters is the
        # allowlist naming a tool the config never asked for.
        new='        argv.push("--allowedTools".to_owned());  // TEETH: pre-approves a shell\n'
        '        argv.push("Bash".to_owned());',
        test="tests/live/test_claude_brain.py::test_c2_the_room_cannot_make_the_agent_run_a_shell",
    ),
    # -- R3: tier 2 and the heartbeat (live) --------------------------------
    Mutation(
        gate="G5",
        guard="connector: the stand-down re-read after the tier-2 back-off",
        path=RUST_SRC / "connector" / "turn.rs",
        old="            if let Some(reason) = "
        "self.stand_down_reason(&worker, &ev, started).await {",
        new="            let _teeth = self.stand_down_reason(&worker, &ev, started).await;\n"
        "            if let Some(reason) = None::<String> {  // TEETH: verdict ignored",
        test="tests/live/test_tier2.py::"
        "test_g5_exactly_one_of_two_bots_answers_an_unaddressed_question",
    ),
    Mutation(
        gate="G6",
        guard="policy: conversation energy gates a bot's mention in a wound-down thread",
        path=RUST_SRC / "policy.rs",
        old="    if ev.is_bot {\n"
        "        let energy = ledger.energy_allows(ev.thread_root_or_self());",
        new="    if false && ev.is_bot {  // TEETH: the thread never winds down\n"
        "        let energy = ledger.energy_allows(ev.thread_root_or_self());",
        test="tests/live/test_tier2.py::"
        "test_g6_a_bot_only_thread_winds_down_and_a_human_revives_it",
    ),
    Mutation(
        gate="G7",
        guard="connector: the tier-2 judge's verdict is what decides",
        path=RUST_SRC / "connector" / "turn.rs",
        old="            if !judgement.speak {",
        new="            if false && !judgement.speak {  // TEETH: verdict ignored",
        test="tests/live/test_tier2.py::"
        "test_g7_an_unaddressed_line_the_judge_declines_is_left_alone",
    ),
    Mutation(
        gate="G8",
        guard="connector: the heartbeat loop starts at all",
        path=RUST_SRC / "connector" / "mod.rs",
        old="            if minutes > 0 {\n"
        "                #[allow(clippy::cast_precision_loss)]\n"
        "                let period_s = minutes as f64 * 60.0;",
        new="            if false && minutes > 0 {  // TEETH: the heartbeat never starts\n"
        "                #[allow(clippy::cast_precision_loss)]\n"
        "                let period_s = minutes as f64 * 60.0;",
        test="tests/live/test_tier2.py::"
        "test_g8_a_heartbeat_speaks_into_a_quiet_room_and_addresses_nobody",
    ),
    # -- Addressing by name (live) ------------------------------------------
    Mutation(
        gate="N1",
        guard="policy: my own name in the body is an address (arm 3c)",
        path=RUST_SRC / "policy.rs",
        old="    if let Some(address) = cues.names.addresses_me(&ev.body, "
        "cfg.bare_name_addresses) {",
        new="    if let Some(address) = cues.names.addresses_me(&ev.body, "
        "cfg.bare_name_addresses).filter(|_| false) {  // TEETH: my own name never counts",
        test="tests/live/test_addressing.py::"
        "test_n1_a_typed_name_is_answered_at_once_and_costs_no_judge",
    ),
    Mutation(
        gate="N2",
        guard="policy: somebody else's name in the body is their turn (arm 3d)",
        path=RUST_SRC / "policy.rs",
        old=OTHER_NAME_GUARD,
        new=OTHER_NAME_GUARD.replace(
            "    {", "        .filter(|_| false)\n    {  // TEETH: not somebody else's turn"
        ),
        test="tests/live/test_addressing.py::"
        "test_n2_a_name_that_is_not_mine_is_somebody_elses_turn",
    ),
    Mutation(
        gate="N4",
        guard="policy: arm 3d again, with both agents in the room",
        path=RUST_SRC / "policy.rs",
        old=OTHER_NAME_GUARD,
        new=OTHER_NAME_GUARD.replace(
            "    {", "        .filter(|_| false)\n    {  // TEETH: not somebody else's turn"
        ),
        test="tests/live/test_addressing.py::test_n4_two_agents_one_name_and_exactly_one_answer",
    ),
    # -- R3: unprompted speech (live) ---------------------------------------
    Mutation(
        gate="G9",
        guard="connector: nothing unprompted is said unless a human is present",
        path=RUST_SRC / "connector" / "unprompted.rs",
        old="        let (present, why) = self.humans_present(worker, last_human_post_ts).await;\n"
        "        if !present {\n"
        "            debug!(",
        new="        let (present, why) = self.humans_present(worker, last_human_post_ts).await;\n"
        "        if false && !present {  // TEETH: speaks into an empty room\n"
        "            debug!(",
        test="tests/live/test_unprompted.py::"
        "test_g9_an_impulse_waits_until_somebody_is_there_to_hear_it",
    ),
    Mutation(
        gate="G10",
        guard="connector: a follow-up never opens a loop of its own",
        path=RUST_SRC / "connector" / "turn.rs",
        old="            if candidate.kind != Occasion::Followup {",
        new="            if true {  // TEETH: a follow-up leaves its own loop behind",
        test="tests/live/test_unprompted.py::"
        "test_g10_a_question_nobody_answers_gets_exactly_one_follow_up",
    ),
    Mutation(
        gate="G11",
        guard="connector: inner thoughts wait for the threshold",
        path=RUST_SRC / "connector" / "unprompted.rs",
        old="    if total < threshold {",
        new="    if false && total < threshold {  // TEETH: speaks on the first message",
        test="tests/live/test_unprompted.py::"
        "test_g11_inner_thoughts_add_up_until_the_agent_speaks_once",
    ),
    Mutation(
        gate="G12",
        guard="openai_compat: the warm-up cooldown",
        path=RUST_SRC / "brain" / "openai_compat.rs",
        old="        if now < until {",
        new="        if false && now < until {  // TEETH: warms on every notice",
        test="tests/live/test_unprompted.py::test_g12_a_typing_human_warms_an_on_demand_model_once",
    ),
    # -- R4: the MCP server, init and doctor ---------------------------------
    Mutation(
        gate="M2",
        guard="mcp_server: thread_root never reaches build_reply_content",
        path=RUST_SRC / "mcp_server.rs",
        old=THREAD_RELATION,
        new=THREAD_RELATION.replace("thread_root,", "thread_root: None,  // TEETH"),
        test=(
            "tests/live/test_mcp.py::"
            "test_m2_the_session_answers_in_the_thread_and_mentions_the_human"
        ),
    ),
    Mutation(
        gate="M3",
        guard="mcp_server: the wait never waits (the timeout is forced to 0)",
        path=RUST_SRC / "mcp_server.rs",
        old="        let timeout_s = timeout_s.clamp(0.0, MAX_WAIT_S);",
        new="        let timeout_s = 0.0 * timeout_s.clamp(0.0, MAX_WAIT_S);  // TEETH",
        test=("tests/live/test_mcp.py::test_m3_the_session_waits_and_wakes_when_somebody_speaks"),
    ),
    Mutation(
        gate="M5",
        guard="mcp_server: the budget refusal is ignored",
        path=RUST_SRC / "mcp_server.rs",
        old="        match self.budget_refusal(room_id) {",
        new="        match None::<String> {  // TEETH",
        test="tests/live/test_mcp.py::test_m5_the_budget_stops_a_session_flooding_the_room",
    ),
    Mutation(
        gate="D1",
        guard="doctor: the token verdict forced to PASS",
        path=RUST_SRC / "doctor.rs",
        old="        match authenticate(api, self.cfg).await {",
        new="        match Ok::<String, CsError>(self.cfg.user_id.clone()) {  // TEETH",
        test=(
            "tests/live/test_onboarding.py::"
            "test_d1_doctor_passes_a_real_config_and_fails_a_wrong_token"
        ),
    ),
    Mutation(
        gate="U8",
        guard="doctor: the 0600 rule in the permission row",
        path=RUST_SRC / "doctor.rs",
        old="    match require_private_mode(path, name) {",
        new="    match Ok::<(), crate::config::ConfigError>(()) {  // TEETH",
        test="doctor::a_token_file_anybody_can_read_fails_the_permission_row",
        marker="offline",
        cargo_test="r4_commands",
    ),
    Mutation(
        gate="U9",
        guard="init: the overwrite refusal",
        path=RUST_SRC / "init_cmd.rs",
        old="    if !existing.is_empty() && !args.force {",
        new="    if false && !existing.is_empty() && !args.force {  // TEETH",
        test="init::init_refuses_to_overwrite_what_is_already_there",
        marker="offline",
        cargo_test="r4_commands",
    ),
    # -- #13: the transcript is capped and rolled -----------------------------
    Mutation(
        gate="T1",
        guard="transcript: the live file rolls at all (live, under a real connector)",
        path=RUST_SRC / "transcript.rs",
        old="        if self.keep == 0 {\n            return Ok(());\n        }",
        new="        if true {  // TEETH: rotation off\n            return Ok(());\n        }",
        test="tests/live/test_rotation.py::"
        "test_t1_a_rolling_transcript_stays_bounded_and_the_agent_keeps_up",
    ),
    Mutation(
        gate="U11",
        guard="transcript: the roll itself, when the cap is crossed",
        path=RUST_SRC / "transcript.rs",
        old="        *held = Some(self.rotate(count)?);",
        new="        *held = Some(count);  // TEETH: counted, never rolled",
        test="transcript::tests::a_transcript_over_the_cap_rolls_exactly_once",
        marker="offline",
        cargo_lib=True,
    ),
    Mutation(
        gate="U12",
        guard="transcript: the new live file is seeded with the newest half",
        path=RUST_SRC / "transcript.rs",
        old="        let seed = self.tail_lines(self.keep / 2);",
        new="        let seed: Vec<String> = Vec::new();  // TEETH: nothing carried over",
        test="transcript::tests::the_new_live_file_holds_the_newest_half_and_recent_reads_it_back",
        marker="offline",
        cargo_lib=True,
    ),
    Mutation(
        gate="U10",
        guard="doctor: more than one sync when hunting for an invitation",
        path=RUST_SRC / "doctor.rs",
        old="pub const SYNC_ATTEMPTS: usize = 3;",
        new="pub const SYNC_ATTEMPTS: usize = 1;  // TEETH",
        test="doctor::an_invitation_the_first_sync_missed_is_still_found",
        marker="offline",
        cargo_test="r4_commands",
    ),
]


def git_clean(path: str = "src") -> bool:
    result = subprocess.run(["git", "diff", "--quiet", "--", path], cwd=REPO, check=False)
    return result.returncode == 0


def cargo_build() -> None:
    """Compile the mutant. A mutation that does not compile is not a gate."""
    result = subprocess.run(
        ["cargo", "build", "--release"], cwd=REPO, capture_output=True, text=True, check=False
    )
    if result.returncode:
        raise SystemExit(f"the mutant does not compile:\n{result.stderr[-3000:]}")


def mutant_env(mutation: Mutation) -> dict[str, str]:
    """The gate runs against the REBUILT binary, never a stale one."""
    env = {**os.environ, "AGENT_ROOM_BIN": str(RUST_BIN)}
    if mutation.needs_live:
        env["AGENT_ROOM_LIVE"] = "1"
    env.update(mutation.env)
    return env


def run(mutation: Mutation) -> tuple[str, float]:
    source = mutation.path.read_text(encoding="utf-8")
    if mutation.old not in source:
        raise SystemExit(f"{mutation.gate}: guard text not found in {mutation.path}")
    mutation.path.write_text(source.replace(mutation.old, mutation.new), encoding="utf-8")
    cargo_build()
    started = time.monotonic()
    if mutation.cargo_lib:
        command = ["cargo", "test", "--lib", "--", mutation.test]
    elif mutation.cargo_test:
        command = ["cargo", "test", "--test", mutation.cargo_test, "--", mutation.test]
    else:
        command = [str(PYTEST), "-q", "-m", mutation.marker, mutation.test]
    cwd = REPO
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=mutant_env(mutation),
            capture_output=True,
            text=True,
            check=False,
        )
    finally:
        subprocess.run(["git", "checkout", "--", str(mutation.path)], cwd=REPO, check=True)
        cargo_build()
    elapsed = time.monotonic() - started
    verdict = "FAILED (good: the gate has teeth)" if result.returncode else "PASSED (BAD)"
    tail = "\n".join(result.stdout.strip().splitlines()[-6:])
    print(f"\n=== {mutation.gate}: {mutation.guard} -> {verdict} in {elapsed:.0f} s ===")
    print(tail)
    if not git_clean():
        raise SystemExit(f"{mutation.gate}: source not restored, stopping")
    return verdict, elapsed


def main(argv: list[str]) -> int:
    if not git_clean():
        raise SystemExit("src/ is dirty; commit or stash before the teeth run")
    live_ok = os.environ.get("AGENT_ROOM_LIVE") == "1"
    wanted = {a.upper() for a in argv} or {m.gate for m in MUTATIONS}
    rows = []
    for mutation in MUTATIONS:
        if mutation.gate not in wanted:
            continue
        if mutation.needs_live and not live_ok:
            print(f"\n=== {mutation.gate}: skipped (needs AGENT_ROOM_LIVE=1) ===")
            continue
        verdict, elapsed = run(mutation)
        rows.append((mutation.gate, mutation.guard, verdict, elapsed))
    print("\n=== teeth summary ===")
    for gate, guard, verdict, elapsed in rows:
        print(f"{gate:3}  {guard:70}  {verdict}  {elapsed:.0f} s")
    return 0 if all("FAILED" in row[2] for row in rows) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
