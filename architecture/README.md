# Local agent orchestration foundation

> **Note.** This package is the original design proposal, kept as written. An implementation now exists; its current state is in [../docs/STATUS.md](../docs/STATUS.md). Statements below that nothing is implemented describe the time of writing.

Design proposal · November 7, 2026 · prepared for the owner, a future implementer and an independent reviewer.

**Status: reviewable design, not an implemented or approved system.** No code from a previously inspected third-party system was executed or copied; no hooks, integrations, active projects, agents, services, or deployment were changed. Only this documentation package was produced. Defaults below authorize no future operations.

The recommendation is a small Linux application with two process roles, coordinator and supervisor, one local SQLite database, typed evidence adapters, and a durable attention inbox. Begin with a fake agent, a disposable Git fixture, one local attention channel, and one work item at a time. Keep UI thin. Prefer Rust provisionally; confirm the platform and maintainability gates before choosing it permanently.

The governing invariant is **accepted work never disappears**: each committed request is active with an owner, next action and deadline; awaiting a specific attention item; or closed with a supported, explicitly classified outcome. An unknown effect, missing exit status, stale observation, or unavailable database cannot be converted to success. There is no terminal “unknown” state.

## Read and review order

| Document | Review purpose |
|---|---|
| [Architecture and trust](02-architecture.md) | Components, isolation, milestone scope and technology tradeoffs |
| [States and invariants](03-states.md) | Work, attempts, approvals, attention and recovery transitions |
| [SQLite and transactions](04-storage.md) | Minimum relational proposal, atomic boundaries and durability |
| [Contracts](05-contracts.md) | Acknowledgment meanings, identities, evidence and review binding |
| [Failure and recovery](06-recovery.md) | Crash windows, ambiguous effects, notifications and outage guarantees |
| [Executable acceptance specification](07-acceptance.md) | Future runnable harness, fault injection and objective oracles |
| [Owner decisions](08-decisions.md) | Recommended defaults, consequences and promotion gates |

## Proposed first demonstration

A simulated review run accepts `fixture/symbols/example` under a bounded preparation permit. A trusted validator reproduces a malformed symbol in a disposable repository. The fake agent prepares a correction; validation runs against a frozen snapshot. Coordinator creates a review and durable attention in one transaction. The owner approves the exact snapshot and a compare-and-swap update of a **fixture-only Git ref**. A trusted action executor performs it; an independent readback establishes the result. Coordinator records supported success. Approval is not itself success. A simulated missed capture instead demonstrates investigation and a proposed repair without claiming historical data was captured.

Failures, rejection and cancellation are demonstrated too. A failure outcome may close only after required cleanup and accounting. An unresolvable effect stays under attention; an owner can accept residual uncertainty as an explicitly non-successful abandonment with the unresolved facts preserved.

## Deliberately small

No desktop framework, phone transport, watch integration, production scheduler, agent plugin system, generic workflow language, remote Git operations, auto-merge, real credentials, or active-project authority in milestone 1. A local inbox plus local desktop notification delivery constitutes one channel; the inbox is authoritative, and notifications only draw attention to it. The harness substitutes a deterministic delivery sink for tests.

## Guarantee boundary

The design provides transactional local accounting, deduplicated commands and explicit external-effect reconciliation. It does **not** promise exactly-once arbitrary external actions, immunity to a compromised account, or alert delivery from an unavailable host. Milestone 1 is an attended prototype. Operational reliance requires an independent outage observer and an agreed alert recipient/path, or a documented owner decision accepting delayed notice until restart.

## Review checklist

- Can every accepted work row be classified by the invariant after every injected crash?
- Can an approval be used on different bytes, evidence, scope, target or revision? It must not.
- Can a lost response cause a second process or unsafe repeated effect? It must not without reconciliation.
- Can an exit code, notification receipt, transcript or agent claim alone cause success? It must not.
- Are containment limits and unavailable alert paths visible, including under database failure?
- Does the real-adapter gate demand demonstrated enforcement and failure tests, rather than configuration claims?

All commands and APIs described in this package are proposed future interfaces. The acceptance harness and product do not exist yet; their tests were not run. Documentation checks and in-memory schema validation do not establish product behavior.

## Design-package checks performed

The supplied archive documents were read and the three governing source files matched their sibling copies byte for byte. All local document links and fenced code blocks were checked. The proposed DDL parsed in an in-memory SQLite database; representative acceptance, attention and closure rows committed with consistent foreign keys. Negative checks rejected a missing deadline, duplicate request key, unsupported closure and event deletion. These are schema/document checks only; the application, harness, notification channel and failure guarantees remain unimplemented and untested.
