# Asher Bridge: why it exists

This page explains the problem Asher Bridge addresses and how its parts fit together. It is not a substitute for the architecture documents or the tests. For the exact current state, read [STATUS.md](STATUS.md).

> **Asher Bridge is a prototype of a local-first coordination and recovery system for AI-assisted engineering.** It keeps durable records of task ownership, execution evidence and unresolved obligations, so that interrupted agent workflows can be reconciled without blindly repeating consequential actions.
>
> So far this has been demonstrated with a deterministic stand-in workload and a disposable fixture repository. No real AI agent has been connected to it.

## The problem

AI-assisted engineering work can be interrupted, leaving uncertainty about task ownership, execution evidence and whether consequential operations completed.

Someone who delegates work to several AI coding tools usually ends up as the glue between them. Each tool holds useful context, but there is no shared, durable view of who owns a task, what actually happened, what is still pending, or what a later session is allowed to do. The person carries prompts and results between conversations, inspects branches and working copies, recovers sessions, tracks incomplete runs, and remembers which tool had authority over which step. As the tools grow more capable, that administrative burden grows with them.

The friction is not only inconvenience. Sessions can be interrupted by context limits, connectivity failures, software restarts or usage limits. Work can complete without its acknowledgement reaching the agent that requested it. A new session might then attempt the same action again, or assume success where evidence is missing.

**The practical question:** how can more engineering work be delegated without a person acting as the manual message bus, and without losing control of consequential decisions?

## The interrupted-operation dilemma

Imagine an agent submitting an operation, followed immediately by a connection failure. From the agent's perspective, the result is unknown:

- The operation might have completed. Repeating it could duplicate a consequential action.
- The operation might never have happened. Assuming completion could strand the task.
- A receipt might be incomplete, missing or corrupted. A confident narrative does not resolve the underlying fact.

The safe response is neither blind retry nor blind success. It is to **preserve uncertainty, inspect durable evidence, record unresolved obligations, and assign responsibility for the next decision**.

Asher Bridge provides durable coordination and recovery mechanisms so that this uncertainty can be reconciled without blindly repeating actions.

## What Asher Bridge is

Asher Bridge is a prototype of a **local-first engineering coordination system, written in Rust on SQLite**. Its design separates:

1. **Ownership and authority:** who owns work, which action was permitted, and where human approval is required.
2. **Durable ledger and evidence:** stable identities, execution facts, receipts, artifacts, recovery runs and unresolved obligations.
3. **Conservative reconciliation:** establishing what can be proven after interruptions without repeating consequential work.
4. **Local control:** a foundation that does not depend on one AI model or vendor to preserve task state.

Its aspiration is not to make the model smarter. It is to make the engineering work surrounding capable models **more dependable and accountable**.

## How it fits together

No background is needed for this section. There are five parts, and one note on what "agent" means today. Throughout, "the owner" is a role: the person who holds authority over the work and its consequential decisions.

**The ledger.** One SQLite database file in a private directory on the owner's machine. Everything the system knows is a row in it: each accepted piece of work, who must act next and by when, every attempt to carry it out, every decision, every piece of evidence. Records of what happened are added, not edited. The rule the whole design serves is that accepted work never disappears: it is either active with an owner, a next action and a deadline, or it is waiting on a specific question for the owner, or it is closed with an outcome the evidence supports.

**The coordinator.** A command-line program called `orchestrator`. The owner runs it to accept work, start an attempt, look at what is pending, record a decision, or check the ledger's health. It is the only program that accepts work, records decisions and changes what is owed; the supervisor adds only its own lifecycle records. It is not a service: it runs when a command is typed and exits when the command is done. Every time it starts in a mode that can change things, it first re-examines what was left unfinished.

**The supervisor.** A second, separate program that the coordinator starts for each attempt. Its only job is to launch the worker (today always a stand-in program, `fake-agent`, described below), keep it within time and output limits, stop it, and write down plain facts: started, stopped, cleaned up, how much output was captured. It never decides whether the work was good. Keeping it separate means that if the coordinator dies mid-attempt, something is still responsible for stopping the worker and recording what happened.

**Evidence.** The system does not take the worker's word for anything. What the worker produced is frozen into a snapshot identified by its hash, checked by code the coordinator trusts, and only then put in front of the owner. A worker's own claim of success is stored as a claim, never as proof. If evidence is found to be missing, cut short or altered, that is recorded permanently as a *gap*, and a gap never counts as a pass (one known exception: a capture that cannot be read at all blocks sealing but may leave no gap row).

**Attention and owner decisions.** When something needs the owner, the ledger records a durable attention item; a desktop notification is only a nudge toward it. The one consequential action the prototype can take needs the owner's authenticated approval of the exact bytes and the exact action. Approving is not the same as succeeding: after the action runs, the result is read back independently, and if it cannot be established, the answer recorded is "unknown", with the owner named as responsible for inspecting it. There is no "try again anyway" command.

**What "agent" means today.** The worker is a small fixed test program called `fake-agent`. It has a handful of built-in behaviours (finish normally, hang, spawn a child that escapes, flood output, prepare one fixed correction to a fixture file), has no network access, and runs only inside throwaway fixture directories. The "consequential action" is moving a branch pointer in a throwaway Git repository with no remote. The AI coding tools mentioned in "The problem" describe the situation that motivates the project. The product does not connect to or drive any of them.

## What has been demonstrated

