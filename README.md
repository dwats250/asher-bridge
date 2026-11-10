# Asher Bridge

Asher Bridge is a local-first coordination and recovery prototype for delegated engineering work, written in Rust on SQLite. It is an **engineering-validated prototype: not production-ready and not validated for unattended use.**

## The problem

AI-assisted engineering work can be interrupted, leaving uncertainty about task ownership, execution evidence and whether consequential operations completed. Asher Bridge provides durable coordination and recovery mechanisms so that uncertainty can be reconciled without blindly repeating actions.

## What it does today

Everything below runs on **disposable fixtures only**, on one Linux x86-64 machine, operated by hand from a command-line program (`orchestrator`) that keeps its records in a single SQLite database, the *ledger*.

- Accepts a piece of synthetic work under a bounded permit and keeps its owner, next action and deadline durable.
- Runs a worker under a separate supervisor process and records launch, lifecycle and cleanup facts. The only worker is `fake-agent`, a fixed, deterministic stand-in.
- Freezes what the stand-in produced into a hashed snapshot and checks it with a trusted in-process checker. The worker's own claims are never treated as evidence.
- Puts one exact proposed action in front of the owner for an authenticated approve-or-reject decision.
- Performs that one action, a compare-and-swap of a branch pointer in a throwaway Git repository with no remote, and verifies the result with an independent readback. After an interruption the action is reconciled as confirmed, not applied, conflicting or unknown; it is never retried automatically.
- Raises durable attention items and, during a manually run sweep, sends a desktop notification. Delivery and acknowledgment are not approval.
- Closes work only with an explicitly classified outcome.
- Diagnoses, backs up and restores the ledger. A restored ledger is for inspection only.

What does not exist: **no real AI agent is connected**, and there is no scheduler, service, installer, remote interface or general isolation. Nothing runs unless a command is running.

## What has been demonstrated

The verification script expects **243 tests passed, 0 failed, 0 ignored** in both a parallel and a serial run, together with a format check, lint with warnings denied, a release build, sandbox boundary checks and a check that the release binaries carry no test-only hooks.

The tests kill and restart real coordinator and supervisor processes, including at named points inside a transaction or an action, and inject storage errors inside SQLite. Across those interruptions, and with missing or altered evidence, the ledger keeps uncertainty as uncertainty: it does not repeat the consequential action, does not turn a missing or doubtful result into success, and does not renew expired authority.

## What has not been shown

- Host reboot, PID reuse, suspend and clock changes were simulated by test-only hooks, never performed.
- Power loss and kernel-level storage faults were not exercised. Storage errors were injected inside SQLite, not by the kernel.
- Migrations have run only on test ledgers. No ledger holding real data exists.
- Processes running as the same user are not isolated from each other. Such a process can read the owner credential and edit the ledger, its backups and its audit trail. Hashes and identities detect accidental change; they are not isolation.
- Process groups are cooperative and are not whole-descendant containment.
- There is no exactly-once guarantee across SQLite and Git.

The full list of recorded limits is in [docs/STATUS.md](docs/STATUS.md).

## Intended use

The aim is a local record-keeper that lets an owner delegate engineering work to AI agents and, after any interruption, see what was accepted, who holds the next step, what actually happened and what is still unresolved. **This is an aspiration and is not yet possible**: connecting a real agent requires enforced isolation and its own validation, neither of which exists. Use the prototype only attended, on freshly created disposable state.

## Where to go next

| Document | Read it for |
|---|---|
| [docs/README.md](docs/README.md) | The documentation map and glossary |
| [docs/WHY-ASHER-BRIDGE.md](docs/WHY-ASHER-BRIDGE.md) | The problem, the idea and how the parts fit together |
| [docs/GETTING-STARTED.md](docs/GETTING-STARTED.md) | Prerequisites, the checks and a small demonstration on throwaway data |
| [docs/STATUS.md](docs/STATUS.md) | What is implemented, what was verified, limits and open follow-ups |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | The operator reference: commands, results, exit codes, recovery and restore |
| [docs/TOOLCHAIN.md](docs/TOOLCHAIN.md) | The pinned offline toolchain and the build sandbox |
| [architecture/README.md](architecture/README.md) | The original design proposal: the governing invariant, states, storage, contracts and recovery |

## Checks

The supported way to build and verify is the verification script. It needs Linux on x86-64, the pinned Rust toolchain and a pre-seeded dependency registry; see [docs/GETTING-STARTED.md](docs/GETTING-STARTED.md) and [docs/TOOLCHAIN.md](docs/TOOLCHAIN.md).

```sh
AB_ID=<new-id> AB_RUST_ROOT=<toolchain-root> scripts/ab-verify.sh
```

`<new-id>` is a short label that has not been used before; `<toolchain-root>` is the directory holding the pinned toolchain and registry. The script works offline in a fresh, sandboxed build root outside the checkout and exits 0 only if every required check passes. It must be run from a clean Git checkout, because it refuses a tree with uncommitted, untracked or ignored files. It runs these five commands:

```sh
cargo test --offline --locked --features fault-injection
cargo test --offline --locked --features fault-injection -- --test-threads=1
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo fmt --check
cargo build --offline --release --locked
```

The tests run with the `fault-injection` feature, which compiles in the test-only hooks. Ordinary release builds do not contain them.
