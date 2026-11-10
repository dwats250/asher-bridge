# State transitions and invariants

All transitions below are coordinator transactions unless explicitly supervisor-owned. Each mutation checks the current version and appends an event. Forbidden transitions return conflict and produce no effect. Timeouts cause transitions; a UI polling failure does not.

## Global accounting invariant

Exactly one accounting mode exists for each work row:

- `active`: `next_owner`, `next_action` and `due_at` are non-null; no closure. This includes queued work and reconciliation.
- `attention`: same scheduling fields name who follows up and when; `attention_id` points to a live attention item; no closure. Acknowledgment alone does not remove this state.
- `closed`: supported outcome, closure evidence, closing actor, reason and time; no next responsibility or live blocking attention.

The invariant is checked inside every work mutation and by a periodic obligation sweep. A past-due row is a service fault requiring a durable attention item, not an excuse to remove the row. Closed rows are immutable except append-only correction events; a newly discovered unresolved effect creates a linked recovery work item and explicitly challenges the old outcome in views. Do not silently rewrite historical success.

## Work mode and phase

Mode is accounting, phase is workflow position; they are not interchangeable.

| From | Trigger and guard | To | Next responsibility |
|---|---|---|---|
| No row | Valid request identity/scope + permit reference; acceptance transaction commits | active / queued | Coordinator schedules within 10 s |
| queued | Permit valid; capacity; launch intent committed | active / preparing | Supervisor start by 10 s |
| preparing | Candidate exists, writers stopped, exit/cleanup known | active / validating | Validator completes within 60 s in fixture |
| validating | Required checks pass on frozen snapshot | attention / reviewing | Owner decides; dispatcher follows up on deadline |
| validating | Reproducible failure or missing/stale evidence | active / reconciling, or attention / blocked | One bounded evidence refresh, then owner attention |
| reviewing | Exact valid approve + effect intent committed | active / applying | Action executor dispatches within 10 s |
| reviewing | Reject | active / finalizing | Coordinator records rejection and verifies cleanup |
| applying | Result confirmed | active / finalizing | Validate effect and cleanup |
| Any nonclosed | Crash ambiguity, lost supervision, conflicting evidence | active / reconciling | Reconciler probes within 10 s; bounded by 60 s |
| reconciling | Outcome established, action still authorized | Appropriate active phase | Resume only the proven next action |
| reconciling | Bound reached or consequential ambiguity | attention / blocked | Owner decision; follow-up deadline remains |
| Any nonclosed | Cancellation requested | active / finalizing or reconciling | Stop and inspect possible effects; cancellation does not undo them |
| finalizing | Outcome supported, no unaccounted effect or live process | closed / done | None |
| attention / blocked | Explicit abandon with residual-risk record | closed / done, outcome abandoned | Follow-up work required for any live unmanaged hazard |

Outcomes: `succeeded`, `failed`, `rejected`, `cancelled`, `abandoned`. Every terminal outcome creates a nonblocking outcome notice in the same transaction, so a closed failure still demands visible delivery; its unresolved notice does not reopen the work. Failed requires evidence of the failure and known effect/cleanup status. Cancelled requires confirmation of stopped work and accounting for any effect already performed. If uncertainty cannot be resolved, abandonment must say so and must never be presented as success. A live unmanaged process cannot be made invisible by abandoning its parent work: keep a linked active/attention cleanup obligation.

## Execution attempt (one record per launch)

`intent → accepted → starting → running → exited → cleaned` is the normal path.

| Transition | Required evidence / interpretation |
|---|---|
| intent → accepted | Supervisor receipt committed for the same request fingerprint; no process claim |
| accepted → starting | Startup begins under its immutable attempt ID and deadline |
| starting → running | Trusted wrapper handshake, process identity, output plumbing ready; release authorized |
| starting → start_failed | Proven no workload released and no remaining process; otherwise unknown |
| running/starting → stop_requested | Durable stop intent; cleanup may proceed even if DB is unavailable |
| stop_requested → stopped | Exact supported stop scope is empty; status may still be unavailable |
| running → exited | Parent exit observed with identity and exit/signal; does not assert descendants absent |
| exited/stopped → cleaned | Containment empty at supported scope, output closed, final census stored |
| Any live state → unknown | Process identity, exit, scope or receipt continuity cannot be established |
| unknown → running/exited/stopped/cleaned/start_failed | Reconciliation supplies missing proof |

