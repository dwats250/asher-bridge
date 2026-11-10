CREATE TABLE schema_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL);
INSERT INTO schema_meta VALUES(1,1);
CREATE TABLE coordinator_epoch (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), epoch INTEGER NOT NULL CHECK(epoch>0),
 instance_id TEXT NOT NULL, boot_id TEXT NOT NULL, committed_heartbeat_at INTEGER NOT NULL
);
CREATE TABLE permit (
 id TEXT PRIMARY KEY, version INTEGER NOT NULL CHECK(version>0), owner TEXT NOT NULL,
 scope_hash TEXT NOT NULL, scope_json TEXT NOT NULL, issued_at INTEGER NOT NULL,
 expires_at INTEGER NOT NULL, revoked_at INTEGER, CHECK(expires_at>issued_at)
);
CREATE TABLE work (
 id TEXT PRIMARY KEY, principal TEXT NOT NULL, request_key TEXT NOT NULL,
 request_hash TEXT NOT NULL, request_json TEXT NOT NULL,
 permit_id TEXT NOT NULL REFERENCES permit(id),
 mode TEXT NOT NULL CHECK(mode='active'), phase TEXT NOT NULL CHECK(phase='queued'),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>0),
 next_owner TEXT NOT NULL CHECK(length(next_owner)>0),
 next_action TEXT NOT NULL CHECK(length(next_action)>0), due_at INTEGER NOT NULL,
 accepted_at INTEGER NOT NULL, CHECK(due_at>=accepted_at), UNIQUE(principal,request_key)
);
CREATE TABLE event (
 seq INTEGER PRIMARY KEY AUTOINCREMENT, work_id TEXT REFERENCES work(id),
 actor TEXT NOT NULL, kind TEXT NOT NULL, entity_id TEXT NOT NULL,
 at INTEGER NOT NULL, payload_json TEXT NOT NULL
);
CREATE TRIGGER event_no_update BEFORE UPDATE ON event BEGIN SELECT RAISE(ABORT,'events are append-only'); END;
CREATE TRIGGER event_no_delete BEFORE DELETE ON event BEGIN SELECT RAISE(ABORT,'events are append-only'); END;
CREATE TRIGGER work_request_immutable BEFORE UPDATE OF principal,request_key,request_hash,request_json,permit_id,accepted_at ON work
 BEGIN SELECT RAISE(ABORT,'accepted request is immutable'); END;
CREATE TRIGGER work_no_delete BEFORE DELETE ON work BEGIN SELECT RAISE(ABORT,'accepted work cannot be deleted'); END;
CREATE TRIGGER permit_scope_immutable BEFORE UPDATE OF version,owner,scope_hash,scope_json,issued_at,expires_at ON permit
 BEGIN SELECT RAISE(ABORT,'permit scope is immutable'); END;
CREATE INDEX work_due ON work(due_at);
