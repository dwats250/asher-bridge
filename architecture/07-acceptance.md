# Executable acceptance-test specification

This is a plan for a future executable suite, not test results. Product implementation and the harness are intentionally not created here. The implementer should implement the interfaces below in the isolated prototype, then run these exact commands. A separate reviewer should run them from a clean checkout and inspect the evidence bundles. Do not substitute screenshots or happy-path unit tests for the failure suite.

## Harness contract

Proposed isolated implementation root: a dedicated prototype directory, separate from this design package (does not exist as part of this delivery).

Future executable targets: `orchestrator`, `supervisor`, `fake-agent`, `fixture-validator`, `attention-sink`; integration suite `acceptance`. Build commands, run from that future root:

```sh
cargo test --locked --lib
cargo test --locked --test acceptance -- --test-threads=1
cargo test --locked --test acceptance ambiguous_ref_effect -- --exact --nocapture
cargo test --locked --test acceptance generated_transition_sequences -- --exact
```

Expected: every named case below passes, no ignored required cases; exit status zero; report includes test name, seed, fault barrier, database snapshot, redacted events, process census, fixture ref history and notification sink receipts. Missing prerequisites produce a clear failed capability gate, not a skipped-pass release. Unit tests do not require a desktop session; one manual local-notification smoke test below is required for the chosen live channel.

Each test creates a fresh `tempdir`, an empty HOME/config environment, a new no-remote Git repository, and isolated runtime/storage/output directories. It must assert the fixture root is beneath the harness-created temp directory, reject symlink roots, and refuse all paths to existing projects. Disable inherited Git configuration and hooks; create a local empty hooks directory. Never use real credentials or network. Harness cleanup is restricted to process identities it created; never kill by a reused PID or broad executable name.

Fixture files:

```text
symbols.csv                  # symbol,scope ; BAD SYMBOL,core
captures/2026-01-01.json      # explicitly incomplete simulated capture
expected.json                # symbol must equal FIXTURE_OK; capture interval expectations
```

Validator is trusted test code with explicit symbol/coverage assertions, not repository-provided scripts. Fake agent rewrites only the fixture candidate to `FIXTURE_OK`, or follows one scripted fault mode. It can emit a false success claim, exit nonzero, delay, fork a child, have a child call setsid, ignore interrupt, fill its bounded output stream or pause at a named barrier. It never invokes a real agent.

Use a virtual clock for expiry, heartbeat and retry tests. Use bounded real monotonic deadlines for actual subprocess termination. A test exceeding its deadline fails and writes a process census before cleanup. Do not use arbitrary sleeps to create race windows: every component exposes test-only named barriers over a harness-only control descriptor, unavailable in production builds.

Required barriers: `before_accept_commit`, `after_accept_commit`, `after_launch_intent`, `after_supervisor_accept`, `after_wrapper_create`, `before_release`, `after_release`, `after_parent_exit`, `after_review_commit`, `before_effect_claim`, `after_effect_claim`, `after_ref_cas`, `before_result_commit`, `after_notify_send`, `before_delivery_commit`. Tests kill the selected component only after receiving the barrier signal, then restart with the same database/runtime identities as appropriate.

Injected SQLite failures must distinguish known rollback from unknown commit result. Use a test-only storage wrapper/VFS for deterministic BUSY/FULL/IOERR/corruption results; add a disposable bounded-filesystem test in the later platform gate. Never exhaust the user's actual disk. SIGKILL/restart tests exercise real process crashes, but do not claim to simulate all power-loss/storage-controller behaviors.

## Executable scenario format

`tests/scenarios/*.json` is data interpreted by the scenario runner (implemented as `run_scenario` in `tests/review.rs`). Unknown operation/field fails parsing. Each step waits for its precise guard/event with a deadline, then performs exactly one action. Test code must invoke public APIs for setup/actions; direct SQL is for invariant inspection or explicit corruption tests only.

Example scenario to be saved by the implementer as `tests/scenarios/ambiguous_ref_effect.json`:

