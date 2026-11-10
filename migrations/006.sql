-- Gate 5: durable attention, delivery history, owner commands, outcomes and closure.
-- Run under the coordinator lock inside one IMMEDIATE transaction, with foreign_keys
-- disabled outside it. Every Task 1-5 row is preserved unchanged.

-- Closed work: mode 'closed', phase 'done', sentinels in the NOT NULL responsibility
-- columns (no live responsibility), immutable once closed. The outcome lives in work_outcome.
CREATE TABLE work_v6 (
 id TEXT PRIMARY KEY, principal TEXT NOT NULL, request_key TEXT NOT NULL,
 request_hash TEXT NOT NULL, request_json TEXT NOT NULL,
 permit_id TEXT NOT NULL REFERENCES permit(id),
 mode TEXT NOT NULL CHECK(mode IN ('active','closed')),
 phase TEXT NOT NULL CHECK(phase IN ('queued','preparing','reconciling','blocked','reviewing','applying','finalizing','done')),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>0),
 next_owner TEXT NOT NULL CHECK(length(next_owner)>0),
 next_action TEXT NOT NULL CHECK(length(next_action)>0), due_at INTEGER NOT NULL,
 accepted_at INTEGER NOT NULL, CHECK(due_at>=accepted_at), UNIQUE(principal,request_key),
 CHECK((mode='active' AND phase<>'done')
    OR (mode='closed' AND phase='done' AND next_owner='none' AND next_action='none'))
);
INSERT INTO work_v6 SELECT * FROM work;
DROP TABLE work;
ALTER TABLE work_v6 RENAME TO work;
CREATE TRIGGER work_request_immutable BEFORE UPDATE OF principal,request_key,request_hash,request_json,permit_id,accepted_at ON work
 BEGIN SELECT RAISE(ABORT,'accepted request is immutable'); END;
CREATE TRIGGER work_no_delete BEFORE DELETE ON work BEGIN SELECT RAISE(ABORT,'accepted work cannot be deleted'); END;
CREATE TRIGGER work_closed_immutable BEFORE UPDATE ON work WHEN OLD.mode='closed'
 BEGIN SELECT RAISE(ABORT,'closed work is immutable'); END;
CREATE TRIGGER work_close_requires_outcome BEFORE UPDATE OF mode ON work
 WHEN NEW.mode='closed' AND NOT EXISTS (SELECT 1 FROM work_outcome WHERE work_id=NEW.id)
 BEGIN SELECT RAISE(ABORT,'closure requires a recorded outcome'); END;
CREATE INDEX work_due ON work(due_at);

-- Attributable owner commands. One row per authenticated, applied command; the row and its
-- transition commit together. Rejected commands are audited as events, not recorded here.
CREATE TABLE owner_command (
 id TEXT PRIMARY KEY,
 action TEXT NOT NULL CHECK(action IN ('ack','snooze','resolve','cancel','abandon','close')),
 target_kind TEXT NOT NULL CHECK(target_kind IN ('attention','work')),
 target_id TEXT NOT NULL,
 work_id TEXT REFERENCES work(id),
 actor TEXT NOT NULL CHECK(actor='local-owner'),
 expected_version INTEGER NOT NULL,
 request_hash TEXT NOT NULL CHECK(length(request_hash)=64),
 receipt_json TEXT NOT NULL,
 decided_at INTEGER NOT NULL,
 coordinator_epoch INTEGER NOT NULL
);
CREATE TRIGGER owner_command_immutable BEFORE UPDATE ON owner_command BEGIN SELECT RAISE(ABORT,'owner commands are immutable'); END;
CREATE TRIGGER owner_command_no_delete BEFORE DELETE ON owner_command BEGIN SELECT RAISE(ABORT,'owner commands are retained'); END;

CREATE TABLE cancel_request (
 work_id TEXT PRIMARY KEY REFERENCES work(id),
 command_id TEXT NOT NULL UNIQUE REFERENCES owner_command(id),
 reason TEXT NOT NULL CHECK(length(reason)>0),
 requested_at INTEGER NOT NULL
);
CREATE TRIGGER cancel_request_immutable BEFORE UPDATE ON cancel_request BEGIN SELECT RAISE(ABORT,'cancel request is immutable'); END;
CREATE TRIGGER cancel_request_no_delete BEFORE DELETE ON cancel_request BEGIN SELECT RAISE(ABORT,'cancel request retained'); END;

