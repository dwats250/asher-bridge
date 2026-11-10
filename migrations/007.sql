-- Gate 6 M1: durable recovery accounting, imported storage incidents and restore records.
-- Additive only: creates new tables, indexes and triggers. No existing table, index, trigger or
-- row is changed. Run under the coordinator lock inside one IMMEDIATE transaction.

-- One row per recovery pass (every mutating startup, an explicit `recover`, or the verification
-- of a restored ledger). A run is recorded 'started' before it examines anything and finishes
-- exactly once. A run that never finishes stays 'started' and is marked 'interrupted' by the next
-- run; nothing ever claims completion for it.
CREATE TABLE recovery_run (
 id TEXT PRIMARY KEY,
 coordinator_epoch INTEGER NOT NULL,
 trigger TEXT NOT NULL CHECK(trigger IN ('startup','explicit','restore')),
 state TEXT NOT NULL CHECK(state IN ('started','completed','partial','failed','interrupted')),
 started_at INTEGER NOT NULL,
 finished_at INTEGER,
 examined INTEGER NOT NULL DEFAULT 0 CHECK(examined>=0),
 unresolved INTEGER NOT NULL DEFAULT 0 CHECK(unresolved>=0),
 summary_json TEXT,
 CHECK((state='started' AND finished_at IS NULL AND summary_json IS NULL)
    OR (state<>'started' AND finished_at IS NOT NULL AND summary_json IS NOT NULL))
);
CREATE INDEX recovery_run_by_start ON recovery_run(started_at);
CREATE TRIGGER recovery_run_starts_started BEFORE INSERT ON recovery_run WHEN NEW.state<>'started'
 BEGIN SELECT RAISE(ABORT,'a recovery run is recorded started before it examines anything'); END;
CREATE TRIGGER recovery_run_identity_immutable BEFORE UPDATE OF id,coordinator_epoch,trigger,started_at ON recovery_run
 BEGIN SELECT RAISE(ABORT,'recovery run identity is immutable'); END;
CREATE TRIGGER recovery_run_finishes_once BEFORE UPDATE ON recovery_run
 WHEN NOT (OLD.state='started' AND NEW.state IN ('completed','partial','failed','interrupted'))
 BEGIN SELECT RAISE(ABORT,'a recovery run finishes exactly once'); END;
CREATE TRIGGER recovery_run_complete_requires_items BEFORE UPDATE OF state ON recovery_run
 WHEN NEW.state='completed'
  AND NEW.examined <> (SELECT count(*) FROM recovery_item WHERE run_id=NEW.id)
 BEGIN SELECT RAISE(ABORT,'a run completes only with every examined obligation recorded'); END;
CREATE TRIGGER recovery_run_no_delete BEFORE DELETE ON recovery_run
 BEGIN SELECT RAISE(ABORT,'recovery history is retained'); END;

-- Cases a run could not resolve, with the responsibility they are assigned to. Open at most once
-- per subject; resolved only by a later run that observes the condition no longer holds.
CREATE TABLE recovery_case (
 id TEXT PRIMARY KEY,
 subject_key TEXT NOT NULL CHECK(length(subject_key)>0),
 subject_kind TEXT NOT NULL CHECK(subject_kind IN
  ('work','attempt','effect','review','artifact','attention','storage_incident','restore')),
 subject_id TEXT NOT NULL,
 work_id TEXT REFERENCES work(id),
 reason TEXT NOT NULL CHECK(length(reason)>0),
 state TEXT NOT NULL CHECK(state IN ('open','resolved')),
 responsible TEXT NOT NULL CHECK(length(responsible)>0),
 required_action TEXT NOT NULL CHECK(length(required_action)>0),
 due_at INTEGER,
 attention_id TEXT REFERENCES attention(id),
 opened_run_id TEXT NOT NULL REFERENCES recovery_run(id),
 opened_at INTEGER NOT NULL,
 observation_json TEXT NOT NULL,
 resolved_run_id TEXT REFERENCES recovery_run(id),
 resolved_at INTEGER,
 resolution TEXT,
 CHECK((state='open' AND resolved_at IS NULL AND resolution IS NULL AND resolved_run_id IS NULL)
    OR (state='resolved' AND resolved_at IS NOT NULL AND length(resolution)>0 AND resolved_run_id IS NOT NULL))
);
CREATE UNIQUE INDEX recovery_case_one_open ON recovery_case(subject_key) WHERE state='open';
CREATE INDEX recovery_case_by_work ON recovery_case(work_id,state);
CREATE TRIGGER recovery_case_identity_immutable BEFORE UPDATE OF id,subject_key,subject_kind,subject_id,work_id,reason,opened_run_id,opened_at,observation_json ON recovery_case
 BEGIN SELECT RAISE(ABORT,'recovery case identity is immutable'); END;
CREATE TRIGGER recovery_case_opens_open BEFORE INSERT ON recovery_case WHEN NEW.state<>'open'
 BEGIN SELECT RAISE(ABORT,'a recovery case is opened before it can be resolved'); END;
