# Failure handling and recovery model

## Recovery is ordinary work

Every accepted obligation is checked at startup and every 5 seconds while healthy. Recovery belongs to the coordinator; process cleanup belongs to the supervisor. Recovery never marks a work successful simply because no process is visible. Retry budgets and deadlines are recorded, not reset silently on restart.

Startup sequence: acquire exclusive ownership locks; verify storage/settings/schema; advance coordinator epoch; disable new launches; load all nonclosed work, unresolved commands/effects and unacknowledged attention; query supervisor census/facts; compare artifact hashes and effect state; assign bounded recovery or attention; start delivery retries; then permit safe new work. Reconcile orphan supervisor receipts even if work was erroneously closed; create a linked incident obligation. No automatic deletion of orphan processes based only on a reused PID.

## Fault matrix

| Fault / ambiguity | Required immediate behavior | Recovery and closing condition |
|---|---|---|
| Crash before acceptance commit | No accepted response | Same request key query distinguishes committed from absent; absent may be submitted again |
| Commit succeeds, response lost | Work remains active with deadline | Retry returns original work; no duplicate obligation |
| Crash after launch intent, before command send | Attempt remains intent | Resend same command ID only after supervisor receipt/census lookup |
| Child created, identity/ACK missing | Assume possible process; no second launch | Reconcile blocked wrapper/known scope; prove stopped/not released, or attention |
| Output reader/writer/thread setup fails | Never release payload; stop exact startup scope | Confirm cleanup or mark unknown unmanaged process and urgent attention |
| Coordinator crash, supervisor alive | No new launches without current owner; existing bounded fake attempt may continue up to absolute deadline | New coordinator ingests durable facts and reconciles before next action |
| Supervisor crash | Wrapper control loss requests cooperative shutdown; coordinator marks supervision lost | Restart census; if complete identity unavailable, attention; never infer exit code |
| Both restart / host reboot | Boot identity invalidates old PID identity | Processes may be gone, effects may remain; inspect effects/artifacts and retain missing exit as gap |
| Leader exits, descendants remain | ExitObserved is not cleanup | Stop remaining supported scope; escaped descendants mean unknown until accounted for |
| Agent ignores interrupt | Grace deadline then forced stop | Require scope emptiness/readback; inability to stop produces urgent unresolved cleanup |
| Agent exits zero, evidence absent | Work goes to validation/recovery | Independent checks required; timeout gives attention, no success |
| Runtime/output/storage budget exceeded | Stop attempt; preserve bounded diagnostic evidence and truncation gap | Supported failed outcome after cleanup; no false claim of hard monetary cap |
| Duplicate submission/launch/decision | Same fingerprint returns prior receipt | Different fingerprint conflicts; no ID reuse; one effective decision/effect |
| Stale/revoked/expired approval | Reject before claim; resolve/supersede old attention and issue new review if appropriate | Fresh evidence and new decision; post-dispatch revocation cannot undo effect |
| Target/snapshot changes after review | Invalidate before dispatch; native CAS still guards race | New snapshot/review; no convenience override |
| Effect may succeed before result commit | Effect stays dispatching/unknown; no blind retry | Probe exact target/effect identity; confirmed result, proven no effect, or owner attention |
| Evidence disconnects / sequence gap | Mark coverage degraded immediately when known or deadline expires | Replay/rescan with disclosed coverage; no pass on missing/stale/wrong-artifact evidence |
| Notification send fails / owner offline | Attention persists, bounded retries and same-channel escalation | Delivery/ack/decision tracked separately; never resolve because retries exhausted |
| Send succeeds, receipt lost | Claim eventually expires | Repeat same attention ID; possible duplicate visible notification is acceptable |
| SQLite busy | Bounded wait/retry, no long external work under lock | Fail current command as unavailable/unknown as appropriate; no uncommitted ACK |
| Disk full / I/O / commit error | Freeze new launches/effects; stop hazardous running work by in-memory supervisor identity | Read back stable IDs after storage repair; unresolved effects reconciled before retry |
| Database corrupt/unreadable | Refuse normal startup; preserve DB/WAL; emergency diagnostics | Restore into separate path; verify and reconcile; never initialize over old state |
| Artifact missing/corrupt | Invalidate review and freeze effect | Recover verified copy or prepare/revalidate anew; evidence gap stays visible |
| Two coordinators / stale owner | Exclusive lock rejects second; supervisor rejects old epochs | No timestamp-only takeover; reconcile dispatched old commands |
| Forged local caller | Reject identity/role/schema mismatch | Audit without exposing secrets; M1 does not defend against full same-user compromise |
| Clock jump / suspend | Re-evaluate deadlines conservatively; stale UI/evidence marked | Expire uncertain authorization, reconcile elapsed work; never extend permit by clock rollback |
| Planned shutdown/update | Stop accepting; surface active impact; drain or stop and record; save attention | Restart reconciliation mandatory; schema change only after backup/quiescence |

