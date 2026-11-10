# Minimum SQLite proposal and transaction boundaries

This is reviewable DDL, not an installed database migration. It deliberately stores current state plus immutable facts/events rather than implementing full event sourcing. One local database contains coordinator-owned tables and two supervisor-owned tables. Each process uses its own connection; SQLite serializes writes. No database transaction spans a process launch, notification send, validation subprocess or Git action.

## Storage setup

Set and read back `foreign_keys=ON`, `journal_mode=WAL`, `synchronous=FULL`, and `busy_timeout=1000` on applicable connections. Use a pinned supported SQLite build, record its version/compile options, and review upstream correctness fixes before shipping. No network filesystem. Local durability still depends on functioning filesystem, hardware and synchronization; database corruption or loss cannot be ruled out by schema.

Use `BEGIN IMMEDIATE` for state-changing transactions, version compare-and-swap for domain objects, and bounded retry of busy errors. Never retry an entire business operation merely because its response was lost. Open a fresh read to determine whether its stable ID committed first. See [SQLite transaction guarantees](https://www.sqlite.org/transactional.html), [pragma settings](https://www.sqlite.org/pragma.html), and [WAL operational constraints](https://www.sqlite.org/wal.html).

IDs are random opaque strings except caller-scoped deduplication keys; hashes are SHA-256 of a specified canonical encoding. Time values are integer Unix milliseconds, with separate boot/monotonic identity in observations. JSON columns below contain versioned canonical payloads validated at the application boundary. Payloads do not replace relational identities or guard columns.

## Proposed DDL

The creation-order forward references below are intentional and supported by SQLite. Insertion order respects foreign keys; cyclic work/attention links are deferred for atomic creation. Status transitions are checked in the domain layer; `CHECK` constraints catch invalid values, not all legal transitions.

```sql
CREATE TABLE coordinator_epoch (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  epoch INTEGER NOT NULL,
  instance_id TEXT NOT NULL,
  boot_id TEXT NOT NULL,
  committed_heartbeat_at INTEGER NOT NULL
);

CREATE TABLE permit (
  id TEXT PRIMARY KEY,
  version INTEGER NOT NULL,
  owner TEXT NOT NULL,
  scope_hash TEXT NOT NULL,
  scope_json TEXT NOT NULL,
  issued_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  revoked_at INTEGER,
  CHECK(expires_at > issued_at)
);

CREATE TABLE work (
  id TEXT PRIMARY KEY,
  principal TEXT NOT NULL,
  request_key TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  request_json TEXT NOT NULL,
  permit_id TEXT NOT NULL REFERENCES permit(id),
  mode TEXT NOT NULL CHECK(mode IN ('active','attention','closed')),
  phase TEXT NOT NULL CHECK(phase IN
    ('queued','preparing','validating','reviewing','applying','reconciling','blocked','finalizing','done')),
  version INTEGER NOT NULL DEFAULT 1,
  next_owner TEXT,
  next_action TEXT,
  due_at INTEGER,
  attention_id TEXT REFERENCES attention(id) DEFERRABLE INITIALLY DEFERRED,
  outcome TEXT CHECK(outcome IN ('succeeded','failed','rejected','cancelled','abandoned')),
  closure_evidence_id TEXT REFERENCES evidence(id),
  closure_reason TEXT,
  closed_by TEXT,
  accepted_at INTEGER NOT NULL,
  closed_at INTEGER,
  parent_work_id TEXT REFERENCES work(id),
  UNIQUE(principal, request_key),
  CHECK(
    (mode IN ('active','attention') AND phase <> 'done' AND
      next_owner IS NOT NULL AND next_action IS NOT NULL AND due_at IS NOT NULL AND
      outcome IS NULL AND closed_at IS NULL AND closure_evidence_id IS NULL AND
      closure_reason IS NULL AND closed_by IS NULL AND
      ((mode='active' AND attention_id IS NULL) OR
       (mode='attention' AND attention_id IS NOT NULL)))
    OR
    (mode='closed' AND phase='done' AND next_owner IS NULL AND next_action IS NULL AND
      due_at IS NULL AND attention_id IS NULL AND outcome IS NOT NULL AND
      closure_evidence_id IS NOT NULL AND closure_reason IS NOT NULL AND
      closed_by IS NOT NULL AND closed_at IS NOT NULL)
  )
);

CREATE TABLE attempt (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL REFERENCES work(id),
  permit_id TEXT NOT NULL REFERENCES permit(id),
  ordinal INTEGER NOT NULL,
  launch_request_id TEXT NOT NULL UNIQUE,
  spec_hash TEXT NOT NULL,
  spec_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN
    ('intent','accepted','starting','running','stop_requested','stopped','exited','cleaned','start_failed','unknown')),
  coordinator_epoch INTEGER NOT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deadline_at INTEGER NOT NULL,
  last_evidence_id TEXT REFERENCES evidence(id),
  UNIQUE(work_id, ordinal)
);
CREATE UNIQUE INDEX one_live_attempt_per_work ON attempt(work_id)
  WHERE state NOT IN ('cleaned','start_failed');

CREATE TABLE artifact (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL REFERENCES work(id),
  manifest_hash TEXT NOT NULL,
  manifest_json TEXT NOT NULL,
  content_uri TEXT NOT NULL,
  sealed_at INTEGER NOT NULL,
  UNIQUE(work_id, manifest_hash)
);

CREATE TABLE evidence (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL REFERENCES work(id),
  attempt_id TEXT REFERENCES attempt(id),
  artifact_id TEXT REFERENCES artifact(id),
  source TEXT NOT NULL,
  source_epoch TEXT NOT NULL,
  source_seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  observed_at INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  valid_until INTEGER,
  coverage TEXT NOT NULL CHECK(coverage IN ('complete','partial','gap','unknown')),
  payload_hash TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  UNIQUE(source, source_epoch, source_seq)
);

CREATE TABLE review (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL REFERENCES work(id),
  artifact_id TEXT NOT NULL REFERENCES artifact(id),
  work_version INTEGER NOT NULL,
  envelope_hash TEXT NOT NULL,
  envelope_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN
    ('pending','authorized','rejected','expired','invalidated','revoked','consumed')),
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX one_open_review_per_work ON review(work_id)
  WHERE state IN ('pending','authorized');
CREATE TABLE review_evidence (
  review_id TEXT NOT NULL REFERENCES review(id),
  evidence_id TEXT NOT NULL REFERENCES evidence(id),
  PRIMARY KEY(review_id, evidence_id)
);
CREATE TABLE decision (
  id TEXT PRIMARY KEY,
  review_id TEXT NOT NULL UNIQUE REFERENCES review(id),
  actor TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  envelope_hash TEXT NOT NULL,
  expected_work_version INTEGER NOT NULL,
  choice TEXT NOT NULL CHECK(choice IN ('approve','reject')),
  decided_at INTEGER NOT NULL
);

CREATE TABLE effect (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL REFERENCES work(id),
  review_id TEXT NOT NULL UNIQUE REFERENCES review(id),
  decision_id TEXT NOT NULL UNIQUE REFERENCES decision(id),
  action_kind TEXT NOT NULL,
  action_hash TEXT NOT NULL,
  action_json TEXT NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  state TEXT NOT NULL CHECK(state IN
    ('intent','dispatching','confirmed','not_applied','unknown','cancelled')),
  coordinator_epoch INTEGER NOT NULL,
  dispatched_at INTEGER,
  result_evidence_id TEXT REFERENCES evidence(id)
);

CREATE TABLE attention (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL REFERENCES work(id),
  review_id TEXT REFERENCES review(id),
  reason TEXT NOT NULL,
  severity TEXT NOT NULL CHECK(severity IN ('info','action','urgent')),
  state TEXT NOT NULL CHECK(state IN ('open','acknowledged','resolved')),
  recipient TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  acknowledged_by TEXT,
  acknowledged_at INTEGER,
  followup_at INTEGER NOT NULL,
  resolved_at INTEGER,
  resolution TEXT,
  CHECK((state='resolved' AND resolved_at IS NOT NULL AND resolution IS NOT NULL)
     OR (state<>'resolved' AND resolved_at IS NULL AND resolution IS NULL))
);
CREATE TABLE outbox (
  id TEXT PRIMARY KEY,
  attention_id TEXT NOT NULL UNIQUE REFERENCES attention(id),
  channel TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('pending','delivering','retry_wait','delivered','cancelled')),
  generation INTEGER NOT NULL DEFAULT 1,
  tries INTEGER NOT NULL DEFAULT 0,
  next_try_at INTEGER NOT NULL,
  claim_epoch INTEGER,
  claim_until INTEGER,
  last_error TEXT,
  receipt_json TEXT
);

CREATE TABLE recovery (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL REFERENCES work(id),
  attempt_id TEXT REFERENCES attempt(id),
  effect_id TEXT REFERENCES effect(id),
  state TEXT NOT NULL CHECK(state IN ('needed','probing','attention','resolved')),
  reason TEXT NOT NULL,
  next_probe_at INTEGER NOT NULL,
  deadline_at INTEGER NOT NULL,
  result TEXT CHECK(result IN
    ('running','confirmed_succeeded','confirmed_failed','confirmed_not_started','unknown')),
  evidence_id TEXT REFERENCES evidence(id)
);

CREATE TABLE event (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  work_id TEXT REFERENCES work(id),
  actor TEXT NOT NULL,
  kind TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  at INTEGER NOT NULL,
  payload_json TEXT NOT NULL
);
CREATE TRIGGER event_no_update BEFORE UPDATE ON event
  BEGIN SELECT RAISE(ABORT,'events are append-only'); END;
CREATE TRIGGER event_no_delete BEFORE DELETE ON event
  BEGIN SELECT RAISE(ABORT,'events are append-only'); END;

-- Supervisor-owned records. Separate transactions, same database.
CREATE TABLE supervisor_command (
  request_id TEXT PRIMARY KEY,
  attempt_id TEXT NOT NULL REFERENCES attempt(id),
  kind TEXT NOT NULL CHECK(kind IN ('launch','stop')),
  fingerprint TEXT NOT NULL,
  coordinator_epoch INTEGER NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('accepted','processing','settled','unknown')),
  accepted_at INTEGER NOT NULL,
  response_json TEXT
);
CREATE UNIQUE INDEX one_supervisor_launch ON supervisor_command(attempt_id)
  WHERE kind='launch';
CREATE TABLE supervisor_fact (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  attempt_id TEXT NOT NULL REFERENCES attempt(id),
  command_id TEXT NOT NULL REFERENCES supervisor_command(request_id),
  supervisor_epoch TEXT NOT NULL,
  kind TEXT NOT NULL,
  observed_at INTEGER NOT NULL,
  identity_json TEXT NOT NULL,
  payload_json TEXT NOT NULL
);
CREATE INDEX work_due ON work(mode, due_at);
CREATE INDEX outbox_due ON outbox(state, next_try_at);
CREATE INDEX recovery_due ON recovery(state, next_probe_at);
```

This is a minimum for the specified guarantees, not a universal agent platform. Avoid task graphs, plugin registries, transcript indexing, token-price catalogs or hash-chain infrastructure. `event` stores rejected/conflicting requests, permit revocations, attention delivery attempts, closure decisions and reconciliation explanations. Full transcripts remain bounded optional artifacts, never required for basic accounting.

## Constraints outside DDL

The transaction layer must verify cross-row relationships: work/attempt/artifact/evidence/review/effect IDs belong together; the attention pointer is unresolved and belongs to the work; approved decision matches its review envelope; closure evidence contains the supported outcome and required checks; every consumed review has exactly one effect; live attempts cannot coexist with terminal work except a linked cleanup obligation explicitly accepted at abandonment. Use immutable inserts for evidence, artifacts, decisions and review envelopes; only review lifecycle status mutates. Permit content is immutable by ID; a changed scope creates a new ID/version. Revocation is a separate timestamp/event.

Add property tests for those constraints and negative SQL tests for basic checks. Direct DB access is not an external API; both trusted processes remain capable of corrupting it through bugs. Table ownership is code discipline in M1, not SQLite role-based authorization.

## Transaction map

| Boundary | Atomic writes | Commit acknowledgment / next action |
|---|---|---|
| T1 Accept | Dedup lookup by principal/key; insert work with responsibility and accepted event | Return accepted ID/version only after commit; duplicate same hash returns original |
| T2 Prepare launch | Validate permit; insert attempt intent + event; advance work deadline | After commit send supervisor command |
| S1 Accept launch | Check committed attempt/spec/epoch; insert supervisor command | `Accepted` means durable receipt, not started |
| S2 Record lifecycle fact | Append exact identity/start/exit/cleanup fact; update command receipt | Report facts as durable only after commit |
| T3 Ingest evidence | Dedup source sequence; insert evidence + event; update attempt/work and next responsibility | ACK cursor only after commit; then adapter may discard replay buffer |
| T4 Create review | Insert artifact references, review, evidence links, attention, outbox; work to attention | No gap in which review exists without attention |
| T5 Decide | Check duplicate first, then version/hash/scope/expiry; insert decision; update review; resolve review attention; approved effect intent or rejected finalization; work next action | Decision accepted means recorded, not executed |
| T6 Claim effect | Recheck authorization and dispatch conditions; mark review consumed + effect dispatching + event | Commit before effect; executor works outside transaction |
| T7 Record effect | Insert independently obtained result evidence; mark effect confirmed/not_applied/unknown; update work/recovery/attention | Never mark success solely on action response |
| T8 Attention delivery | Claim due row in short transaction; send outside; store receipt/retry and event in second transaction | Lost send response may cause duplicate; same attention ID |
| T9 Close | Verify cleanup, evidence, no unresolved effects/recovery; resolve blocking attention/outbox; write outcome and closure event; insert a separate nonblocking outcome attention/outbox | Closed receipt is durable accounting, not broader project acceptance; outcome notice may remain unacknowledged |
| T10 Recover | Classify known IDs/facts; create recovery cases; assign each unresolved work a responsibility or attention | Re-enable new launches only after census/fencing checks |

Immutable artifact files are written to a temporary directory, synchronized, renamed into a content-addressed store and directory-synchronized before their database reference commits. On restart, referenced missing/corrupt artifacts invalidate reviews and raise attention. Unreferenced complete files are harmless orphans, retained for a bounded grace period before garbage collection. The files and database are not one atomic transaction.

## Ownership and fencing

Coordinator holds an exclusive host lock for its lifetime, then increments durable epoch at startup. Supervisor has its own exclusive lock. A new owner cannot take over merely because a timestamp expired: prove old process gone/lock released and reconcile in-flight operations. Supervisor rejects old-epoch mutations and verifies attempt intent in the database. Accepted old-epoch launches must be reconciled and stopped or re-authorized under the current owner before releasing a blocked wrapper. Epoch checks do not retract a command already dispatched; that operation stays unresolved until observed.

## Retention and restore

Retain all unresolved work, decisions, effect identities and referenced evidence/artifacts. Default completed-work metadata retention: indefinite for the prototype; bounded raw logs: 7 days, 10 MiB per attempt. Redact secrets before persisting; recording “redacted” must not erase required artifact identity or gap information. No raw credential values in hashes, notifications or event payloads.

Before migrations, quiesce launches, reconcile active work, create a verified snapshot and matching artifact manifest; refuse unknown/newer schema. Use SQLite's [online backup API](https://www.sqlite.org/backup.html). Restore to a separate location, verify integrity/foreign keys/artifact hashes, then reconcile external effects before resuming. Restored state may forget decisions/effects after the snapshot: it cannot justify replay. Preserve surviving database/WAL/receipts for investigation; never automatically create an empty database over corruption. A local backup does not survive host/disk loss. Off-host durable receipts/backup are a later owner decision tied to the outage guarantee.