CREATE TRIGGER recovery_case_resolves_once BEFORE UPDATE OF state ON recovery_case
 WHEN NOT (OLD.state='open' AND NEW.state='resolved')
 BEGIN SELECT RAISE(ABORT,'a recovery case resolves exactly once'); END;
-- Only a recovery pass that is still running (and then finishes in the same transaction) may
-- resolve a case, and uncertainty a restored ledger inherited from its backup is never resolved.
CREATE TRIGGER recovery_case_resolved_by_running_pass BEFORE UPDATE OF state ON recovery_case
 WHEN NEW.state='resolved'
  AND ((SELECT state FROM recovery_run WHERE id=NEW.resolved_run_id) IS NOT 'started'
       OR OLD.subject_key LIKE 'restore:%')
 BEGIN SELECT RAISE(ABORT,'a case is resolved only by a running pass, never for restore uncertainty'); END;
CREATE TRIGGER recovery_case_resolved_immutable BEFORE UPDATE ON recovery_case WHEN OLD.state='resolved'
 BEGIN SELECT RAISE(ABORT,'resolved recovery case is immutable'); END;
CREATE TRIGGER recovery_case_no_delete BEFORE DELETE ON recovery_case
 BEGIN SELECT RAISE(ABORT,'recovery cases are retained'); END;

-- Every obligation a run examined and how it classified it. Append-only.
CREATE TABLE recovery_item (
 run_id TEXT NOT NULL REFERENCES recovery_run(id),
 seq INTEGER NOT NULL CHECK(seq>0),
 subject_kind TEXT NOT NULL CHECK(subject_kind IN
  ('work','attempt','effect','review','artifact','attention','storage_incident','restore')),
 subject_id TEXT NOT NULL,
 work_id TEXT REFERENCES work(id),
 classification TEXT NOT NULL CHECK(classification IN ('accounted','unresolved','unclassifiable')),
 observation_json TEXT NOT NULL,
 case_id TEXT REFERENCES recovery_case(id),
 PRIMARY KEY(run_id,seq),
 CHECK(classification='accounted' OR case_id IS NOT NULL)
);
CREATE INDEX recovery_item_by_subject ON recovery_item(subject_kind,subject_id);
CREATE TRIGGER recovery_item_immutable BEFORE UPDATE ON recovery_item
 BEGIN SELECT RAISE(ABORT,'recovery items are immutable'); END;
CREATE TRIGGER recovery_item_no_delete BEFORE DELETE ON recovery_item
 BEGIN SELECT RAISE(ABORT,'recovery items are retained'); END;
CREATE TRIGGER recovery_item_only_while_started BEFORE INSERT ON recovery_item
 WHEN (SELECT state FROM recovery_run WHERE id=NEW.run_id) IS NOT 'started'
 BEGIN SELECT RAISE(ABORT,'items are recorded only by a running recovery pass'); END;

-- A ledger produced by `restore` from a backup. It (or a snapshot marker) makes the ledger
-- inspect-only: ordinary startup, launches and effect dispatch are refused, because external
-- actions may have happened after the backup and the restored rows cannot show them.
-- Restoration authorizes no replay, resume or repair; this slice provides no way to lift it.
CREATE TABLE ledger_restore (
 id TEXT PRIMARY KEY,
 backup_manifest_json TEXT NOT NULL,
 backup_manifest_sha256 TEXT NOT NULL CHECK(length(backup_manifest_sha256)=64),
 backup_ledger_sha256 TEXT NOT NULL CHECK(length(backup_ledger_sha256)=64),
 source_root TEXT NOT NULL,
 backup_created_at INTEGER NOT NULL,
 backup_schema INTEGER NOT NULL,
 restored_at INTEGER NOT NULL,
 verification_json TEXT NOT NULL
);
-- Written into every backup copy (never into the source). Any ledger derived from a backup, by
-- `restore` or by copying the file, carries it and is therefore fenced and inspect-only.
CREATE TABLE ledger_snapshot (
 id TEXT PRIMARY KEY,
 source_root TEXT NOT NULL,
 source_schema INTEGER NOT NULL,
 created_at INTEGER NOT NULL
);
CREATE TRIGGER ledger_snapshot_immutable BEFORE UPDATE ON ledger_snapshot
 BEGIN SELECT RAISE(ABORT,'snapshot marker is immutable'); END;
CREATE TRIGGER ledger_snapshot_no_delete BEFORE DELETE ON ledger_snapshot
 BEGIN SELECT RAISE(ABORT,'snapshot marker is retained'); END;
CREATE TRIGGER ledger_restore_immutable BEFORE UPDATE ON ledger_restore
 BEGIN SELECT RAISE(ABORT,'restore record is immutable'); END;
CREATE TRIGGER ledger_restore_no_delete BEFORE DELETE ON ledger_restore
 BEGIN SELECT RAISE(ABORT,'restore record is retained'); END;
UPDATE schema_meta SET version=7 WHERE singleton=1;