-- The attention obligation. Delivery facts live in delivery_attempt; this row is never
-- resolved by backend acceptance, retry exhaustion or acknowledgment.
CREATE TABLE attention (
 id TEXT PRIMARY KEY,
 work_id TEXT NOT NULL REFERENCES work(id),
 review_id TEXT REFERENCES review(id),
 kind TEXT NOT NULL CHECK(kind IN ('responsibility','overdue','outcome')),
 source_key TEXT NOT NULL,
 reason TEXT NOT NULL CHECK(length(reason)>0),
 severity TEXT NOT NULL CHECK(severity IN ('info','action','urgent')),
 blocking INTEGER NOT NULL CHECK(blocking IN (0,1)),
 state TEXT NOT NULL CHECK(state IN ('open','acknowledged','resolved')),
 recipient TEXT NOT NULL CHECK(recipient='local-owner'),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>0),
 escalation_level INTEGER NOT NULL DEFAULT 0 CHECK(escalation_level BETWEEN 0 AND 2),
 created_at INTEGER NOT NULL,
 followup_at INTEGER NOT NULL,
 snoozed_until INTEGER,
 acknowledged_by TEXT, acknowledged_at INTEGER,
 acknowledged_command TEXT REFERENCES owner_command(id),
 resolved_at INTEGER, resolution TEXT, resolved_by TEXT,
 resolved_command TEXT REFERENCES owner_command(id),
 CHECK((state='resolved' AND resolved_at IS NOT NULL AND length(resolution)>0 AND resolved_by IS NOT NULL)
    OR (state<>'resolved' AND resolved_at IS NULL AND resolution IS NULL AND resolved_by IS NULL)),
 CHECK(state<>'acknowledged' OR (acknowledged_at IS NOT NULL AND acknowledged_by='local-owner'))
);
CREATE UNIQUE INDEX attention_one_live_source ON attention(source_key) WHERE state<>'resolved';
CREATE INDEX attention_by_work ON attention(work_id,state);
CREATE TRIGGER attention_identity_immutable BEFORE UPDATE OF id,work_id,kind,source_key,reason,blocking,recipient,created_at ON attention
 BEGIN SELECT RAISE(ABORT,'attention identity is immutable'); END;
CREATE TRIGGER attention_review_link_once BEFORE UPDATE OF review_id ON attention
 WHEN OLD.review_id IS NOT NULL
 BEGIN SELECT RAISE(ABORT,'attention review link is set once'); END;
CREATE TRIGGER attention_legal_transition BEFORE UPDATE OF state ON attention
 WHEN NOT ((OLD.state='open' AND NEW.state IN ('acknowledged','resolved'))
        OR (OLD.state='acknowledged' AND NEW.state='resolved'))
 BEGIN SELECT RAISE(ABORT,'illegal attention transition'); END;
CREATE TRIGGER attention_resolved_immutable BEFORE UPDATE ON attention WHEN OLD.state='resolved'
 BEGIN SELECT RAISE(ABORT,'resolved attention is immutable'); END;
CREATE TRIGGER attention_no_delete BEFORE DELETE ON attention BEGIN SELECT RAISE(ABORT,'attention retained'); END;

-- Queued delivery work for one attention item. 'accepted' means only that the latest
-- attempt was backend-accepted; it never clears the obligation.
CREATE TABLE outbox (
 id TEXT PRIMARY KEY,
 attention_id TEXT NOT NULL UNIQUE REFERENCES attention(id),
 channel TEXT NOT NULL CHECK(channel='desktop'),
 state TEXT NOT NULL CHECK(state IN ('queued','retry_wait','accepted','exhausted','cancelled')),
 tries INTEGER NOT NULL DEFAULT 0,
 level_tries INTEGER NOT NULL DEFAULT 0,
 next_try_at INTEGER NOT NULL,
 created_at INTEGER NOT NULL
);
CREATE INDEX outbox_due ON outbox(state,next_try_at);
CREATE TRIGGER outbox_identity_immutable BEFORE UPDATE OF id,attention_id,channel,created_at ON outbox
 BEGIN SELECT RAISE(ABORT,'outbox identity is immutable'); END;
