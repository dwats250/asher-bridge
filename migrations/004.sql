-- Task 4: exact review, owner decision, fixture ref effect and reconciliation.
-- Run under the coordinator lock inside one IMMEDIATE transaction, foreign_keys disabled
-- outside the transaction. Existing work, events, permits, attempts, receipts, facts,
-- artifacts and evidence rows are preserved unchanged.
CREATE TABLE work_v4 (
 id TEXT PRIMARY KEY, principal TEXT NOT NULL, request_key TEXT NOT NULL,
 request_hash TEXT NOT NULL, request_json TEXT NOT NULL,
 permit_id TEXT NOT NULL REFERENCES permit(id),
 mode TEXT NOT NULL CHECK(mode='active'),
 phase TEXT NOT NULL CHECK(phase IN ('queued','preparing','reconciling','blocked','reviewing','applying','finalizing')),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>0),
 next_owner TEXT NOT NULL CHECK(length(next_owner)>0),
 next_action TEXT NOT NULL CHECK(length(next_action)>0), due_at INTEGER NOT NULL,
 accepted_at INTEGER NOT NULL, CHECK(due_at>=accepted_at), UNIQUE(principal,request_key)
);
INSERT INTO work_v4 SELECT * FROM work;
DROP TABLE work;
ALTER TABLE work_v4 RENAME TO work;
CREATE TRIGGER work_request_immutable BEFORE UPDATE OF principal,request_key,request_hash,request_json,permit_id,accepted_at ON work
 BEGIN SELECT RAISE(ABORT,'accepted request is immutable'); END;
CREATE TRIGGER work_no_delete BEFORE DELETE ON work BEGIN SELECT RAISE(ABORT,'accepted work cannot be deleted'); END;
CREATE INDEX work_due ON work(due_at);

-- Bounded trusted revalidation appends new trusted rows for the same frozen artifact.
DROP INDEX one_trusted_result;
CREATE INDEX trusted_by_kind ON evidence(attempt_id,kind,source_seq);

CREATE TABLE fixture_repo (
 id TEXT PRIMARY KEY,
 work_id TEXT NOT NULL UNIQUE REFERENCES work(id),
 attempt_id TEXT NOT NULL,
 path TEXT NOT NULL, dev INTEGER NOT NULL, ino INTEGER NOT NULL, nonce TEXT NOT NULL,
 ref_name TEXT NOT NULL CHECK(ref_name='refs/heads/fixture'),
 base_commit TEXT NOT NULL CHECK(length(base_commit)=40),
 base_artifact_id TEXT NOT NULL REFERENCES artifact(id),
 coordinator_epoch INTEGER NOT NULL, created_at INTEGER NOT NULL,
 FOREIGN KEY(attempt_id,work_id) REFERENCES attempt(id,work_id)
);
CREATE TRIGGER fixture_repo_immutable BEFORE UPDATE ON fixture_repo BEGIN SELECT RAISE(ABORT,'repository identity immutable'); END;
CREATE TRIGGER fixture_repo_no_delete BEFORE DELETE ON fixture_repo BEGIN SELECT RAISE(ABORT,'repository identity retained'); END;

CREATE TABLE review (
 id TEXT PRIMARY KEY,
 work_id TEXT NOT NULL REFERENCES work(id),
 attempt_id TEXT NOT NULL,
 candidate_artifact_id TEXT NOT NULL REFERENCES artifact(id),
 repo_id TEXT NOT NULL REFERENCES fixture_repo(id),
 work_version INTEGER NOT NULL CHECK(work_version>0),
 envelope_hash TEXT NOT NULL CHECK(length(envelope_hash)=64),
 envelope_json TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('pending','authorized','rejected','expired','invalidated','revoked','consumed')),
 state_reason TEXT,
 expires_at INTEGER NOT NULL,
 created_at INTEGER NOT NULL,
 coordinator_epoch INTEGER NOT NULL,
 CHECK(expires_at>created_at),
 FOREIGN KEY(attempt_id,work_id) REFERENCES attempt(id,work_id)
);
CREATE UNIQUE INDEX one_open_review_per_work ON review(work_id) WHERE state IN ('pending','authorized');
CREATE TRIGGER review_identity_immutable BEFORE UPDATE OF id,work_id,attempt_id,candidate_artifact_id,repo_id,work_version,envelope_hash,envelope_json,expires_at,created_at,coordinator_epoch ON review
 BEGIN SELECT RAISE(ABORT,'review envelope is immutable'); END;
CREATE TRIGGER review_legal_transition BEFORE UPDATE OF state ON review
 WHEN NOT ((OLD.state='pending' AND NEW.state IN ('authorized','rejected','expired','invalidated','revoked'))
        OR (OLD.state='authorized' AND NEW.state IN ('consumed','expired','invalidated','revoked')))
 BEGIN SELECT RAISE(ABORT,'illegal review transition; a new review is required'); END;
