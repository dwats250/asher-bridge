# Getting started

How to run the project's checks and a small attended demonstration on throwaway data.

**How this page was produced.** Its steps were derived from the source and the tests. **Not all of them have been run by hand, and none was re-executed while this page was written.** Each step says where it was verified. Output fields may differ from what you see; look for the states and reasons named here, not for byte-identical output.

**What this is and is not.** Asher Bridge is an engineering-validated prototype. There is no installer, no service and no unattended mode. The only worker it can run is a fixed, deterministic stand-in program (`fake-agent`); no real AI agent is involved. Use only freshly created, disposable state directories.

## 1. What you need

The build is deliberately strict: it runs offline, with one pinned compiler, inside a sandbox. Full detail is in [TOOLCHAIN.md](TOOLCHAIN.md).

- Linux on x86-64, with unprivileged user namespaces.
- `bwrap` (bubblewrap; tested with 0.12.0), `bash`, `python3`, a C compiler and linker, `git`, `strings`.
- At least 10 GB free on `/var/tmp`.
- The pinned Rust toolchain, 1.99.1 (rustc, cargo, rustfmt, clippy, rustdoc), as an unpacked directory tree, plus a pre-seeded copy of the dependency registry. Both are located through one variable, `AB_RUST_ROOT`, which must contain `rustup/toolchains/stable-x86_64-unknown-linux-gnu/` and `cargo/registry/`. The scripts check the toolchain against recorded SHA-256 digests and refuse anything else.

Two things to know before trying:

- **`AB_RUST_ROOT` is required.** There is no built-in default; the scripts stop if it is unset. Set it in your environment for every command in section 2.
- **Obtaining the pinned Rust 1.99.1 toolchain and the dependency registry is your responsibility.** The repository ships neither and the scripts download nothing. Whether an installation of that version on another machine matches the recorded digests is unverified; if it does not, the scripts refuse to run. The verification result reported in [STATUS.md](STATUS.md) was obtained on the development host only.

There is no rustup, no network access and no "quick" unsandboxed mode. The scripts also refuse to run if your shell exports variables that could steer a build (`RUST*`, `CARGO*`, compiler and linker variables, `LD_*` and similar).

## 2. Run the checks

From a completely clean Git checkout (no uncommitted, untracked or ignored files) whose code matches the content digest pinned in `scripts/ab-env.sh`:

```sh
export AB_RUST_ROOT=<directory holding the toolchain and registry>
AB_ID=<new-id> scripts/ab-verify.sh
```

`<new-id>` is a label you choose: lowercase letters, digits or `-`, at most 6 characters, and never used before. The script creates a fresh build root at `/var/tmp/ab-<new-id>/` and will not reuse an existing one, so a second run needs a different id.

It runs, inside the sandbox, the five required checks:

```sh
cargo test --offline --locked --features fault-injection
cargo test --offline --locked --features fault-injection -- --test-threads=1
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo fmt --check
cargo build --offline --release --locked
```

plus a dependency proof, a sandbox boundary probe and checks on the release binaries. It exits 0 only if everything passes, and it expects exactly **243 tests passed, 0 failed, 0 ignored** in each of the two test runs. Do not run the `cargo` lines directly with a toolchain from your `PATH`; that is not the supported path.

A separate, build-free check of the environment itself:

```sh
AB_ID=<another-new-id> scripts/ab-selftest.sh
```

The selftest exercises the environment checks themselves (it expects them to refuse bad input) and does not depend on any build root that existed beforehand. Like the verification, it needs `AB_RUST_ROOT`, and its behaviour on a machine other than the development host is unverified.

Verified in: [TOOLCHAIN.md](TOOLCHAIN.md) ("Use"), `scripts/ab-verify.sh`, `scripts/ab-env.sh`, `scripts/ab-selftest.sh`. The 243-test result was obtained on the development host and has not been independently reproduced.

Two practical consequences:

- A checkout with uncommitted edits, including documentation edits, fails the clean-tree check until they are committed.
- The release binaries are written to `/var/tmp/ab-<id>/target/release/`, not into the repository. The repository contains no prebuilt binaries; they come only from the build.

## 3. A small attended demonstration

This uses three programs from the release build, which must sit side by side: `orchestrator` (the command you type), `supervisor` and `fake-agent` (started for you; never run them by hand).

```sh
BIN=/var/tmp/ab-<id>/target/release        # from a passed ab-verify.sh run
demo_parent=$(mktemp -d)                   # throwaway; no symlinks in its path
S="$demo_parent/state"
```

A zero exit status means the command was handled, not that work succeeded. Always read the `state` and `reason` in the output.

**Do not run `sweep` during this demonstration.** With no flag it sends a real desktop notification.