Unknown attempts retain the unique live-attempt slot. No second attempt while a prior attempt's possible activity is unresolved. Retrying preparation uses a new attempt ID and immutable permit version; never reset an old attempt to hide a failure. Limit M1 to one automatic preparation retry after confirmed cleanup and still-valid permit; no retry of ambiguous final actions.

## Approval/review lifecycle

Review statuses: `pending → authorized | rejected | expired | invalidated`; authorized can become `revoked` before effect dispatch or `consumed` when dispatch is claimed. Expiry/invalidation also applies to authorized but unconsumed reviews. Decisions are immutable records, not status overwrite.

The decision request includes decision ID, review ID, envelope digest, expected work version and `approve|reject`. One effective decision per review. Same ID and bytes returns its prior receipt, even after state changes; same ID with different bytes is conflict. A second distinct decision is conflict and cannot cause effects. A new review is required to reconsider rejection.

Approval and effect intent creation are atomic. Dispatch claim atomically rechecks approval/permit expiry and revocation, preconditions, owner epoch and effect identity, then marks review consumed and effect `dispatching`. Revocation racing the claim is serialized: before claim it prevents execution; after claim it records “too late to guarantee prevention” and requests stop/reconciliation. Do not claim an atomic boundary between this check and an external operation. The exact reviewed action's native precondition must constrain the effect itself.

An expired review produces attention and requires fresh review/evidence; it never silently approves. Post-review byte edits, new base revision, new validation policy or changed permission scope invalidate the review. Re-display and re-decide against a new envelope.

## Attention lifecycle

Attention statuses: `open → acknowledged → resolved`; an open item can also resolve through a valid decision without a separate acknowledgment. Resolution requires a recorded reason and resolving transaction. Invalidated/expired review attention resolves as superseded only when replacement attention is created atomically.

Delivery is a separate outbox state: `pending → delivering → delivered`, or `delivering → retry_wait → delivering`. Crash during send may duplicate the notification; use the same attention ID. Delivery attempts and provider receipts are recorded. A receipt means only the channel accepted/displayed the message according to its contract, never that the owner saw it.

Acknowledgment records who saw/claimed attention; it does not decide, pause all deadlines, or close work. Snooze sets a bounded follow-up time (default maximum 1 hour) with actor/reason; it does not change review expiry. Escalation increases visible urgency and repeats the same channel, not an unapproved recipient. M1 has no alternative delivery path; channel failure remains explicitly visible as degradation.

## Recovery lifecycle

`needed → probing → resolved` or `needed/probing → attention`. Each case records reason, attempt/effect, observations, next probe and deadline. A case in attention remains unresolved. Owner-triggered reprobe returns to probing with a new bounded deadline; it is not an automatic infinite loop.

Reconciliation results are `confirmed_succeeded`, `confirmed_failed`, `confirmed_not_started`, or `unknown`. For process recovery “running with identity established” resolves the discovery case but leaves work active. Retry is legal only after proof of no hazardous prior activity/effect and a current permit. A `not_found` response with incomplete coverage is unknown, not `confirmed_not_started`.

## Scheduling defaults (proposals)

Coordinator sweep every 5 s; start deadline 10 s; fake runtime 60 s; interrupt grace 5 s, force-stop observation 5 s; evidence heartbeat age 15 s; recovery probe budget 60 s; notification retry 5 s, 30 s, 2 min, then every 15 min with bounded jitter; escalate after 5 min without delivery or 15 min without acknowledgment. Review expiry 24 hours. Use injected clocks in tests. Runtime deadlines use monotonic time; persisted wall deadlines are re-evaluated conservatively after restart. Clock rollback or unknown time expires authorization conservatively and raises attention rather than extending authority.
