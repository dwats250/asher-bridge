-- Task 4 correction pass. Historical effect/evidence rows are retained; quarantined work
-- responsibility is corrected below.
-- Run under the coordinator lock inside one IMMEDIATE transaction.
--
-- The executor's own compare-and-swap result is retained as explicitly sourced evidence,
-- separately from the coordinator's independent readback (effect_observation). A lost
-- executor result is never reconstructed: absence stays absence.
CREATE TABLE effect_executor_result (
 effect_id TEXT PRIMARY KEY REFERENCES effect(id),
 source TEXT NOT NULL CHECK(source='executor_in_process'),
 executor_json TEXT NOT NULL,
 result_json TEXT NOT NULL,
 recorded_at INTEGER NOT NULL,
 coordinator_epoch INTEGER NOT NULL
);
CREATE TRIGGER effect_executor_result_only_when_claimed BEFORE INSERT ON effect_executor_result
 WHEN (SELECT state FROM effect WHERE id=NEW.effect_id) IS NOT 'dispatching'
 BEGIN SELECT RAISE(ABORT,'executor result only for a claimed dispatching effect'); END;
CREATE TRIGGER effect_executor_result_immutable BEFORE UPDATE ON effect_executor_result BEGIN SELECT RAISE(ABORT,'executor result immutable'); END;
CREATE TRIGGER effect_executor_result_no_delete BEFORE DELETE ON effect_executor_result BEGIN SELECT RAISE(ABORT,'executor result retained'); END;

-- A desired state alone never establishes attributable execution. 'confirmed' requires the
-- recorded observation to cite an audit entry or the executor's own CAS result.
CREATE TRIGGER effect_confirmed_requires_attribution BEFORE UPDATE OF state ON effect
 WHEN NEW.state='confirmed' AND NOT EXISTS (
  SELECT 1 FROM effect_observation o WHERE o.effect_id=NEW.id AND o.classification='confirmed_applied'
   AND json_extract(o.payload_json,'$.attribution') IN ('audit_entry','executor_reported_cas'))
 BEGIN SELECT RAISE(ABORT,'confirmed requires attribution evidence (audit entry or executor-reported CAS)'); END;

-- Effects confirmed by the previous candidate's desired-state rule (no audit entry) are
-- flagged, not rewritten: history stays; status reports them as not closure-eligible.
INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json)
 SELECT e.work_id,'coordinator','effect.legacy_confirmation_unattributed',e.id,
        CAST(strftime('%s','now') AS INTEGER)*1000,
        '{"note":"confirmed under the superseded desired-state rule without attribution evidence; not closure-eligible; inspect"}'
 FROM effect e JOIN effect_observation o ON o.id=e.result_observation_id
 WHERE e.state='confirmed'
   AND COALESCE(json_extract(o.payload_json,'$.attribution'),'') NOT IN ('audit_entry','executor_reported_cas')
   AND COALESCE(json_extract(o.payload_json,'$.attribution'),'') NOT LIKE 'audit entry for this effect%';
-- Replace only the stale close-directed action, and only while a confirmed, still-unattributed
-- legacy effect remains quarantined. Preserve phase, owner and deadline. The audit entry and
-- correction share this migration transaction.
INSERT INTO event(work_id,actor,kind,entity_id,at,payload_json)
 SELECT w.id,'coordinator','work.legacy_quarantine_responsibility_repaired',w.id,
        CAST(strftime('%s','now') AS INTEGER)*1000,
        json_object('previous_phase',w.phase,'previous_owner',w.next_owner,
                    'previous_next_action',w.next_action,'next_phase',w.phase,
                    'next_owner',w.next_owner,
                    'next_action','inspect_legacy_unattributed_confirmation',
                    'due_at_preserved',w.due_at)
   FROM work w
  WHERE w.next_action='action_applied_verify_and_close_task5_not_implemented'
    AND EXISTS (
      SELECT 1 FROM effect e JOIN event q ON q.entity_id=e.id AND q.work_id=e.work_id
       WHERE e.work_id=w.id AND e.state='confirmed'
         AND q.kind='effect.legacy_confirmation_unattributed'
         AND COALESCE((SELECT json_extract(o.payload_json,'$.attribution')
                         FROM effect_observation o WHERE o.effect_id=e.id
                        ORDER BY o.observed_at DESC,o.rowid DESC LIMIT 1),'')
             NOT IN ('audit_entry','executor_reported_cas')
    );
UPDATE work
   SET next_action='inspect_legacy_unattributed_confirmation', version=version+1
 WHERE next_action='action_applied_verify_and_close_task5_not_implemented'
   AND EXISTS (
      SELECT 1 FROM effect e JOIN event q ON q.entity_id=e.id AND q.work_id=e.work_id
       WHERE e.work_id=work.id AND e.state='confirmed'
         AND q.kind='effect.legacy_confirmation_unattributed'
         AND COALESCE((SELECT json_extract(o.payload_json,'$.attribution')
                         FROM effect_observation o WHERE o.effect_id=e.id
                        ORDER BY o.observed_at DESC,o.rowid DESC LIMIT 1),'')
             NOT IN ('audit_entry','executor_reported_cas')
   );
UPDATE schema_meta SET version=5 WHERE singleton=1;