```json
{
  "name": "ambiguous_ref_effect",
  "seed": 7,
  "steps": [
    {"op": "fixture_create", "initial_symbol": "BAD SYMBOL"},
    {"op": "start", "roles": ["coordinator", "supervisor", "attention_sink"]},
    {"op": "submit", "key": "check-symbol-1", "permit": "prepare-fixture"},
    {"op": "wait_phase", "phase": "reviewing", "timeout_ms": 5000},
    {"op": "arm_barrier", "name": "after_ref_cas", "role": "coordinator"},
    {"op": "decide", "choice": "approve", "envelope": "current", "id": "decision-1"},
    {"op": "wait_barrier", "name": "after_ref_cas", "timeout_ms": 5000},
    {"op": "kill", "role": "coordinator", "signal": "KILL"},
    {"op": "start", "roles": ["coordinator"]},
    {"op": "wait_mode", "mode": "closed", "timeout_ms": 5000},
    {"op": "assert", "oracle": "supported_success"},
    {"op": "assert", "oracle": "ref_mutation_count", "equals": 1},
    {"op": "assert", "oracle": "effect_count", "equals": 1},
    {"op": "assert", "oracle": "all_obligations_accounted"}
  ]
}
```

This test's one mutation is an assertion under exclusive fixture ownership and CAS, not an exactly-once guarantee for arbitrary integrations. A companion case changes the ref to a conflicting third value during the crash: it must remain under attention and perform no second CAS.

## Common executable oracles

Implement `assert_all_obligations_accounted()` after every scenario step, after each committed event and after restart. It checks structural accounting with the query below, then cross-row guards in the state specification. Any returned row fails the test:

```sql
SELECT w.id FROM work w
LEFT JOIN attention a ON a.id=w.attention_id
WHERE
  (w.mode='active' AND
    (w.next_owner IS NULL OR w.next_action IS NULL OR w.due_at IS NULL))
  OR (w.mode='attention' AND
    (a.id IS NULL OR a.work_id<>w.id OR a.state='resolved' OR w.due_at IS NULL))
  OR (w.mode='closed' AND
    (w.outcome IS NULL OR w.closure_evidence_id IS NULL OR w.closed_at IS NULL));
```

Additional oracles are mandatory: overdue work creates attention by the next sweep; no success with a gap in required evidence; every effect binds one valid decision/envelope; every live attempt has a work obligation; terminal success has no unaccounted processes/effects; all output stays beneath temp roots; unsupported caller roles cannot mutate authority; at most one live attempt per work; no automatic launch when an older attempt is unknown; all test-created processes are either confirmed gone or explicitly represented by an unresolved cleanup obligation before harness teardown.

## Required test cases

Each name is a separate Rust `#[test]` in `tests/acceptance.rs`; all run under the aggregate command above.

