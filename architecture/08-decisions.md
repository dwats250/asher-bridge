# Owner decisions and unresolved questions

These are review questions, not a request to authorize implementation or deployment now. The design proceeds using labeled defaults. Defaults affecting real work must be explicitly confirmed before the relevant promotion gate.

## Decisions for the owner

| Decision | Recommended default | Consequence / alternative |
|---|---|---|
| Initial platform/language | Linux-only Rust prototype with SQLite; revisit after first two tasks | Good typed contract review; more implementation complexity than a familiar Go/Python stack. Choose maintainability if Rust slows reliable delivery |
| Exact first approved action | Compare-and-swap one ref in a disposable repository | Demonstrates review/effect reconciliation without touching projects. A real merge is a separate future action contract |
| Standing authority | Investigation, candidate preparation and trusted validation only; one automatic retry after proven cleanup | Minimizes blast radius; no automatic publication, merge, bulk rewrite, credential use or dependency installation |
| First attention channel | Durable local inbox plus local desktop notification, the owner as sole recipient | Works only while local components/session are available. Harness sink proves retry logic, not human delivery |
| Unattended outage coverage | Require an independent off-host observer before operational reliance | Adds service/channel choice and privacy considerations; alternative is explicit acceptance of no notice until recovery |
| Durable ledger loss boundary | M1 accepts local-storage failure risk; require verified independent backups before operational use, and decide acceptable recovery point | Backup intervals can lose recent acceptance records. Zero accepted-record loss across host destruction requires independent durable receipt replication before acceptance, beyond M1 |
| Who may close failures? | Coordinator may close evidenced deterministic failure after cleanup and issue outcome attention; the owner alone may abandon uncertainty | Avoids needless manual closure of routine failures while ensuring unknowns cannot vanish |
| Approval duration | 24 hours, single exact action, revoke until dispatch claim | Longer periods increase staleness; after dispatch revocation cannot guarantee prevention |
| Attention urgency | Delivery escalation after 5 min, unacknowledged action after 15 min; same recipient/channel in M1 | Does not guarantee interruption or response. Alternate recipients/channels require explicit consent |
| Agent outage policy | Existing bounded fake attempt may run until absolute deadline; no new launch while owner absent | Preserves useful output without unbounded autonomous activity; real adapter may require immediate stop on control loss |
| Real adapter privileges | Enforced fixture-only access, no credentials/network initially, no control-plane access | May limit the agent; extending privileges needs a new scoped review and tests |
| Uncertain consequential effect | Hold for reconciliation, no automatic retry | Slower progress but avoids duplicate merges/payments/messages. Native idempotency can justify a later bounded retry contract |
| Retention | Keep unresolved/accounting metadata; logs 7 days / 10 MiB per attempt; redact early | Less debugging detail after expiry; artifact/evidence references required for review must remain available |
| Future phone/watch authority | Phone may later decide through same validation; watch initially status and attention acknowledgment only | Watch cannot silently approve from a truncated presentation; adding approval needs explicit device/identity design |

## Unresolved technical questions

1. Does the target host provide suitable cgroup delegation, service-manager APIs and filesystem/network isolation without broad agent privileges? M1 group-only prototype can expose limits; real promotion must inspect and demonstrate capabilities. No host configuration was inspected or changed here.
2. Can the chosen agent adapter emit reliable execution identity and result provenance? What does its own retry behavior do? A hidden provider retry can duplicate effects even if coordinator deduplicates requests. Disable or account for it.
3. Which exact domain rules define real success? The fixture values are synthetic. Domain-owner rules and authoritative data sources must replace them before active-project work.
4. What future action is intended after review: patch export, branch commit, local merge, remote PR, or remote merge? Each has different idempotency/readback and authority. None follows from fixture approval.
5. Which local notification API is available, and what does its receipt actually promise? Validate with the smoke test; keep channel semantics explicit.
6. Who should receive outages when the owner's desktop is absent? No external account, endpoint, phone number or reviewer contact is assumed.
7. What ledger-loss recovery point is acceptable? Off-host observation detects silence but cannot reconstruct accepted work. Backups and receipt replication solve a different problem.
8. Which secrets/data may future evidence contain, and who may inspect retained logs? Real adapter gate requires a redaction/retention review.
9. Should owner abandonment be permitted with residual uncertainty? Recommended yes only as explicitly non-successful accounting, preserving evidence and linked cleanup for live hazards. If prohibited, the obligation must remain under attention indefinitely.

## Promotion gates

### Gate A: design approval to implement milestone 1

Review this package's invariants, fixture action and scope; choose language provisionally. Future implementation stays in a new isolated prototype repository/root. No gate implies permission to modify active projects. This delivery ends at design.

### Gate B: milestone 1 accepted

All required acceptance cases pass from a clean checkout, with no ignored faults; separate reviewer verifies crash evidence, no false ACK/success, role checks, durable attention, external-effect reconciliation and clearly exposed group-only containment limits. A local notification smoke test demonstrates the selected channel. Any unresolved blocker is listed as a failed gate rather than waived by a happy-path demo.

### Gate C: one real agent adapter, still fixture-only

Owner explicitly selects one adapter and grants its narrow permissions. Require enforced process containment and filesystem isolation, control-plane separation, credential/network restrictions, no hidden retries, capability/version inventory and the same failure tests. The adapter first solves the disposable symbol fixture, with trusted validation and the same exact-review boundary. No auto-merge, active repository access, install hooks or production scheduler.

A reviewer must demonstrate attempted forbidden access is denied, escaped descendants are stopped by the enforced scope, stale/duplicate decisions cannot cause effects, and an ambiguous adapter result stays unresolved. If restrictions require privileges/configuration, prepare the precise configuration for review before any such action; do not silently broaden authority.

### Gate D: first unattended real-project pilot (separate future design)

Select one repository and exact action, read its owner rules, create isolated workspace and backup strategy, define domain evidence and success criteria, grant bounded permit, establish independent observer/delivery path and ledger-loss recovery policy, test reconciliation against the real action semantics, then obtain explicit authority. A real scheduler must durably register expected runs and alarm on missing submissions. Desktop/phone/watch integration and broader autonomy remain separate gates.