### Step 1. Create a ledger, a permit, and accept one piece of synthetic work

```sh
$BIN/orchestrator init "$S"
$BIN/orchestrator permit "$S" permit-1 fixture:demo
printf '%s\n' '{"protocol_version":1,"request_key":"demo-symbol","fixture_id":"fixture:demo","success_criteria":"Synthetic fake execution only; no successful-work claim","source_evidence":["fixture:initial-failure"],"permit_id":"permit-1"}' | $BIN/orchestrator submit "$S"
```

Look for `"disposition":"accepted"` and a `next_action` of `prepare_fixture`. Sending the identical `submit` line again returns `"disposition":"existing"`: the same request is recognised, not accepted twice. A permit made this way allows one preparation attempt and expires after one hour.

Verified in: `src/bin/orchestrator.rs` (`init`, `permit`, `submit`); [OPERATIONS.md](OPERATIONS.md) ("Disposable demo"); `tests/acceptance.rs` (`equivalent_json_order_replays_same_identity`).

### Step 2. Run the stand-in worker under the supervisor

```sh
$BIN/orchestrator run-fake "$S" demo-symbol attempt-1 complete 1000
```

Look for state `cleaned` with a reason beginning `fake cleanup confirmed; work outcome NOT validated`. "Cleaned" means the worker was observed to stop and be cleaned up. It says nothing about whether any work was good.

### Step 3. Repeat the command: nothing runs twice

Run the identical `run-fake` line again. You get the same recorded attempt back; no second worker is started. Changing the mode or runtime under the same attempt id is refused, and so is a new attempt id for the same work.

Verified in: `tests/lifecycle.rs` (`duplicate_commands`); `src/execution.rs`.

### Step 4. Produce a result that cannot be established, and see it kept as "unknown"

The `escape` mode makes the stand-in start a child process that deliberately leaves the supervised group, so complete cleanup cannot be confirmed.

```sh
$BIN/orchestrator permit "$S" permit-2 fixture:demo
printf '%s\n' '{"protocol_version":1,"request_key":"escape","fixture_id":"fixture:demo","success_criteria":"Synthetic fake execution only; no successful-work claim","source_evidence":["fixture:initial-failure"],"permit_id":"permit-2"}' | $BIN/orchestrator submit "$S"
$BIN/orchestrator run-fake "$S" escape attempt-2 escape 1000
$BIN/orchestrator attempts "$S"
$BIN/orchestrator list "$S"
$BIN/orchestrator check "$S"
```

Look for state `unknown` with a reason beginning `unresolved supervision or escaped descendant`. In `list`, that work shows `next_owner` `local-owner` and `next_action` `inspect_supervision_uncertainty`: the uncertainty has been handed to a named person, not resolved by guessing. `check` still reports that the accounting invariants hold.

Verified in: `tests/lifecycle.rs` (`escaped_descendant`); `src/execution.rs`. This page issues a second permit for the second piece of work; whether one permit could cover both has not been checked (unverified). Each command used is verified individually in source.

### Step 5. Restart and reconcile

Every command that can change the ledger is itself a restart: it re-examines unfinished attempts before doing anything else. To do only that:

```sh
$BIN/orchestrator reconcile "$S"
$BIN/orchestrator attempts "$S"
$BIN/orchestrator list "$S"
```

The `unknown` attempt stays `unknown`. Nothing is relaunched and its deadline is not pushed back.

Verified in: [OPERATIONS.md](OPERATIONS.md) ("Restart and unresolved obligations"); `tests/lifecycle.rs` (`unchanged_unknown_deadline_is_not_postponed`).

### Step 6. Diagnose (read-only)

```sh
$BIN/orchestrator diagnose "$S"
```

This always exits 0 and prints a JSON report with a `status`. It writes nothing. The tests show a fresh ledger holding one accepted piece of work reports `healthy` at schema 8. What status this demonstration's ledger reports, with an `unknown` attempt in it, has not been recorded by hand: do not expect `healthy`.

Verified in: `src/bin/orchestrator.rs`; [OPERATIONS.md](OPERATIONS.md) (Gate 6 M1 "Commands"); `tests/recovery.rs` (`diagnostics_distinguish_unreadable_unavailable_degraded_and_healthy`). **This and the next two steps have not been run by hand**; they are covered by the automated tests named.

### Step 7. Back up

```sh
$BIN/orchestrator backup "$S" "$demo_parent/backup-1"
```

The destination must not already exist and must not be inside a ledger directory. The output includes `"source_content_written": false`. The backup directory holds `snapshot.db` and `backup-manifest.json`, and deliberately no `ledger.db`, so it can never be mistaken for a live state directory.

Verified in: `src/restore.rs`; `tests/recovery.rs` (`backup_never_writes_the_source_ledger`).

