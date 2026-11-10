# Documentation map

Where everything is, and how much weight to put on it.

Asher Bridge is an engineering-validated prototype of a local record-keeper for delegated engineering work. The only worker it runs today is a deterministic stand-in; it is not production-ready and not validated for unattended use.

Two kinds of document live here, and they should not be read the same way:

- **Current** documents describe the code as it is.
- **Design contracts** were written before the implementation and say what the system is meant to guarantee. The implementation diverges from them in places.

## Start here

| Document | Read it for |
|---|---|
| [WHY-ASHER-BRIDGE.md](WHY-ASHER-BRIDGE.md) | The problem, the idea, how the parts fit together, what has and has not been shown. No background needed. |
| [STATUS.md](STATUS.md) | What is implemented, how it is verified, what was real and what was simulated, the known limitations and what is not built. Opens with a plain-language summary. |
| [GETTING-STARTED.md](GETTING-STARTED.md) | What you need, how to run the checks, and a small demonstration on throwaway data. |

## Operating and building

| Document | Read it for |
|---|---|
| [OPERATIONS.md](OPERATIONS.md) | The operator reference: every command, what each result means, exit codes, recovery and restore procedure. It is written one part per stage of the implementation (Task 2, Task 3, Task 4, Gate 5, Gate 6 M1, Gate 6 M2), each part building on the ones before. |
| [TOOLCHAIN.md](TOOLCHAIN.md) | The pinned offline Rust toolchain, the sandbox and the one supported way to build and verify. |

## Design contracts

The [`architecture/`](../architecture/README.md) directory is the **pre-implementation design proposal**. It is the source of the governing rule ("accepted work never disappears") and of the contracts the implementation was built against. It is not a description of the current code: where the two differ, [OPERATIONS.md](OPERATIONS.md) and [STATUS.md](STATUS.md) describe what was built.

| Document | Subject |
|---|---|
| [architecture/README.md](../architecture/README.md) | Overview, governing invariant, reading order |
| [02-architecture.md](../architecture/02-architecture.md) | Components, trust boundaries, technology choice |
| [03-states.md](../architecture/03-states.md) | States and invariants for work, attempts, approvals, attention |
| [04-storage.md](../architecture/04-storage.md) | SQLite schema proposal and the transaction boundaries (T1–T10, S1, S2) |
| [05-contracts.md](../architecture/05-contracts.md) | What each acknowledgment means; identities, evidence, review binding |
| [06-recovery.md](../architecture/06-recovery.md) | Crash windows, ambiguous effects, outage behaviour |
| [07-acceptance.md](../architecture/07-acceptance.md) | The acceptance-test specification |
| [08-decisions.md](../architecture/08-decisions.md) | Owner decisions and the lettered promotion gates A–D |

## Source layout

| Path | Contents |
|---|---|
| `src/` | The library and the three programs in `src/bin/`: `orchestrator` (the coordinator and command-line tool), `supervisor`, and `fake-agent` (the stand-in worker) |
| `tests/` | The integration tests; `tests/scenarios/` holds two scripted interruption scenarios |
| `migrations/` | Schema migrations `001.sql` to `008.sql` |
| `scripts/` | `ab-verify.sh` (full verification), `ab-selftest.sh` (checks of the environment checks), `ab-env.sh` and `ab-sandbox.sh` (the pinned environment and the sandbox) |

## Glossary

Several words are used in more than one sense across these documents. These are the meanings to assume.

