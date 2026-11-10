-- Run under the coordinator lock, with foreign_keys disabled outside this transaction.
CREATE TABLE work_v2 (
 id TEXT PRIMARY KEY, principal TEXT NOT NULL, request_key TEXT NOT NULL,
 request_hash TEXT NOT NULL, request_json TEXT NOT NULL,
 permit_id TEXT NOT NULL REFERENCES permit(id),
 mode TEXT NOT NULL CHECK(mode='active'),
 phase TEXT NOT NULL CHECK(phase IN ('queued','preparing','reconciling','blocked')),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>0),
 next_owner TEXT NOT NULL CHECK(length(next_owner)>0),
 next_action TEXT NOT NULL CHECK(length(next_action)>0), due_at INTEGER NOT NULL,
 accepted_at INTEGER NOT NULL, CHECK(due_at>=accepted_at), UNIQUE(principal,request_key)
);
INSERT INTO work_v2 SELECT * FROM work;
DROP TABLE work;
ALTER TABLE work_v2 RENAME TO work;
CREATE TRIGGER work_request_immutable BEFORE UPDATE OF principal,request_key,request_hash,request_json,permit_id,accepted_at ON work
 BEGIN SELECT RAISE(ABORT,'accepted request is immutable'); END;
CREATE TRIGGER work_no_delete BEFORE DELETE ON work BEGIN SELECT RAISE(ABORT,'accepted work cannot be deleted'); END;
CREATE INDEX work_due ON work(due_at);
CREATE TABLE owner_session (
 epoch INTEGER PRIMARY KEY, pid INTEGER NOT NULL, start TEXT NOT NULL, boot TEXT NOT NULL
);
CREATE TABLE attempt (
 id TEXT PRIMARY KEY, work_id TEXT NOT NULL REFERENCES work(id),
 permit_id TEXT NOT NULL REFERENCES permit(id), spec_hash TEXT NOT NULL, spec_json TEXT NOT NULL,
 fixture_path TEXT NOT NULL, fixture_dev INTEGER NOT NULL, fixture_ino INTEGER NOT NULL,
 coordinator_epoch INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('intent','accepted','starting','running','stop_requested','exited','stopped','cleaned','unknown')),
 reason TEXT NOT NULL, created_at INTEGER NOT NULL, deadline_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX one_live_attempt ON attempt(work_id) WHERE state <> 'cleaned';
CREATE TRIGGER attempt_identity_immutable BEFORE UPDATE OF id,work_id,permit_id,spec_hash,spec_json,fixture_path,fixture_dev,fixture_ino,created_at,deadline_at ON attempt
 BEGIN SELECT RAISE(ABORT,'attempt identity is immutable'); END;
CREATE TRIGGER attempt_no_delete BEFORE DELETE ON attempt BEGIN SELECT RAISE(ABORT,'attempt history retained'); END;
CREATE TABLE supervisor_command (
 request_id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL UNIQUE REFERENCES attempt(id),
 fingerprint TEXT NOT NULL, coordinator_epoch INTEGER NOT NULL,
 supervisor_pid INTEGER NOT NULL, supervisor_start TEXT NOT NULL, supervisor_boot TEXT NOT NULL,
 accepted_at INTEGER NOT NULL
);
CREATE TRIGGER command_no_update BEFORE UPDATE ON supervisor_command BEGIN SELECT RAISE(ABORT,'receipt immutable'); END;
CREATE TRIGGER command_no_delete BEFORE DELETE ON supervisor_command BEGIN SELECT RAISE(ABORT,'receipt retained'); END;
CREATE TABLE supervisor_fact (
 seq INTEGER PRIMARY KEY AUTOINCREMENT, attempt_id TEXT NOT NULL REFERENCES attempt(id),
 kind TEXT NOT NULL, at INTEGER NOT NULL, payload_json TEXT NOT NULL
);
CREATE TRIGGER fact_no_update BEFORE UPDATE ON supervisor_fact BEGIN SELECT RAISE(ABORT,'fact immutable'); END;
CREATE TRIGGER fact_no_delete BEFORE DELETE ON supervisor_fact BEGIN SELECT RAISE(ABORT,'fact retained'); END;
UPDATE schema_meta SET version=2 WHERE singleton=1;