### Step 8. Restore for inspection only

Note the argument order: the backup directory comes first.

```sh
$BIN/orchestrator restore "$demo_parent/backup-1" "$demo_parent/restored"
$BIN/orchestrator list "$demo_parent/restored"
$BIN/orchestrator reconcile "$demo_parent/restored"
```

The restore output includes `"replay_authorized": false` and `"resume_authorized": false` and lists the open cases the backup cannot vouch for. `list` works on the restored copy. `reconcile`, like every ordinary command that would change it, is refused with exit status 6 and `restored_inspect_only`. (The one exception is `recover`, which records an accounting pass and changes no domain row.) A restored ledger is something to read, not something to resume: the outside world may have moved on since the backup was taken.

Verified in: `src/restore.rs`, `src/recovery.rs`; [OPERATIONS.md](OPERATIONS.md) ("Ledgers derived from a backup are inspect-only"); `tests/recovery.rs` (`restore_only_creates_new_separate_verified_targets`).

### Optional. Review, approval, one action, and no second dispatch

The longer path (freeze the stand-in's output, validate it, create an exact review, sign an approval, perform the one fixture action, then try to dispatch it again) is written out command by command in [OPERATIONS.md](OPERATIONS.md), under "Task 3 operations" and "Task 4 operations" ("Disposable demo" in each). Use `$BIN/orchestrator` wherever those sections name the `orchestrator` binary. A second `dispatch` of the same effect returns `dispatched: false`: a consumed approval is never dispatched again.

Verified in source and in `tests/review.rs`. Closing work afterwards (`closure-summary`, `command-template ... close`, `command`) is described in the Gate 5 part of [OPERATIONS.md](OPERATIONS.md); the exact hand command lines for a close are verified in source only.

### Clean up

Remove only the directory you created:

```sh
rm -rf "$demo_parent"
```

Leave `/var/tmp/ab-<id>/` alone unless you created it and no longer need its results.

## 4. What you can see only through the test suite

The demonstration above shows uncertainty being preserved and repeated commands not repeating work. It does not show the system being killed at the worst possible moment, because ordinary builds contain no way to pause there. The following are exercised only by the automated tests, in a build with the test-only `fault-injection` feature:

| Behaviour | Where it is tested |
|---|---|
| Coordinator killed while a worker is running, then reconciled to `cleaned` with exactly one attempt | `tests/lifecycle.rs`: `coordinator_restart` |
| Supervisor killed: the attempt becomes `unknown`, nothing is relaunched | `tests/lifecycle.rs`: `supervisor_restart`, `both_roles_restart` |
| **The central story:** the one consequential action is paused just after it takes effect, the program is killed, and on restart the action is accounted for exactly once and never repeated | `tests/review.rs`: `scenario_ambiguous_ref_effect`, `scenario_ambiguous_ref_conflict` (scripts in `tests/scenarios/`), `ambiguous_ref_effect`, `ambiguous_ref_conflict` |
| A commit whose outcome is unknown after an injected storage error; `recover` settling it; an action run at most once after an invisible claim | `tests/recovery.rs`: the `t1_*` to `t7_*` and `s1_*`, `s2_*` tests, `a_dispatch_retried_after_an_invisible_claim_runs_the_action_at_most_once` |
| Repeated failed startups stopped by the startup breaker; refused ledgers left byte for byte unchanged; an interrupted restore | `tests/recovery.rs`: `interrupted_runs_never_claim_completion_and_the_breaker_bounds_startup` and neighbours |
| Simulated host reboot and PID reuse | `tests/restart_time.rs`, `tests/pid_reuse.rs` |
| Simulated clock rollback, unreadable clock and suspend | `tests/restart_time.rs` (`clock_and_budget_*`, `r1_*`) |
| Missing, truncated, torn or same-length-altered captured output | `tests/evidence_loss.rs` |

"Simulated" means the tests substitute what the operating system reports (boot id, process table entries, clock offsets) or inject errors inside SQLite. No real reboot, suspend, clock change, power loss or kernel-level storage failure was performed.

`readback` and `recover` can be typed by hand, but on a ledger that has never suffered an injected fault there is nothing uncertain for them to settle.

Killing the program by hand during a run (for example while `run-fake ... hold` is waiting) uses only real commands, but it has not been recorded as run and no test covers it without the test-only pause points. It is not documented here as demonstrated.

## 5. Where to go next

- [OPERATIONS.md](OPERATIONS.md): the full operator reference, every command and what each result means.
- [STATUS.md](STATUS.md): what is implemented, how it is verified, and the limitations.
- [WHY-ASHER-BRIDGE.md](WHY-ASHER-BRIDGE.md): the problem and the reasons.
- [README.md](README.md) in this directory: the map of all documentation.