**Engineering status.** Asher Bridge is an **engineering-validated prototype**. Nothing has been released or deployed. The repository's verification script, `scripts/ab-verify.sh`, expects **243 tests to pass in both a parallel and a serial run**, alongside formatting, Clippy, release-build and sandbox-boundary checks. That result was obtained on the development host and has not been independently reproduced. The tests run with a test-only `fault-injection` feature that ordinary release builds do not contain.

The implemented recovery foundation has been tested for:

- **Storage-fault classification, uncertain commits, conservative readback and recovery obligations.** Faults are injected inside SQLite's commit path by a test wrapper, and by a second real connection holding locks; they are not real disk failures. Classification covers seven named transaction boundaries. A readback can establish that a record is present, never that it is absent.
- **Ledger integrity checks, guarded backups and inspect-only restored ledgers.** A restored ledger can be examined but not resumed; adopting one for ordinary use is not implemented.
- **Bounded diagnosis that writes nothing, and recovery passes that take no consequential action**: a recovery pass runs the same classification and reconciliation as an ordinary start and never dispatches, launches, retries, stops, notifies, closes, repairs or restores. On a ledger restored from a backup it records an accounting pass only.
- **Restarts, stale process identity and PID reuse.** Process kills and restarts of the coordinator and supervisor are real. Host reboot and PID reuse are simulated by substituting what the operating system reports.
- **Conservative time and deadline behaviour under injected clock rollback and simulated suspend.** A rollback never renews expired authority.
- **Missing, truncated and altered evidence**, including comparison of a digest of captured output at validation. The captured output is the stand-in worker's, on disposable fixtures.

**Important distinction.** These results come from controlled automated tests, not from use. It is not a demonstrated production-ready or unattended installation. **No real host reboot, suspend, clock change, power loss or kernel-level storage fault has been performed; where these were covered at all, they were simulated through test-only hooks** (power loss was not simulated as such: only process kills and injected write and sync errors). The schema migrations have only ever run on disposable or synthetic test ledgers; no ledger of real data exists.

## Who might find it useful

Asher Bridge addresses a problem that can affect any developer running multiple AI-assisted engineering tasks:

- **Solo developers** may want less manual tracking of sessions, working copies and unfinished assignments.
- **Multi-agent developers** may need clear ownership and evidence when work overlaps or gets interrupted.
- **Small engineering teams** may benefit from an auditable separation between request, execution, review and acceptance.
- **Security-conscious engineers** may appreciate local control, bounded authority, and failure modes that stop rather than guess.
- **Agent-framework builders** may find durable accounting and recovery primitives useful independently of the model layer.

These are **candidate audiences**, not established customers or evidence of product-market fit. None of them can use the prototype for that purpose today, because it drives only its own stand-in worker.

The strongest demonstration would be a short, runnable example: start a task, interrupt it at a consequential boundary, restart the coordinator, and show that it accounts for the uncertainty without duplicating an action. Today that exact sequence exists only inside the automated test suite, which pauses and kills the program at named points that ordinary builds do not contain. What can be shown by hand is narrower: uncertainty being preserved, and a repeated command not repeating the action. [GETTING-STARTED.md](GETTING-STARTED.md) says which is which.

## Why share it

The project investigates a concrete engineering challenge: **how to make delegated work accountable when agents, processes and communication channels are not perfectly reliable**.

The code, architecture contracts, adversarial tests and explicit limitations are written so that others can evaluate where a test backs a guarantee and where the design remains experimental.

It should not be read as a universal agent manager, a zero-risk sandbox or something ready for production deployment. Its credibility depends on precise claims, tests that can be read and honest boundaries.

## Limitations and next proof

Limitations that must remain visible:

- **The only worker is the deterministic stand-in**, and the only consequential action is on a disposable fixture repository. Nothing here shows readiness to run a real agent.
- **Nothing runs unattended.** There is no service, scheduler or installer; checks and notifications happen only while the owner is running a command.
- **The trust boundary is the user account.** Another process running as the same user can read the owner's credential and edit the ledger. The safeguards detect accidents and support attribution; they are not isolation.
- Large or difficult ledgers can receive an **inconclusive** diagnosis rather than a definitive healthy result; full integrity checks can be costly.
- Unreadable captured evidence blocks sealing but may not always produce a durable gap row.
- Some status displays can be misleading during wall-clock rollback, although authorization paths remain conservative.
- A very large forward clock jump may deny authority until wall time catches up.
- The schema migrations, including 007 and 008, have run only on disposable or synthetic test ledgers, never on real data.
- Physical reboot, suspend and clock changes were simulated, not performed on hardware; power loss and kernel-level storage failures were not exercised.
- The verification result comes from one development host. Reproducing it elsewhere requires obtaining the pinned toolchain yourself and is unverified.

The full list is in [STATUS.md](STATUS.md).

The most meaningful next demonstration would be a **small, supervised, real engineering workflow**: assign, execute, review, interrupt in a controlled way, recover, reconcile, and provide the owner with a truthful decision package. That step is not built. Before the system could be relied on routinely, installation and day-to-day operation would also have to be made understandable to someone who did not build it.

## The central idea

**AI agents have become capable enough to take on substantial engineering work, but a person is still the glue between fragmented sessions and tools.**

Asher Bridge aims to remove repetitive manual coordination **without surrendering authority over consequential decisions**.

It is not autonomy for its own sake. It is **durable accountability for increasingly autonomous engineering**.