| Test name | Arrange and injection | Pass oracle |
|---|---|---|
| `happy_path_exact_review` | Submit defect; prepare; validate; deliver; approve exact review; apply fixture CAS | Initial failure + candidate pass + result readback; one supported success; receipt alone never closes work |
| `missed_capture_scope` | Missing interval in fixture; fake repairs code but no capture evidence | Cannot claim capture restored; review limited to repair; missing coverage remains explicit |
| `acceptance_crash_boundaries` | Kill before and after T1 commit; replay same submission | Before: absent or later accepted once; after: same work ID; zero launches before acceptance |
| `launch_intent_replay` | Kill after intent, before supervisor send | Same attempt/command recovers; no duplicate process |
| `child_before_ack` | Kill supervisor after wrapper creation and after release, before durable started receipt | Never blind relaunch; reconcile known scope or attention with unknown attempt |
| `partial_start_cleanup` | Fail output reader, writer and worker setup separately after creation | Workload not released; cleanup confirmed or urgent unresolved obligation |
| `coordinator_restart` | Kill coordinator while fake runs | Supervisor deadline survives; recovered work/evidence same identity; no false completion |
| `supervisor_restart` | Kill supervisor while running | Lost supervision visible; wrapper shutdown/census checked; unknown where proof unavailable |
| `both_roles_restart` | Kill both after release; restart both | Every work/command recovered; unknown exit not fabricated; no second attempt |
| `parent_exit_descendants` | Leader exits zero while child remains | Exit observation does not close work; child cleanup required |
| `escaped_descendant` | Fake child calls setsid | Group backend cannot assert whole-descendant stop; urgent attention + unknown scope; harness later removes its known child |
| `ignored_interrupt` | Fake ignores graceful stop | Force stop by deadline; actual census; explicit failure if unable |
| `zero_exit_missing_evidence` | Fake claims pass/exits zero; suppress validation evidence | No success; bounded recovery then attention |
| `evidence_freshness_and_gaps` | Wrong artifact, old epoch, missing sequence, stale heartbeat and disconnected adapter | Each rejected/degraded according to type; no synthetic complete coverage |
| `duplicate_commands` | Concurrent same-ID submissions/launches with same and changed payloads | Same returns prior identity; changed conflicts; one process and one work |
| `decision_race` | Two approve/reject requests race; replay winning decision | One effective decision and at most one effect; losing request conflict; replay returns prior receipt |
| `approval_staleness` | Expire/revoke review; alter snapshot, ref, permit or policy before dispatch | Zero authorized effect from stale envelope; new attention/review required |
| `revocation_dispatch_race` | Revoke before claim and just after claim | Before blocks effect; after reports too-late ambiguity honestly and reconciles |
| `ambiguous_ref_effect` | CAS succeeds; kill before result commit | Readback confirms exact result; no second mutation; supported closure |
| `ambiguous_ref_conflict` | Same crash; external test writer sets third ref value | Attention, zero automatic retry/rollback; changed target preserved |
| `effect_not_applied` | Crash after claim before CAS; prove executor gone and ref unchanged under exclusive fixture lock | Record not applied; old approval not reset; fresh review needed |
| `notification_failures` | Sink offline, lost receipt, duplicate send, owner offline | Durable attention; bounded retry/escalation; duplicate same ID allowed; no automatic acknowledgment |
| `ack_is_not_decision` | Acknowledge/snooze review attention | Review still pending; expiry still enforced; no effect |
| `sqlite_busy_full_ioerr` | Inject errors at T1/T4/T5/T6/T7/S1/S2 separately | No false durable ACK; no uncommitted launch; safety stop independent of DB; stable-ID readback/recovery |
| `database_corruption_restore` | Corrupt disposable copy, restore older backup with later effect on fixture | Refuse normal writes; preserve evidence; no repeat of forgotten effect; reconcile first |
| `artifact_loss` | Remove/corrupt sealed snapshot after review | Review invalidated; no effect; missing artifact attention |
| `stale_owner_and_forgery` | Start second owner; send old epoch and unauthorized role commands | Lock/epoch/role rejection; no effect; audit exists |
| `clock_and_budget` | Roll clock back, suspend simulated time, exceed runtime/output limit | No authority extension; enforce actual bounds; truncation gap; cleanup accounted |
| `closure_and_cancel` | Reject; cancel before/after effect; request abandonment of unknown child | Non-success outcomes labeled; effect retained; linked cleanup prevents silent live hazard |
| `shutdown_and_migration` | Drain/stop with review pending; restart; present unsupported schema | Pending attention survives; unsupported schema refuses mutations |
| `obligation_sweep` | Stop a worker from progressing while coordinator remains alive | Deadline missed → durable attention within one sweep; process heartbeat not mistaken for progress |
| `generated_transition_sequences` | Seeded model-based sequences of submit/replay/expire/revoke/crash/observe/decide | Invariants hold at each commit; counterexample seed/trace saved; at least 1,000 sequences |

## Attention smoke test, after implementation only

On an attended disposable run, use the proposed command:

```sh
cargo run --locked --bin orchestrator -- demo --fixture-only --attention local
```

Expected: visible notification with attention ID and reason; inbox displays exact review; closing the notification does not acknowledge or decide; with desktop delivery disabled, the durable inbox persists and shows delivery failure/escalation. Approval changes only the fixture ref, then an outcome notice appears. Reviewer checks actual local channel behavior because a fake sink cannot establish desktop delivery semantics.

## Later platform and real-adapter tests

Before adding one real adapter, repeat the same suite with an enforced containment backend and a deliberately escaping descendant; now emptiness of the whole owned scope must be established. Verify filesystem/network restrictions by attempted forbidden writes/access, not permit text. Run disposable filesystem disk-full tests, host reboot recovery, separate observer missed-heartbeat tests, and restore reconciliation. Record unavailable capabilities as a failed gate. A real agent's own completion summary remains untrusted; retain trusted validation/result adapters.

Acceptance evidence records build commit, dependency lock digest, OS/kernel/SQLite versions, containment mode, source revision, seed and time. No passing status is inherited from inspection of a previously examined third-party system, this design's schema check, or a successful build alone.
