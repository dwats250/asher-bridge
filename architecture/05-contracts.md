# Component contracts and acknowledgment semantics

All interfaces below are proposed version 1. Transport errors and application errors differ. Requests carry protocol version, request ID, authenticated actor/role, body fingerprint and expected entity version where relevant. Reject unknown versions, oversize bodies and unknown action types. Agent-generated IDs cannot impersonate owner requests.

## Acceptance and standing permits

`SubmitWork(principal, request_key, scope, success_criteria, source_evidence, permit_id)` returns one of:

- `Accepted(work_id, version, committed_at, next_owner, due_at)`: T1 committed. The durable accounting obligation exists, even if no response reaches the caller.
- `Existing(work_id, version, original_request_hash)`: same principal/key and canonical body already committed. A changed payload with the same key is `Conflict`, not another work item.
- `Rejected(reason)`: known not accepted. A timeout or unknown commit outcome is **not** a rejection; caller queries/retries the same key.
- `Unavailable(acceptance_unknown=true)`: storage/readback cannot establish commit status; retain caller's request ID and reconcile when service returns.

The client must generate/persist its submission key before sending. Request keys are stable per finding, not per retry; distinct findings can be separate work. Internal content similarity does not silently merge obligations.

Preparation authority and final-action authority are distinct: the preparation permit never grants the ref update. An exact owner approval grants that one typed action; dispatch checks both that approval and any applicable parent-scope restrictions/revocations.

Permit fields: immutable ID/version, issuer, allowed work/action classes, fixture root identity, allowed paths, executable identities, network/credential policy, maximum attempts, concurrency, runtime, output/storage ceilings, issuance/expiry, revocation and explicit prohibition on final-action authority. M1 permits only trusted fake preparation and validator in fixture roots. Consuming a retry/capacity reservation is transactional with attempt creation. Supervisor rechecks a permit's execution deadline before release; revocation triggers stop for active attempts, with a visible cleanup result.

The permit is coordinator authorization, not isolation. A real permit is valid only with an enforcement profile whose capability checks passed. A launch is refused if required enforcement is unavailable. Estimated token/spend limits are advisory unless enforced by a provider; use local runtime, launch count and concurrency limits as the actual M1 bounds.

## Supervisor API

Minimal commands:

```text
Launch(request_id, attempt_id, epoch, spec_hash)
GetAttempt(attempt_id)
ListOwned(since_fact_seq) -> facts, complete_census, census_time, gaps
Stop(request_id, attempt_id, epoch, reason, grace_ms, force_deadline_ms)
ReadFacts(after_seq, limit)
```

Launch reads the immutable persisted spec; arbitrary shell strings are not accepted. Spec includes a typed purpose (`prepare`, `validate`, or explicitly approved `effect_helper`), optional effect ID, executable identity, argv array, scrubbed environment profile, fixture directory identity, control nonce, absolute runtime/output ceilings and containment mode. Read facts is observation-only. No attach-write or freeform `Send` in M1.

| Response | Guarantee | Does not guarantee |
|---|---|---|
| `Accepted` | Command fingerprint and attempt reference committed | Child exists, running, or successful |
| `Starting` | Startup operation is in progress for this identity | Agent workload released |
| `Started` | Trusted wrapper identified process; output channels ready; durable identity receipt recorded; workload release initiated | Useful progress or final work success; release/result may be uncertain after a crash |
| `ExitObserved` | Exact parent identity exit/signal observed and persisted | Descendants absent, output complete or work successful |
| `StopAccepted` | Stop request recorded; cleanup owns a deadline | Any process has stopped |
| `StopConfirmed(scope)` | Fresh census establishes no members in the stated supported scope, output transport closed, receipt persisted | Whole-descendant absence beyond the declared containment capability |
| `Unknown(reason, coverage)` | Cannot establish requested fact; caller must reconcile | Absence, failure, or permission to relaunch |
| `NotStarted` | Durable receipt plus full startup/containment reconciliation proves workload never released and no processes remain | Safe replay of an unrelated external effect |

Facts observed while DB writes fail are marked `volatile` and are not durable acknowledgments. Supervisor still stops on runtime/control loss using in-memory identity. It emits emergency diagnostics and retries fact persistence; restart may leave an unknown exit. Safety cleanup must not wait indefinitely for SQLite.

### Launch and partial startup algorithm

1. Verify authenticated peer, role, current epoch, unique command/body, committed attempt/spec and remaining permit. Commit command acceptance before any child creation.
2. Establish named startup/containment identity. Prepare output capture and the trusted wrapper; wrapper starts blocked, receives attempt nonce and sends PID/start/boot identity. It must not run the fake payload before release. Durable command already exists if a child appears before its receipt.
3. Persist wrapper identity and readiness facts. Verify current epoch/permit once more. Release the wrapper and observe readiness; store the resulting fact. A crash between release and recording is **possibly running**, not “did not start.”
4. Any setup failure after process creation requests stop, closes release/control channels and waits for supported scope emptiness. If not proved within deadline, record unknown + urgent attention. Never fall back to an unsupervised process.
5. Launch retry with same identity inspects its command/facts/census. It returns current receipt; it never silently creates a new process. Historical identity reuse is forbidden even after exit.

