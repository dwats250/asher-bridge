-- Task 3: trusted evidence and frozen candidate artifacts. Additive; existing rows untouched.
-- Run under the coordinator lock inside one IMMEDIATE transaction.
CREATE UNIQUE INDEX attempt_id_work ON attempt(id,work_id);
CREATE TABLE artifact (
 id TEXT PRIMARY KEY,
 work_id TEXT NOT NULL REFERENCES work(id),
 attempt_id TEXT NOT NULL,
 role TEXT NOT NULL CHECK(role IN ('initial','candidate')),
 layout TEXT NOT NULL CHECK(layout='symbol-fixture-v1'),
 manifest_hash TEXT NOT NULL CHECK(length(manifest_hash)=64),
 manifest_json TEXT NOT NULL,
 identity_json TEXT NOT NULL,
 content_path TEXT NOT NULL,
 coordinator_epoch INTEGER NOT NULL,
 sealed_at INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('valid','invalid')),
 invalid_reason TEXT,
 UNIQUE(attempt_id,role),
 UNIQUE(id,work_id,attempt_id),
 FOREIGN KEY(attempt_id,work_id) REFERENCES attempt(id,work_id),
 CHECK((state='valid' AND invalid_reason IS NULL) OR (state='invalid' AND length(invalid_reason)>0))
);
CREATE TRIGGER artifact_identity_immutable BEFORE UPDATE OF id,work_id,attempt_id,role,layout,manifest_hash,manifest_json,identity_json,content_path,coordinator_epoch,sealed_at ON artifact
 BEGIN SELECT RAISE(ABORT,'artifact identity is immutable'); END;
CREATE TRIGGER artifact_invalidation_one_way BEFORE UPDATE OF state,invalid_reason ON artifact
 WHEN NOT (OLD.state='valid' AND NEW.state='invalid')
 BEGIN SELECT RAISE(ABORT,'artifact invalidation is one-way'); END;
CREATE TRIGGER artifact_no_delete BEFORE DELETE ON artifact BEGIN SELECT RAISE(ABORT,'artifact history retained'); END;
CREATE TABLE evidence (
 id TEXT PRIMARY KEY,
 work_id TEXT NOT NULL REFERENCES work(id),
 attempt_id TEXT NOT NULL,
 artifact_id TEXT,
 authority TEXT NOT NULL CHECK(authority IN ('agent_claim','trusted_validator')),
 source TEXT NOT NULL,
 source_epoch TEXT NOT NULL,
 source_seq INTEGER NOT NULL CHECK(source_seq>0),
 kind TEXT NOT NULL,
 observed_at INTEGER NOT NULL CHECK(observed_at>=0),
 received_at INTEGER NOT NULL,
 coverage TEXT NOT NULL CHECK(coverage IN ('complete','partial','gap','unknown')),
 gaps_json TEXT NOT NULL,
 payload_hash TEXT NOT NULL,
 payload_json TEXT NOT NULL,
 envelope_hash TEXT NOT NULL,
 coordinator_epoch INTEGER NOT NULL,
 UNIQUE(source,source_epoch,source_seq),
 FOREIGN KEY(attempt_id,work_id) REFERENCES attempt(id,work_id),
 FOREIGN KEY(artifact_id,work_id,attempt_id) REFERENCES artifact(id,work_id,attempt_id),
 CHECK(
  (authority='agent_claim' AND kind='agent_claim' AND source='agent:'||attempt_id)
  OR
  (authority='trusted_validator' AND kind IN ('initial_defect','candidate_validation','capture_coverage')
   AND source='validator:'||attempt_id AND source_epoch='fixture-validator-v1' AND artifact_id IS NOT NULL)
 )
);
CREATE UNIQUE INDEX one_trusted_result ON evidence(attempt_id,kind) WHERE authority='trusted_validator';
CREATE TRIGGER trusted_evidence_role BEFORE INSERT ON evidence
 WHEN NEW.authority='trusted_validator' AND
  (SELECT role FROM artifact WHERE id=NEW.artifact_id) IS NOT (CASE NEW.kind WHEN 'initial_defect' THEN 'initial' ELSE 'candidate' END)
 BEGIN SELECT RAISE(ABORT,'trusted evidence bound to wrong artifact role'); END;
CREATE TRIGGER evidence_no_update BEFORE UPDATE ON evidence BEGIN SELECT RAISE(ABORT,'evidence immutable'); END;
CREATE TRIGGER evidence_no_delete BEFORE DELETE ON evidence BEGIN SELECT RAISE(ABORT,'evidence retained'); END;
UPDATE schema_meta SET version=3 WHERE singleton=1;