## External-effect reconciliation algorithm

For each `dispatching` or `unknown` effect, first fence old executor ownership and determine whether an execution may still be in flight. A timeout is not proof it stopped. Query the effect's native idempotency record or inspect exact target state and preconditions through an authoritative adapter.

1. **Confirmed applied:** exact result and scope evidenced; persist result and proceed to final validation. Record whether operation attribution or only desired-state attainment is proven.
2. **Confirmed not applied and no in-flight executor:** persist evidence. Retry only if policy declares the action safe to retry, native preconditions still hold, and authorization is still valid. M1 requires a new review for a final-action retry after a failed/expired claim; the old review is not reset.
3. **Conflicting result:** record observed state; require owner attention/new review. No automatic rollback of unrelated changes.
4. **Unknown:** preserve the effect ID and evidence gap, assign attention, and do not dispatch again. An authorized owner may investigate, supply evidence, or abandon with explicit residual uncertainty. “Retry anyway” is a new explicit risk decision and is outside M1.

Fixture example: approval authorizes old ref `A` to new commit `B`. Crash after CAS but before SQLite write. On recovery, current `B` plus exclusive-writer evidence supports the fixture result; `A` plus proof no executor remains supports not applied; `C`, absent repository or inaccessible state is unresolved. If a shared writer could move `B` back to `A`, current `A` alone does not prove no prior effect. Real adapters need native idempotency keys, operation history or stronger reconciliation; otherwise ambiguity stays open.

## Database outage and emergency reporting

Database-dependent attention cannot be written when the database is unavailable. Do not pretend otherwise. Supervisor/coordinator send a minimal redacted emergency error to stderr/service journal and attempt a direct local notification using a stable incident ID. These are best-effort diagnostics, not durable accepted-work receipts. A preallocated small local emergency log can improve disk-full diagnostics, but is not a substitute for an independent observer and does not protect against disk loss.

Stop releasing new workloads. Supervisor enforces its already-known absolute deadlines and attempts cleanup without relying on a database commit. After storage returns, import any preserved diagnostics as explicitly secondary evidence, inspect stable IDs and reconcile. If no evidence survived, keep unknown status. If the database itself is lost, a single-host single-disk system cannot reconstruct every accepted request; independent receipt/backup storage is needed to strengthen that durability boundary. This risk must be accepted explicitly before operational use.

## Who notices the application is dead?

| Guarantee wanted | Minimum independent observation | Limit |
|---|---|---|
| Application failed while host works | Separate local watchdog/service manager observing a coordinator heartbeat that advances only after successful DB commit and obligation sweep; watchdog has its own delivery path | Same host/network/account can still fail together |
| Host or network unavailable | Observer on another host/service with missed-heartbeat deadline and independently stored contact/channel configuration | Can report absence, not distinguish power loss from network partition with certainty |
| Owner actually saw the alert | Explicit acknowledgment, retries and agreed escalation path/recipient | No notification API can guarantee human attention |
| Recover accepted ledger after disk loss | Verified independent backups and/or replicated acceptance receipts with stated recovery point | A heartbeat alone does not preserve the work ledger |

Proposed operational heartbeat: every 30 s, miss threshold 120 s; payload includes boot/instance, last successful obligation sweep, durable DB write age and overdue obligation count. Observer alarms on stale sequence as well as missing heartbeat; an alive but wedged process must not appear healthy. Observer test must kill the app, sever host connectivity and simulate DB write failure. Observer's own monitoring/delivery failure must be visible through its chosen provider or separate supervision. Planned maintenance has a bounded, explicit expiry.

**M1 implements no external observer and configures no service.** It proves local failure detection while the app/host can operate and recovery on next startup. It cannot meet host-outage notification guarantees. Before unattended real work, owner chooses an independent observer and channel or explicitly accepts delayed outage notice; recommended default is to require the observer. No one is contacted by this design task.

## Human closure rules

Coordinator may automatically close supported success, or supported deterministic failure after bounded retries and known cleanup. Such closure still creates an informational outcome attention item/outbox so failures are proactively surfaced; the work can be closed while that nonblocking outcome notice remains open. Blocking unknowns require owner attention and cannot auto-close. Rejection is an explicit review decision. Cancellation and abandonment require authenticated owner actions, reason and evidence summary. Abandonment with a live hazard must create a linked cleanup obligation atomically.
