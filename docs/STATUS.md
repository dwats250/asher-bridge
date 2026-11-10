# Implementation status

Asher Bridge is an engineering-validated prototype. Schema 8, migrations 001–008. The pinned verification expects 243 tests. Nothing has been released or deployed, and no real AI agent is connected.

## In plain language

**What it is.** Asher Bridge is a prototype of a local record-keeper for delegated engineering work. It runs on one Linux machine, keeps its records in a single SQLite database (the *ledger*), and is operated by hand from a command-line program. Its purpose is to keep an honest account of what work was accepted, who is responsible for the next step, what actually happened, and what is still unresolved, so that after an interruption nothing is silently repeated and nothing is silently assumed to have succeeded.

**What it can do today.** On disposable test fixtures it can accept a piece of synthetic work under a bounded permit; run a fixed, deterministic stand-in worker (`fake-agent`) under a separate supervisor process; freeze and check what the stand-in produced; put an exact proposed action in front of the owner for an authenticated approve-or-reject decision; perform that one action (updating a branch pointer in a throwaway Git repository) and verify the result independently; raise durable attention items and, during a manually run sweep, send a desktop notification; close work with an explicitly classified outcome; and diagnose, back up and restore the ledger for inspection.

**What the tests demonstrate.** That across process kills, restarts, injected storage errors, simulated reboots, simulated clock rollback and suspend, and missing or altered evidence, the ledger keeps uncertainty as uncertainty: it does not repeat the consequential action, does not turn a missing or doubtful result into success, and does not renew expired authority.

**What has not been shown or built.** Not built: **no real AI agent has ever been connected, and the only worker is the deterministic stand-in.** The only consequential action is the fixture branch-pointer update. There is no service, scheduler, installer or unattended operation. Simulated only: host reboot, PID reuse, suspend and clock changes were simulated through test-only hooks, never performed. Not exercised at all: power loss and kernel-level storage failures; storage errors were injected inside SQLite, not by the kernel. It is an engineering-validated prototype, not production-ready and not validated for unattended use.

New here? Read [WHY-ASHER-BRIDGE.md](WHY-ASHER-BRIDGE.md) for the problem and the idea, [GETTING-STARTED.md](GETTING-STARTED.md) for the checks and a small demonstration, and [README.md](README.md) (this directory) for the documentation map.

## What is implemented

| Area | What exists | Operator reference in [OPERATIONS.md](OPERATIONS.md) |
|---|---|---|
| Acceptance ledger | Durable, idempotent acceptance of synthetic work under a bounded permit | Task 2 part ("Durability and meanings", "Schema and ownership") |
| Supervised execution | The stand-in worker run by a separate supervisor process; lifecycle facts; bounded output and runtime; reconciliation at every mutating start | Task 2 part |
| Evidence | Trusted fixture checks; candidate output frozen into a hashed snapshot | Task 3 part |
| Review and action | Exact review envelope; authenticated owner decision; one fixture ref compare-and-swap; independent readback and reconciliation | Task 4 part |
| Attention and closure | Durable attention items; desktop notification; owner commands; manual sweep; supported work closure | Gate 5 part |
| Storage recovery | Storage-fault classification; recovery runs and startup breaker; `diagnose`; verified backup; inspect-only restore | Gate 6 M1 part |
| Restart and time | Boot identity; stale-identity handling; suspend-aware budgets; durable time floor; evidence gaps; capture digest | Gate 6 M2 part |

The stage labels (Task 2–4, Gate 5, Gate 6 M1 and M2) are only the headings under which [OPERATIONS.md](OPERATIONS.md) is organised.

The design documents in [`architecture/`](../architecture/README.md) define lettered promotion gates (A–D in [08-decisions.md](../architecture/08-decisions.md)) and a "Milestone 1". This page does not claim that Milestone 1 or any lettered gate has been passed.

## Verification

`scripts/ab-verify.sh` is the one supported way to check the code. Inside an offline sandbox with the pinned Rust 1.99.1 toolchain it runs:

- the test suite twice, in parallel and with `--test-threads=1`, both with the `fault-injection` feature, and expects exactly **243 passed, 0 failed, 0 ignored** in each run;
- `cargo fmt --check`;
- Clippy with warnings denied;
- the locked, offline release build;
- a dependency proof, a sandbox boundary probe, and a check that the three release binaries contain no test-hook strings.

The 243 are 17 unit tests and 226 integration tests: 18 acceptance, 40 attention, 22 evidence, 8 evidence_loss, 21 lifecycle, 4 pid_reuse, 63 recovery, 15 restart_time, 35 review.

**That result was obtained on the development host. It has not been independently reproduced**, and whether the toolchain digests the scripts check can be matched on another machine is unverified (see [TOOLCHAIN.md](TOOLCHAIN.md) and [GETTING-STARTED.md](GETTING-STARTED.md)).

The tests run with the `fault-injection` feature. Ordinary release builds do not contain the fault hooks.

## Schema

Current schema is 8 (`migrations/001.sql` to `008.sql`). A mutating start migrates an older ledger forward automatically, one migration step per transaction. The read-only commands `list`, `get`, `inbox`, `check`, `readback` and `diagnose` accept schemas 1 to 8; `attempts` and the supervisor require exactly 8. The migrations have run only on disposable and synthetic test ledgers.