CREATE TRIGGER outbox_no_delete BEFORE DELETE ON outbox BEGIN SELECT RAISE(ABORT,'outbox retained'); END;

-- Delivery history, separate from the obligation. The row is committed 'attempted' BEFORE
-- the backend call; a crash leaves it in doubt and a later sweep marks it 'unknown'.
CREATE TABLE delivery_attempt (
 id TEXT PRIMARY KEY,
 outbox_id TEXT NOT NULL REFERENCES outbox(id),
 attention_id TEXT NOT NULL REFERENCES attention(id),
 ordinal INTEGER NOT NULL CHECK(ordinal>0),
 backend TEXT NOT NULL CHECK(backend IN ('desktop','test_sink')),
 severity TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('attempted','accepted','failed','unknown')),
 started_at INTEGER NOT NULL,
 claim_until INTEGER NOT NULL,
 finished_at INTEGER,
 receipt_json TEXT,
 error TEXT,
 coordinator_epoch INTEGER NOT NULL,
 UNIQUE(outbox_id,ordinal)
);
CREATE INDEX delivery_attempt_by_attention ON delivery_attempt(attention_id,ordinal);
CREATE TRIGGER delivery_attempt_identity_immutable BEFORE UPDATE OF id,outbox_id,attention_id,ordinal,backend,severity,started_at,claim_until,coordinator_epoch ON delivery_attempt
 BEGIN SELECT RAISE(ABORT,'delivery attempt identity is immutable'); END;
CREATE TRIGGER delivery_attempt_one_way BEFORE UPDATE OF state ON delivery_attempt
 WHEN NOT (OLD.state='attempted' AND NEW.state IN ('accepted','failed','unknown'))
 BEGIN SELECT RAISE(ABORT,'delivery attempt outcome is recorded once'); END;
CREATE TRIGGER delivery_attempt_no_delete BEFORE DELETE ON delivery_attempt BEGIN SELECT RAISE(ABORT,'delivery history retained'); END;

CREATE TABLE sweep_run (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 started_at INTEGER NOT NULL,
 finished_at INTEGER NOT NULL,
 coordinator_epoch INTEGER NOT NULL,
 backend TEXT NOT NULL,
 summary_json TEXT NOT NULL
);
CREATE TRIGGER sweep_run_immutable BEFORE UPDATE ON sweep_run BEGIN SELECT RAISE(ABORT,'sweep history is append-only'); END;
CREATE TRIGGER sweep_run_no_delete BEFORE DELETE ON sweep_run BEGIN SELECT RAISE(ABORT,'sweep history is append-only'); END;

CREATE TABLE work_outcome (
 work_id TEXT PRIMARY KEY REFERENCES work(id),
 outcome TEXT NOT NULL CHECK(outcome IN ('succeeded','failed','rejected','cancelled','abandoned')),
 closed_by TEXT NOT NULL CHECK(closed_by='local-owner'),
 reason TEXT NOT NULL CHECK(length(reason)>0),
 evidence_json TEXT NOT NULL,
 evidence_digest TEXT NOT NULL CHECK(length(evidence_digest)=64),
 command_id TEXT NOT NULL UNIQUE REFERENCES owner_command(id),
 notice_attention_id TEXT NOT NULL REFERENCES attention(id),
 closed_at INTEGER NOT NULL
);
CREATE TRIGGER work_outcome_immutable BEFORE UPDATE ON work_outcome BEGIN SELECT RAISE(ABORT,'outcomes are immutable'); END;
CREATE TRIGGER work_outcome_no_delete BEFORE DELETE ON work_outcome BEGIN SELECT RAISE(ABORT,'outcomes are retained'); END;
UPDATE schema_meta SET version=6 WHERE singleton=1;