- **Ledger.** The SQLite database (`ledger.db` in a state directory) holding every record the system keeps.
- **Coordinator.** The process role that owns the ledger: it accepts work, checks permits, records decisions and performs recovery. It is the `orchestrator` binary, which is also the owner's command-line tool. "Orchestrator" names the program; "coordinator" names its role.
- **Supervisor.** The separate process that launches, bounds and stops an attempt and records lifecycle facts. It never decides success.
- **Owner.** A role: the person who holds authority over the work and its consequential decisions. Where OPERATIONS.md says "Owner" with a lock (`Owner::open`, "owner lock"), it means the coordinator process holding the exclusive ledger lock. `local-owner` as a `next_owner` value means the person.
- **Owner key, owner credential.** The secret (`owner.key` in the state directory) with which owner decisions and owner commands are authenticated. It distinguishes the owner console from agents and forged roles; it is not protection against another process running as the same user.
- **Agent, fake agent.** Today, only `fake-agent`: a fixed, deterministic, offline test program. No AI model is connected.
- **Worker, stand-in, executor.** Synonyms in these documents for the program an attempt runs; today always `fake-agent`.
- **Permit.** A ledger row that authorizes bounded preparation attempts for accepted work. It never grants the final action.
- **Attempt.** One supervised launch of the agent for a piece of work, with an immutable identity. A "delivery attempt" is a different thing: one try at showing a notification.
- **Candidate.** The agent's prepared output (`candidate/`), which becomes a candidate artifact once sealed.
- **Seal.** To freeze the candidate files into an immutable, hashed snapshot after the attempt is confirmed cleaned and quiescent, so that review and validation act on exact bytes.
- **Quiescence.** The condition that nothing that could still write the candidate is running.
- **Effect.** A consequential action outside the ledger; today only a compare-and-swap of a fixture Git ref. Its identity is recorded before it runs. After an interruption it is reconciled as confirmed, not applied, conflict or unknown, and is never blindly repeated. "Action", "final action" and "effect" are used interchangeably in places.
- **Attention item.** A durable record that a specific matter needs the owner. A notification is only an attempt to draw the owner to it. Neither delivery nor acknowledgment is approval or resolution.
- **Obligation.** Anything the ledger has accepted and not yet closed with a supported outcome. It must always have an owner, a next action and a deadline, or a live attention item.
- **Evidence gap.** A permanent record that some evidence for an attempt is missing, truncated or altered. A gap never counts as a pass and blocks sealing, validation and closure for that attempt. (A capture that cannot be read at all blocks sealing but may leave no gap row; see the limitations in STATUS.md.)
- **Inspect-only.** The state of any ledger derived from a backup: it can be read, never resumed.
- **Epoch.** A counter the coordinator advances at each mutating start, so that commands from an earlier run can be told apart.
- **T1…T10, S1, S2.** Names for the transaction boundaries defined in `architecture/04-storage.md` (T for coordinator transactions, S for supervisor ones).
- **Time floor, authority time.** The latest wall-clock time the ledger has durably observed, and the reading used for deadlines that grant authority. A clock that reads earlier than the floor is treated as uncertain, and uncertain is treated as expired.
- **Sweep.** One manually run `orchestrator sweep` pass that records what is overdue and attempts delivery of due notifications; nothing sweeps on its own.
- **Test-only hook (seam).** A hook compiled only under the `fault-injection` feature that lets tests pause the program at a named point, inject an error, or substitute what the operating system reports; absent from release builds. OPERATIONS.md calls these "test-only seams".
- **Recovery run, recovery case, startup breaker.** The record of one startup's re-examination; an open item for each obligation it could not resolve; and the rule that refuses ordinary startup after three unfinished runs in a row.
- **Engineering-validated prototype.** The code passes its pinned automated checks on the development host. It has not been used on real work, with a real agent, or unattended, and the result has not been independently reproduced.
- **Task 2–4, Gate 5, Gate 6 M1 and M2.** Labels for the stages in which the implementation was written. They survive only as the part headings of OPERATIONS.md. In its Gate 6 M2 part, a bare "M1" means Gate 6 M1.
- **Gate (lettered A–D), Milestone 1.** Promotion decisions and a scope name defined in `architecture/08-decisions.md`, about how far the system may be trusted. None is claimed here as passed. "Real-adapter gate" and "real-agent gate" refer to Gate C.