## Demonstrated for real, and simulated only

| Performed for real (in the test suite unless noted) | Simulated through test-only hooks |
|---|---|
| Killing and restarting the coordinator and supervisor processes, including at named points inside a transaction or an action | Host reboot (a substituted boot id) |
| A competing SQLite connection holding real locks | PID reuse (a substituted `/proc/<pid>/stat`) |
| The real `git` binary as an independent check and competing writer on the fixture repository | Suspend, clock rollback, forward jump, unreadable clock (offset files) |
| One attended desktop notification run, by hand and outside the test suite, on the development host; not reproducible from this repository | Disk-full and I/O errors at commit (a wrapping SQLite VFS, not the kernel) |

Never performed, simulated or otherwise: power loss, a real kernel-level storage failure, a soak test, or any migration of a ledger holding real data (only disposable and synthetic ledgers exist).

## Known limitations

These are known limits of the prototype. Operational detail is in [OPERATIONS.md](OPERATIONS.md).

Scope and trust:

- The fixed fake agent is the only executor; the only consequential action is `fixture.ref_compare_swap` on a disposable, no-remote fixture repository.
- Linux x86-64 only, and the tested `/proc`, pidfd and seccomp capabilities are required.
- Same-user trust boundary: a process running as the same user can read the owner credential and edit the ledger, emergency log, backups and audit trail. Hashes and identities detect accidental change and support attribution; they are not isolation. The owner credential authenticates the owner console against agents and forged roles; it does not defend against a compromised same-user account.
- Cooperative process groups are not whole-descendant containment. `cleaned` means observed cleanup of the supported cooperative stand-in scope only.
- No exactly-once guarantee across SQLite and Git. The final authorization recheck is not atomic with the ref rename. Revocation after the dispatch claim cannot guarantee prevention; it is recorded and the effect reconciled.
- A desired end state alone does not establish attributable execution: a ref found at the proposed commit with no audit entry and no executor result is recorded as unknown and is never eligible for closure. Ambiguity stays unresolved; there is no retry command.
- Candidate content between cleanup and seal is not attested by the supervisor. Trusted evidence expires after one hour; bounded revalidation (at most three per attempt) reruns checks on the same frozen bytes and never extends timestamps.
- Each accepted work's permit allows one preparation attempt; there is no automatic retry policy. A final action is never retried automatically.

Attention and notification:

- Sweeps are manual; nothing is checked or delivered unless a command is running, and nothing wakes the host.
- Deadlines are cooperative: delivery and sweep budgets are enforced by the process doing the work, not by an external supervisor.
- A desktop notification can be duplicated, because SQLite and the notification daemon share no atomic transaction. Its receipt proves only that the notification daemon accepted it, never that anyone saw it.
- Irrecoverable evidence stays unresolved. Unknown, unattributed, quarantined or overwritten results are never turned into success; they remain blocking discrepancies for the owner.

Storage and recovery:

- Fault classification covers seven transaction boundaries (T1, T4, T5, T6, T7, S1, S2); others keep a generic message. The commit that finishes a recovery run is among the unclassified ones.
- `diagnose` is `inconclusive`, never `healthy`, on a ledger over 64 MiB, when its 5 s budget runs out, or when the emergency log is unreadable or over 256 KiB. It does not check that `coordinator.lock` and the write-ahead log are writable, and a query interrupted by its watchdog may be reported as definite.
- The full integrity check runs at every ordinary open and costs time proportional to the ledger. Behaviour on very large ledgers is not covered by the test suite.
- Backups are local and exclude artifact snapshots, fixture repositories, the owner credential and the emergency log. An interrupted backup can leave a `.snapshot-*.partial` file behind. A restored ledger is inspect-only; adopting it for ordinary use is not implemented.
- Recovery history grows with every mutating start and is not pruned.

Restart, time and evidence:

- An unreadable capture refuses sealing but may leave no durable gap row.
- Several kinds of capture corruption share one gap kind, `capture_torn`.
- A capture that is altered and then restored to its original bytes before any check is undetectable. The capture digest is not a defence against a same-user rewrite of the ledger.
- Status displays ("expired", "overdue") can show raw wall time under a clock rollback, although authorization stays conservative.
- A large forward clock jump that is later corrected blocks authority until wall time catches up.
- A supervisor killed before it records `cleanup_completed` has no dedicated test under the capture-digest check.
- Migration 008, like the others, has not run on real data.
- Real reboot, suspend and clock changes were simulated only. Storage errors were injected inside SQLite by a test wrapper. Power loss was not simulated.

Demonstration and documentation:

- The verification result has not been independently reproduced, and obtaining the pinned toolchain is left to the user.
- No hand-run of `diagnose`, `backup`, `restore`, `recover` or `readback` is recorded; those commands are covered by the automated tests.
- The crate comment at the top of `src/lib.rs` describes an earlier state and still says there are no notifications or work closure.

## Not built

- No adapter for any real AI agent. The stand-in must not be replaced with a real agent without the separate design, containment and authorization work described in [08-decisions.md](../architecture/08-decisions.md) (Gate C).
- No service, scheduler, installer, remote API, off-host observer, hooks or unattended operation.
- No release and no deployment.
- No adoption of a restored ledger for ordinary use.
- No phone or watch client.