A group-only wrapper cannot establish absence after every supervisor crash or malicious escape. M1 therefore fails closed into attention when proof is missing. Real containment must persist a discoverable attempt scope independent of supervisor memory. A PID alone, an environment tag, or “no process in this group” is insufficient proof of global descendant absence.

## Evidence adapter API

```text
Observation {
  evidence_id, source, source_epoch, sequence,
  work_id, attempt_id?, artifact_id?, kind,
  observed_at, received_at, boot_id?, monotonic_time?,
  valid_until?, covered_from?, covered_to?,
  coverage: complete|partial|gap|unknown,
  gap_reason?, payload, payload_hash, producer_version
}
IngestObservation(...) -> Committed(evidence_id, cursor) | Duplicate | Conflict | Unavailable
GetCoverage(source, scope, interval) -> last_observation, gaps, freshness, provenance
```

Freshness is evaluated by the coordinator under the rule for that evidence type; a producer cannot make its evidence eternally fresh. Missing sequence numbers, restarted source epochs, unavailable scans and truncated logs produce explicit gaps. Duplicate `(source, epoch, sequence)` with identical payload is harmless; a changed payload is a conflict that degrades trust and raises attention. ACK after commit lets adapters replay safely; lost ACK results in replay, not data loss.

Evidence types for M1: initial defect reproduction, candidate manifest, validator check result, supervisor lifecycle fact, containment census, fixture-ref result and attention delivery receipt. Agent self-report is labeled a claim. Trusted validation reruns known checks independently; no JSON message from the agent can masquerade as a validator or supervisor source.

A test pass on an immutable snapshot remains associated with those bytes, but dispatch can still require current base/ref state, current policy and unexpired environmental observations. Fresh heartbeat says an adapter is reachable; it does not prove data coverage or task progress. A directory scan must disclose exclusions; incomplete scans cannot prove absence.

## Exact review envelope

Canonical versioned envelope contains:

- Work and review IDs, success criteria version, permit ID/version and policy version.
- Typed action `fixture.ref_compare_swap`, repository identity, exact target ref, expected old object ID, proposed commit ID and action parameters.
- SHA-256 manifest of frozen file bytes, relative paths, modes and allowed symlink targets; include dirty/untracked content by snapshot or reject it. Reject unsupported submodules/symlinks in M1. Git commit identity alone is insufficient.
- Base revision, trusted validator identity/version, validation commands/configuration, environment/dependency fingerprints, required evidence IDs and hashes, coverage and known gaps.
- Approver role, expiry and explicit scope (“update this disposable fixture ref only”).

Stop all candidate writers; create a fresh sealed snapshot; verify its hash before and after validation. The trusted executor uses that snapshot and a native compare-and-swap on the ref. A shared mutable checkout is not the reviewed artifact. UI renders normalized action and digest plus diff/checks; logs cannot override this summary. Hashing alone is insufficient if an agent can modify the snapshot or executor environment; real adapter gate requires access controls.

`Decide(decision_id, review_id, envelope_hash, expected_work_version, choice)` is accepted only for an authenticated authorized principal and live exact review. Replay lookup precedes current-state validation so a retry returns the original decision. New decisions require current state. Review version represents semantic review changes; heartbeat/delivery bookkeeping does not gratuitously bump it. Changed evidence/preconditions require invalidation and a new review.

For fixture action, hold an exclusive fixture mutation lock and verify the canonical repository identity, old ref and new artifact. Compare-and-swap failure means no ref change by this operation. Readback of the exact desired ref is required for success. If it is already desired during recovery, report observed desired state; attribute it to this operation only when exclusive-writer/audit evidence supports that. In a future shared project, coincidental matching state alone may be insufficient.

## Attention contract and device limits

M1: coordinator persists inbox entry and outbox in T4. A local desktop notifier emits a short redacted message linking to owner CLI/inbox. It returns a local delivery receipt only if the delivery API confirms its documented acceptance; absence of a desktop session means retryable failure. In tests, a fake sink supports deterministic lost/duplicate/delayed receipts.

Attention payload contains item ID, work ID, reason, urgency, deadline, freshness and review ID when applicable. It excludes secrets and broad action authority. Offline clients see last-sync age and cannot queue blind approval of an expired review. Reconnecting sends the exact decision against the original envelope; coordinator may reject it. Future watch acknowledgment cannot be interpreted as approval. Phone approval requires the same envelope display and authenticated identity as desktop; device convenience cannot weaken the boundary.