CREATE TRIGGER review_no_delete BEFORE DELETE ON review BEGIN SELECT RAISE(ABORT,'review history retained'); END;
CREATE TABLE review_evidence (
 review_id TEXT NOT NULL REFERENCES review(id),
 evidence_id TEXT NOT NULL REFERENCES evidence(id),
 evidence_envelope_hash TEXT NOT NULL,
 PRIMARY KEY(review_id,evidence_id)
);
CREATE TRIGGER review_evidence_immutable BEFORE UPDATE ON review_evidence BEGIN SELECT RAISE(ABORT,'review evidence immutable'); END;
CREATE TRIGGER review_evidence_no_delete BEFORE DELETE ON review_evidence BEGIN SELECT RAISE(ABORT,'review evidence retained'); END;

CREATE TABLE decision (
 id TEXT PRIMARY KEY,
 review_id TEXT NOT NULL UNIQUE REFERENCES review(id),
 actor TEXT NOT NULL CHECK(actor='local-owner'),
 request_hash TEXT NOT NULL CHECK(length(request_hash)=64),
 envelope_hash TEXT NOT NULL CHECK(length(envelope_hash)=64),
 expected_work_version INTEGER NOT NULL,
 choice TEXT NOT NULL CHECK(choice IN ('approve','reject')),
 decided_at INTEGER NOT NULL,
 receipt_json TEXT NOT NULL,
 coordinator_epoch INTEGER NOT NULL
);
CREATE TRIGGER decision_immutable BEFORE UPDATE ON decision BEGIN SELECT RAISE(ABORT,'decisions are immutable'); END;
CREATE TRIGGER decision_no_delete BEFORE DELETE ON decision BEGIN SELECT RAISE(ABORT,'decisions retained'); END;

CREATE TABLE effect (
 id TEXT PRIMARY KEY,
 work_id TEXT NOT NULL REFERENCES work(id),
 review_id TEXT NOT NULL UNIQUE REFERENCES review(id),
 decision_id TEXT NOT NULL UNIQUE REFERENCES decision(id),
 repo_id TEXT NOT NULL REFERENCES fixture_repo(id),
 action_kind TEXT NOT NULL CHECK(action_kind='fixture.ref_compare_swap'),
 action_hash TEXT NOT NULL CHECK(length(action_hash)=64),
 action_json TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('intent','dispatching','confirmed','not_applied','conflict','unknown','cancelled')),
 state_reason TEXT NOT NULL,
 intent_epoch INTEGER NOT NULL,
 created_at INTEGER NOT NULL,
 claim_epoch INTEGER, dispatched_at INTEGER, executor_json TEXT,
 claim_ref TEXT, claim_ref_ident TEXT, claim_reflog_len INTEGER, claim_reflog_hash TEXT,
 result_observation_id TEXT,
 CHECK((state='intent' OR state='cancelled') OR (claim_epoch IS NOT NULL AND dispatched_at IS NOT NULL AND executor_json IS NOT NULL AND claim_ref IS NOT NULL AND claim_ref_ident IS NOT NULL AND claim_reflog_len IS NOT NULL AND claim_reflog_hash IS NOT NULL))
);
CREATE UNIQUE INDEX one_unresolved_effect_per_work ON effect(work_id) WHERE state IN ('intent','dispatching','unknown');
CREATE TRIGGER effect_identity_immutable BEFORE UPDATE OF id,work_id,review_id,decision_id,repo_id,action_kind,action_hash,action_json,intent_epoch,created_at ON effect
 BEGIN SELECT RAISE(ABORT,'effect identity is immutable'); END;
CREATE TRIGGER effect_claim_once BEFORE UPDATE OF claim_epoch,dispatched_at,executor_json,claim_ref,claim_ref_ident,claim_reflog_len,claim_reflog_hash ON effect
 WHEN OLD.claim_epoch IS NOT NULL
 BEGIN SELECT RAISE(ABORT,'effect claim is recorded once'); END;
CREATE TRIGGER effect_legal_transition BEFORE UPDATE OF state ON effect
 WHEN NOT ((OLD.state='intent' AND NEW.state IN ('dispatching','cancelled'))
        OR (OLD.state='dispatching' AND NEW.state IN ('confirmed','not_applied','conflict','unknown'))
        OR (OLD.state='unknown' AND NEW.state IN ('confirmed','not_applied','conflict')))
 BEGIN SELECT RAISE(ABORT,'illegal effect transition; consumed approvals are never reset'); END;
CREATE TRIGGER effect_no_delete BEFORE DELETE ON effect BEGIN SELECT RAISE(ABORT,'effect identity retained'); END;

CREATE TABLE effect_observation (
 id TEXT PRIMARY KEY,
 effect_id TEXT NOT NULL REFERENCES effect(id),
 kind TEXT NOT NULL CHECK(kind IN ('dispatch_readback','reconciliation')),
 classification TEXT NOT NULL CHECK(classification IN ('confirmed_applied','confirmed_not_applied','conflict','unknown')),
 observed_at INTEGER NOT NULL,
 payload_hash TEXT NOT NULL,
 payload_json TEXT NOT NULL,
 coordinator_epoch INTEGER NOT NULL
);
CREATE TRIGGER effect_observation_immutable BEFORE UPDATE ON effect_observation BEGIN SELECT RAISE(ABORT,'observations immutable'); END;
CREATE TRIGGER effect_observation_no_delete BEFORE DELETE ON effect_observation BEGIN SELECT RAISE(ABORT,'observations retained'); END;
UPDATE schema_meta SET version=4 WHERE singleton=1;
