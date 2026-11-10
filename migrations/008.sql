-- Gate 6 M2: restart and time truthfulness. Additive only: creates new tables and triggers.
-- No existing table, index, trigger or row is changed. Run under the coordinator lock inside one
-- IMMEDIATE transaction.

-- Boot identities observed at startup. A row is appended when a startup runs under a boot other
-- than the previous owner session's (previous_boot set), and once as the baseline for a ledger
-- (previous_boot NULL). Every process identity recorded under an earlier boot is stale.
CREATE TABLE host_boot (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,
 detected_epoch INTEGER NOT NULL CHECK(detected_epoch>0),
 previous_boot TEXT CHECK(previous_boot IS NULL OR length(previous_boot)>0),
 current_boot TEXT NOT NULL CHECK(length(current_boot)>0),
 wall_at INTEGER,
 CHECK(previous_boot IS NULL OR previous_boot<>current_boot)
);
CREATE TRIGGER host_boot_no_update BEFORE UPDATE ON host_boot
 BEGIN SELECT RAISE(ABORT,'boot history is append-only'); END;
CREATE TRIGGER host_boot_no_delete BEFORE DELETE ON host_boot
 BEGIN SELECT RAISE(ABORT,'boot history is retained'); END;

-- Durable conservative wall-clock floor: the latest wall time this ledger has accepted. It only
-- moves forward. boot/boot_ms pin the boot-local monotonic reading taken with it; boot_ms is
-- never compared across boots.
CREATE TABLE time_floor (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 wall_ms INTEGER NOT NULL,
 boot TEXT NOT NULL CHECK(length(boot)>0),
 boot_ms INTEGER NOT NULL CHECK(boot_ms>=0),
 epoch INTEGER NOT NULL CHECK(epoch>0)
);
CREATE TRIGGER time_floor_never_lowered BEFORE UPDATE ON time_floor WHEN NEW.wall_ms<OLD.wall_ms
 BEGIN SELECT RAISE(ABORT,'the time floor is never lowered'); END;
CREATE TRIGGER time_floor_no_delete BEFORE DELETE ON time_floor
 BEGIN SELECT RAISE(ABORT,'the time floor is retained'); END;

-- Observed clock regressions, forward jumps and unreadable time sources.
CREATE TABLE time_anomaly (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,
 kind TEXT NOT NULL CHECK(kind IN ('regression','jump','unreadable')),
 epoch INTEGER NOT NULL,
 boot TEXT NOT NULL,
 observed_wall INTEGER,
 floor_wall INTEGER NOT NULL,
 at_boot_ms INTEGER NOT NULL
);
CREATE TRIGGER time_anomaly_no_update BEFORE UPDATE ON time_anomaly
 BEGIN SELECT RAISE(ABORT,'time anomalies are append-only'); END;
CREATE TRIGGER time_anomaly_no_delete BEFORE DELETE ON time_anomaly
 BEGIN SELECT RAISE(ABORT,'time anomalies are retained'); END;

-- Explicit evidence gaps. A gap never establishes completeness, a pass or a supported success.
CREATE TABLE evidence_gap (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,
 attempt_id TEXT REFERENCES attempt(id),
 kind TEXT NOT NULL CHECK(kind IN ('output_truncated','capture_missing','capture_torn','fact_corrupt','host_rebooted')),
 detail_json TEXT NOT NULL,
 at INTEGER NOT NULL
);
CREATE INDEX evidence_gap_by_attempt ON evidence_gap(attempt_id,kind);
CREATE TRIGGER evidence_gap_no_update BEFORE UPDATE ON evidence_gap
 BEGIN SELECT RAISE(ABORT,'evidence gaps are append-only'); END;
CREATE TRIGGER evidence_gap_no_delete BEFORE DELETE ON evidence_gap
 BEGIN SELECT RAISE(ABORT,'evidence gaps are retained'); END;

UPDATE schema_meta SET version=8 WHERE singleton=1;
