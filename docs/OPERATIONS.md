# Asher Bridge operator reference

This file is the operator reference for the whole prototype. It has one part per stage (Task 2, Task 3, Task 4, Gate 5, Gate 6 M1, Gate 6 M2); all six are implemented, and every part describes the current build. The current schema is 8. A short guided path is in [GETTING-STARTED.md](GETTING-STARTED.md); present status is in [STATUS.md](STATUS.md).

# Task 2 operations: attended, disposable fake execution

Use only newly created disposable state. No active repositories, credentials, network services or real agents. These commands start no installed service. `run-fake` holds the coordinator owner lock for the foreground lifecycle and starts a distinct, per-session supervisor; it is not a permanent daemon.

## Build and verify

Building and verifying go only through the pinned, offline sandbox scripts. [TOOLCHAIN.md](TOOLCHAIN.md) describes the toolchain pin, what `AB_RUST_ROOT` must contain and the build-root convention:

```sh
AB_ID=<new-id> scripts/ab-verify.sh
```

Inside the sandbox that runs:

```sh
cargo fmt --check
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo test --offline --locked --features fault-injection
cargo test --offline --locked --features fault-injection -- --test-threads=1
cargo build --offline --release --locked
```

**Use `/var/tmp/ab-<id>/target/release/orchestrator`, with `supervisor` and `fake-agent` beside it.** The repository tracks no binary and nothing is built into the checkout. The examples below assume:

```sh
BIN=/var/tmp/ab-<id>/target/release        # from a passed ab-verify.sh run
```

Ordinary release build has no `fault-injection` feature. `ASHER_TEST_BARRIER`, `ASHER_TEST_BARRIER_DIR` and `ASHER_TEST_FAIL` have no effect there. Never enable the test feature for an operational build. The test suite deliberately kills processes and must run only with its generated fixtures.

## Disposable demo

```sh
demo_parent=$(mktemp -d)
$BIN/orchestrator init "$demo_parent/state"
$BIN/orchestrator permit "$demo_parent/state" permit-1 fixture:demo
printf '%s\n' '{"protocol_version":1,"request_key":"demo-symbol","fixture_id":"fixture:demo","success_criteria":"Synthetic fake execution only; no successful-work claim","source_evidence":["fixture:initial-failure"],"permit_id":"permit-1"}' | $BIN/orchestrator submit "$demo_parent/state"
$BIN/orchestrator run-fake "$demo_parent/state" demo-symbol attempt-1 complete 1000
$BIN/orchestrator attempts "$demo_parent/state"
$BIN/orchestrator list "$demo_parent/state"
$BIN/orchestrator check "$demo_parent/state"
```

A zero CLI exit status means a response was handled, not that work succeeded; always read the returned state/reason.

Expected: attempt `cleaned`, work still `active/blocked` with `inspect_fake_result_not_validatable` (only `prepare_symbol` attempts can be validated, see the Task 3 part). `init`, `permit` and `list` print `"notification_delivery":"explicit_sweep_only"`: nothing is delivered unless a manual `sweep` is running (Gate 5 part). This is execution accounting, not validated work success. The candidate is merely synthetic text.

Replaying the exact `run-fake` returns the historical attempt via a replacement supervisor's receipt lookup without a second wrapper. Changing its mode/runtime under the same ID conflicts. A new ID for the same work is rejected by the one-attempt permit budget. Replaying historical work does not renew a permit or authorize another execution.

Supported modes: `complete`, `hold`, `descendant`, `escape`, `ignore`, `flood`, `pipe_holder` and `prepare_symbol` (see the Task 3 part). All are fixed synthetic behaviors, not command strings. The mode is part of the immutable spec. To demonstrate another mode, accept a new synthetic work key and use a new immutable attempt ID. `escape` creates a bounded descendant that calls `setsid`; the resulting attempt remains `unknown`, even after the known child stops, because whole-descendant coverage was not established.

Runtime argument is 1–60000 ms, no greater than the permit's maximum. CLI output cap is 16384 bytes (API bounds 1024–65536). New fixture directories are generated beneath the private ledger's `fixtures/` directory with recorded device/inode identity; callers cannot provide arbitrary working directories or executable paths. The fixed fake reads no inherited Git configuration or hooks and runs with a scrubbed environment. No Git hooks, shell router, PTY or interactive input API exists.

## Durability and meanings

1. Attempt intent commits before contacting the supervisor.
2. Supervisor launch acceptance commits before child creation. Acceptance does not mean started.
3. Wrapper starts blocked in its own group, reports boot/PID/start/group identity, and receives release only after output capture and identity bookkeeping plus current ownership/permit checks.
4. Durable facts distinguish accepted, starting, started, parent exit, stop request, forced-stop signal, supported-scope stop confirmation and cleanup completion.
5. Cleanup still runs if SQLite fact writes fail. Those observations are explicitly volatile, never durable success receipts. Existing durable intent/receipt remains unresolved and is reconciled when storage can be read again.

Deadlines: request framing 2 s total; wrapper readiness 2 s, with an independent wrapper release wait of 3 s; runtime as requested; post-parent-exit drain 200 ms; graceful stop 150 ms then force observation up to 500 ms. These are prototype defaults, not production SLOs. OS scheduling, bounded SQLite waits and filesystem calls add latency. Coordinator transport ceiling is 75 s and its final supervisor reap allowance is 4 s. Descendant fake self-limit is 4 s, an additional cooperative fallback, not containment.

Signaling uses a pidfd and revalidates boot/start identity. New group members are enrolled only while a fresh known identity confirms continuity of the original group. Missing census data or an escape prevents a whole-cleanup claim. Zombie entries are non-running; wrapper is reaped by its supervisor, while orphaned descendant reaping belongs to the host's parent/reaper. No generic process-name kill or broad unrelated-process cleanup is used.

## Restart and unresolved obligations

Every mutating `Owner::open` advances the ownership epoch and reconciles existing attempts before exposing mutations. It projects only durable facts. A live supervisor with just an acceptance receipt is `accepted`, not `running`. A disappeared supervisor without durable cleanup is `unknown`, never “did not start.” The live slot stays occupied. Restart never blindly relaunches an accepted/unknown command.

```sh
$BIN/orchestrator reconcile "$demo_parent/state"
$BIN/orchestrator attempts "$demo_parent/state"
$BIN/orchestrator list "$demo_parent/state"
```

`attempts` and `list` are stored projections, not a live progress guarantee. Run `reconcile` after interruption when no coordinator currently owns the ledger. Unchanged reconciliation preserves the existing deadline and version, so repeated inspection cannot hide an overdue obligation. `reason` includes the termination cause even if cleanup succeeded. `unknown` names owner inspection; `list` displays overdue responsibility. Read-only inspection works while another coordinator owns the ledger; a competing mutator is refused.

The only notification channel is the desktop backend, and it is used only while a manual `sweep` is running (see the Gate 5 part). A dead application cannot alert, and neither local nor off-host observers exist. Do not rely on unattended work: durable, CLI-visible obligations are the boundary.

If the ledger is unavailable, stop requesting launches and preserve its files. `diagnose`, `recover`, `backup` and inspect-only `restore` (see the Gate 6 M1 part) examine, account for and copy a ledger; no command repairs one. Do not delete the database, reset a state, remove unknown attempts, change IDs to evade the live slot, or label missing facts successful. Reopening after storage recovery projects surviving facts conservatively. Database destruction cannot be recovered from process observation alone.

## Schema and ownership

Owner automatically migrates a supported older schema step by step to the current one (1 through 8) under the exclusive coordinator lock, one transaction per step, with foreign-key checks; a failing step leaves the previous version. The 1 → 2 step rebuilds the queued-only work constraint transactionally, preserves accepted work/events and immutability protections and creates the lifecycle tables. Read-only inspection accepts schemas 1–8; `attempts` and the supervisor require exactly 8. Missing, corrupt or unsupported newer schemas are refused, never silently reinitialized.

Supervisor writes only its receipt/fact tables. Coordinator domain writes are synchronous; no detached thread retains its connection. Normal `execute` waits for or terminates/reaps the supervisor before returning; coordinator crash leaves the independent supervisor responsible for bounded cleanup/facts. Owner absence/revocation/epoch change prohibits release and causes active fake stop. This is an attended-prototype policy, not a standing-permit prohibition for all future designs.

Socket role verification uses an inherited Unix socket, peer UID/PID, recorded start/boot identity, current epoch and held owner lock. There is no public listening endpoint. These checks prevent accidental wrong-role commands; same-user account compromise is outside this prototype's isolation guarantee. The real-agent gate still requires stronger filesystem/control-plane/process isolation.

# Task 3 operations: trusted fixture evidence and frozen candidate

Same attended, disposable boundary as Task 2. Task 3 adds **no new subprocess**: the trusted checker is in-process Rust in the coordinator. Only the existing fake preparation runs, via the existing supervisor. No Git invocation, hooks, repository scripts or inherited configuration are used for the flat synthetic fixture.

## Disposable demo

```sh
demo_parent=$(mktemp -d); S="$demo_parent/state"
$BIN/orchestrator init "$S"
$BIN/orchestrator permit "$S" permit-1 fixture:symbol
printf '%s\n' '{"protocol_version":1,"request_key":"w","fixture_id":"fixture:symbol","success_criteria":"Prepare synthetic symbol correction; capture remains incomplete","source_evidence":["fixture:malformed-symbol"],"permit_id":"permit-1"}' | $BIN/orchestrator submit "$S"
$BIN/orchestrator symbol-intent "$S" w a          # immutable prepare_symbol spec; creates initial/ and candidate/
$BIN/orchestrator reproduce "$S" a                # freeze initial/, record trusted initial_defect (before any launch)
$BIN/orchestrator run-fake "$S" w a prepare_symbol 1000   # same intent; supervised fake edits candidate/symbols.csv only
$BIN/orchestrator seal "$S" a                     # requires durable cleanup + current quiescence; prints artifact id/path/manifest
$BIN/orchestrator validate "$S" ARTIFACT_ID       # trusted checks on the frozen snapshot
$BIN/orchestrator task3-status "$S" a             # ready/blockers; work_closed is always false
$BIN/orchestrator evidence-status "$S" a          # rows: authority, trusted, fresh, sequence_complete, coverage, gaps
printf '%s' '{"source_epoch":"agent-epoch-1","source_seq":1,"artifact_id":null,"observed_at":0,"coverage":"complete","gaps":[],"payload":{"claimed_success":true}}' | $BIN/orchestrator ingest-claim "$S" a
```

`observed_at` in the claim example is a Unix millisecond time; 0 is accepted but immediately stale. Use a current time for a fresh claim.

Expected: `validate` reports `symbol_validated: true`, `capture_restored: false` with the incomplete interval disclosed; `task3-status` reports `ready: true` and `work_closed: false`; work stays `active/blocked` with `create_exact_review`. **`ready` means only: reproduced initial defect + trusted validated frozen candidate, available for a future review boundary.** It is not approval, effect, successful work or closure: review, approval and effect are in the Task 4 part, closure in the Gate 5 part.

`run-fake ... prepare_symbol` before `reproduce` is refused and launches nothing. A launched attempt can never be reproduced afterwards. Replays of `symbol-intent`, `reproduce`, `seal` and `validate` return the existing record after re-verifying it; they never create a second trusted result.

## Supported fixture layout

Fixture root (`state/fixtures/<uuid>/`): exactly `initial/`, `candidate/`, and after launch the supervisor's `captured.log` (kept outside the candidate). Each of `initial/` and `candidate/`, mode 0700, contains exactly `capture.json` and `symbols.csv`, each a single-link regular file owned by the user, mode 0600, at most 65,536 bytes. Anything else is rejected and named: symlinks, hard links, sockets/FIFOs/devices, subdirectories, `.git`/`.gitmodules`, extra files, missing files, other modes. Listings are bounded to 64 entries before rejection. Files are opened with `openat(O_NOFOLLOW|O_NONBLOCK)` relative to a directory descriptor; type is checked with `fstatat(AT_SYMLINK_NOFOLLOW)` before opening; size/identity are rechecked after reading. Nothing is silently excluded.

Initial `symbols.csv` is `BAD SYMBOL,core`; the prepared value is `FIXTURE_OK,core`. `capture.json` is an explicitly incomplete interval and is never altered by preparation. Seal additionally requires `initial/` to still match the frozen initial manifest, so preparation may change only `candidate/`.

## Manifest, snapshot and identity

The manifest is canonical JSON `{"version":1,"layout":"symbol-fixture-v1","entries":[{path,mode,size,sha256}...]}` with entries in fixed sorted order; its identity is SHA-256 of exactly those bytes. Identical content/modes from different fixtures have the same manifest hash. Non-canonical encodings are refused on read.

Snapshots live at `state/artifacts/<artifact-id>/` (outside the fixture). They are staged in `.stage-<id>/`, written from the **exact verified source bytes**, each file and the directory fsynced, renamed with `RENAME_NOREPLACE`, the parent fsynced, read back and compared, and only then referenced by a database commit. The source is rescanned after copying and must be identical (including inode/ctime). Artifact files and SQLite are not one atomic transaction: a crash before the commit leaves an unreferenced complete or staging directory, which is a harmless orphan (not garbage-collected in this prototype). A committed reference whose files later disappear is invalidated.

Each artifact row also records the snapshot's kernel identity: directory and file dev/inode/mode/link count/size/mtime/ctime. Any change, including rewriting identical bytes, chmod, relinking, or replacing the directory with an identical copy, invalidates the artifact. Invalidation is durable and one-way (`valid → invalid` with a reason, enforced by trigger), happens on every mutating open (`Owner::open` reconciliation), `task3-status`, `seal`/`validate` replay and launch guard, and sets work to `blocked / local-owner / inspect_invalid_artifact` once. Snapshot files are mode 0600 (owner-writable, so the same-user tests can model tampering). **Hashes, modes and identities detect change after the fact; they are not isolation against a same-user writer**, and a transient mutation that is restored between two checks is outside this prototype's guarantee except where ctime changes reveal it.

## Sealing and quiescence

`seal` requires all of: symbol-preparation attempt; valid frozen initial artifact; attempt state `cleaned`; latest durable lifecycle fact `cleanup_completed`; no `unknown` fact; a durable `stop_confirmed` with an empty census; the recorded supervisor, wrapper and every known descendant/started/exit identity currently non-live by boot/start identity (uncertainty counts as live); and, on the same boot, an empty current census of the wrapper's process group. Quiescence is checked before copying and again before the commit. A live coordinator holding the owner lock makes `seal` fail with `coordinator already owned`. These are Task 2's cooperative-group and known-identity limits: **not whole-descendant isolation**. A group number reused by an unrelated process makes sealing fail closed.

A candidate layout rejection is audited (`seal.rejected`), sets `inspect_candidate_layout` and is **terminal for that attempt**: restoring the files afterwards does not make that attempt sealable, because those later edits did not come from supervised preparation. Use a new work item/attempt. Refusals caused by state (not launched, unknown, live writer) are audited (`seal.refused`) without changing the work's deadline.

Limit: candidate content between cleanup completion and seal is not attested by the supervisor; a same-user writer acting in that window is outside this prototype's guarantee.

## Trusted checks

`validate` loads the candidate artifact, re-verifies it, runs the checker on the exact verified byte buffers (no pathname reread), passes the test-only `after_validation_checks` barrier, then rescans the complete snapshot and requires identical bytes and identity. Only then does one transaction insert two trusted rows: `candidate_validation` (symbol check, complete coverage) and `capture_coverage` (gap, naming the missing interval). `symbol_validated` requires exactly one `symbol,scope` row equal to `FIXTURE_OK,core`; `validation_passed` additionally requires capture bytes unchanged from the initial artifact. `capture_restored` is always false: symbol repair cannot establish restoration of incomplete capture intervals, and a candidate whose `capture.json` claims completeness is reported as changed capture, not restored. A failing check is recorded as a trusted failing result (`inspect_failed_candidate_validation`); a mid-check snapshot change invalidates the artifact and records no validation evidence.

`reproduce` uses the same checker on the frozen initial snapshot and records `initial_defect` only when the malformed symbol is reproduced, before any launch.

## Evidence ingestion and status

`ingest-claim STATE ATTEMPT` reads at most 65,536 bytes of JSON from stdin before taking ownership. Accepted fields only: `source_epoch`, `source_seq` (>0), optional `artifact_id`, `observed_at` (ms, ≥0, not more than 5 s in the future), `coverage` (`complete|partial|gap|unknown`), `gaps` (non-empty iff coverage is not complete; ≤32 entries of ≤512 bytes) and `payload`. The coordinator assigns `authority=agent_claim`, `kind=agent_claim`, `source=agent:ATTEMPT`. Supplying `source`, `authority`, `kind`, `role`, `trusted`, `id`, `work_id`, `attempt_id`, `received_at`, `payload_hash` or `producer` is refused and audited as `evidence.rejected` (field names and body hash only, not content). Unknown attempts, malformed bodies, oversize bodies, and artifacts not owned by the same work/attempt are also audited rejections.

The JSON acknowledgment prints only after the commit returns. Identical full-envelope replay (associations, observation time, coverage, gaps and payload; key order is irrelevant) returns the original evidence ID as `existing`. Changed content under the same source/epoch/sequence fails, writes an `evidence.conflict` event, and blocks readiness for that attempt until owner inspection. After a lost ACK, replay the same body.

Trusted rows are only created by coordinator code holding an in-crate capability; the database additionally binds `trusted_validator` to `source=validator:ATTEMPT`, epoch `fixture-validator-v1` and the correct artifact role (schema 3 also allowed only one row per kind per attempt; schema 4 permits bounded revalidation rows).

`evidence-status` rows derive `fresh` centrally (claims 15 s, trusted 1 h from `observed_at`; future-dated is never fresh), `sequence_complete` (all earlier sequence numbers for that source/epoch present), `intact` (payload/envelope hashes recomputed), `conflicts`, and `trusted` (validator authority + intact + artifact still valid + no conflict; claims are never trusted). Trusted evidence older than 1 h makes `task3-status` not ready; it is not refreshed by replay. Task 4 adds bounded `revalidate` (see below); `task3-status` uses the latest trusted row of each kind.

`task3-status` and `evidence-status` take the owner lock (they reconcile and may durably invalidate artifacts) and therefore fail with `coordinator already owned` while another coordinator runs. `list`, `get`, `attempts` and `check` remain read-only.

## Schema 3

Migration 003 is additive: it adds `artifact`, `evidence`, a unique `(attempt.id, work_id)` index and triggers, and touches no existing rows. At schema 3 the work phase vocabulary is unchanged (`queued/preparing/reconciling/blocked`) and no attention, outbox, review, effect or closure tables exist; later migrations add them. Unsupported newer schemas are refused without mutation.

Test barriers added for Task 3 (`before_evidence_commit`, `after_evidence_commit`, `before_seal_commit`, `after_seal_commit`, `after_validation_checks`) exist only with the `fault-injection` feature.

# Task 4 operations: exact review, owner decision and fixture ref effect

Same attended, disposable boundary. Task 4 adds **no new subprocess**: the fixture Git repository is created and updated in-process by `src/gitfixture.rs` (Git loose objects with SHA-1 ids and stored-block zlib; Git's own `<ref>.lock` compare-and-swap protocol). No `git` binary, repository script, hook or inherited configuration is ever executed by the product. The test suite uses the real `git` binary only as an independent oracle and competing writer, with system/global config disabled.

## Disposable demo

Continue the Task 3 demo after `validate` (work is `blocked / create_exact_review`):

```sh
$BIN/orchestrator review-create "$S" a                      # canonical envelope, pending review; prints review_id, envelope_hash
$BIN/orchestrator task4-status "$S" a                       # pending_review, reviews, effects, next owner/action/deadline
$BIN/orchestrator decision-template "$S" REVIEW_ID d-1 approve > decision.json   # owner console: signs with state/owner.key
$BIN/orchestrator decide "$S" < decision.json               # records decision + effect intent atomically; NOT executed
$BIN/orchestrator dispatch "$S" EFFECT_ID                   # claim, ref compare-and-swap, independent readback
$BIN/orchestrator reconcile-effects "$S"                    # explicit reprobe; also runs on every mutating open
$BIN/orchestrator revalidate "$S" ARTIFACT_ID               # only after trusted evidence expired (see below)
$BIN/orchestrator revoke "$S" permit-1                      # lock-free; may race a dispatch claim
```

Expected: `dispatch` prints `classification: confirmed_applied`; `task4-status` shows the effect `confirmed`, work `active / finalizing / local-owner / action_applied_review_closure_summary_and_close`, `action_result_is_work_completion: false` and `work_closed: false`. **An action result is not work completion.** Closure is a separate, explicit owner command; closure, attention and notification are in the Gate 5 part.

## What the review binds

`review-create` requires: preparation lifecycle `cleaned` with current quiescence (Task 2/3 limits), valid verified initial and candidate artifacts, the **latest** trusted `initial_defect`, `candidate_validation` and `capture_coverage` rows fresh, intact, sequence-complete, conflict-free and bound to those artifacts, a current unrevoked preparation permit whose fixture scope matches the request, and no applied/conflicting/unresolved effect for the work. Task 3 readiness is a precondition, not authority.

The canonical envelope (SHA-256 over its exact JSON, field order fixed, `deny_unknown_fields`) contains: work/review/attempt IDs and request hash; permit ID/version/scope hash/fixture/expiry with `grants_final_action: false`; review policy, validator and validator epoch versions, evidence freshness rule; initial and candidate artifact IDs, manifest hashes and manifests; each bound evidence ID with envelope/payload hashes, observation time and freshness bound; capture `restored: false` and gaps; the containment limit statement; the typed action (`fixture.ref_compare_swap`, repository path/dev/inode/nonce, `refs/heads/fixture`, expected old commit, proposed commit and tree, commit time/message, scope "update this disposable fixture ref only"); approver role `local-owner`; creation and expiry times. Review expiry = min(24 h, earliest evidence freshness bound, permit expiry); a review with less than 5 s left is refused.

The first review for a work creates `state/repos/<uuid>/`: a bare layout with `HEAD`, a minimal `config` (no remotes, no credentials), an **empty** `hooks/`, an empty reflog and a nonce file. The base commit is built from the frozen **initial** artifact bytes and the proposed commit (parent = base) from the frozen **candidate** bytes; objects are written before the DB reference. A crash before that commit leaves a harmless unreferenced repository. Repository identity (path, dev/inode, nonce), absence of `packed-refs`/`shallow`/grafts/modules and an empty hooks directory are rechecked at every use.

## Decisions

`decide` reads at most 16 KiB of JSON: `protocol_version`, `decision_id`, `review_id`, `envelope_hash`, `expected_work_version`, `choice` (`approve|reject`), `actor` (must be `local-owner`) and `owner_proof` = HMAC-SHA256 over the canonical other fields with the 32-byte private `state/owner.key` (0600; created atomically on first use, never silently replaced; see "Owner credential lifecycle" below). Unknown/missing fields and duplicate keys are refused. Order: authentication (role + proof) → **replay lookup** (same decision ID and same canonical content returns the recorded receipt even after the review was consumed or expired; same ID with different content is an audited `decision.conflict`) → competing decision for the review (conflict) → review must be `pending` → envelope hash and expected work version must be current → full reassessment (authority, freshness, artifacts, quiescence, repository, ref, objects) → one transaction inserting the immutable decision and, for approval, the review `authorized` + effect `intent` (or `rejected`). Forgery attempts are audited as `decision.forgery`. The owner key authenticates the owner console against agents and role confusion; **it does not defend against a compromised same-user account**, which can read the key.

A rejected, expired, invalidated or revoked review is never reset: a new review (and new evidence when stale) is required. Triggers enforce the only legal review transitions (`pending → authorized|rejected|expired|invalidated|revoked`, `authorized → consumed|expired|invalidated|revoked`) and immutability of envelopes, decisions, effects' identity and observations.

## Dispatch and the ref update

`dispatch EFFECT_ID`:

1. Refuses unless the effect is `intent` (a consumed approval returns `dispatched: false`; a cancelled one exits non-zero with its reason).
2. Takes the exclusive fixture mutation lock `state/locks/fixture-<repo>.lock` (held until the result commits; released by the kernel on death).
3. Reassesses the review (authority, freshness, artifacts, quiescence, repository, ref = expected old, no ref lock, proposed objects byte-identical). Any failure retires the review (`expired|invalidated|revoked`) and cancels the unclaimed intent.
4. Claim transaction rechecks review/effect state, coordinator epoch, permit revocation/expiry, review expiry and evidence freshness **inside** `BEGIN IMMEDIATE`, then marks the review `consumed` and the effect `dispatching` with executor identity, claim epoch, claimed ref value, ref-file inode identity and reflog length/hash. No SQLite transaction is held across the ref update.
5. After the claim the executor re-verifies the candidate and proposed objects and aborts (not applied) if they changed.
6. Compare-and-swap: create `refs/heads/fixture.lock` with `O_EXCL` (a held lock means another writer: nothing changed), compare the current value with the exact expected old ID, then **while still holding the ref lock run the final authorization recheck** (preparation permit revocation/expiry/scope, review expiry, bound evidence trusted and fresh); a refusal removes the lock and nothing changes (`cas: aborted` with the reason). Otherwise write the new ID, fsync, rename over the ref, fsync the directory, then append one reflog line `asher-effect <effect-id>`.
7. The executor's own CAS result is committed to `effect_executor_result` (source `executor_in_process`) **before** the readback; then independent readback and one result transaction (`effect_observation` + effect state + work responsibility). Success is never taken from the CAS response alone, and the executor's report is never presented as independent evidence.

**Remaining non-atomic boundary (stated honestly).** The final recheck is *not atomic with the ref update*: permit revocation or expiry, or review expiry, that lands after the recheck and before the rename cannot be prevented, and no mechanism here can make it so. Such an update is recorded as applied with `revoked_after_claim` and the revocation limit; it is never hidden or rolled back. Likewise the compare-and-swap protects the ref value, not the authority.

There is **no exactly-once claim across SQLite and Git.** Effect identity is durable before the action; interruption leads to reconciliation, never a second dispatch.

## Reconciliation

Every mutating `Owner::open` (and `reconcile-effects`) classifies each `dispatching`/`unknown` effect. If the fixture mutation lock is held, or the recorded executor identity is still live, the result is "executor may be live": nothing is concluded and nothing is written. Otherwise it reads the repository identity, the ref value, the reflog (which must still begin with the bytes present at claim) and the ref lock:

| Observation | Result |
|---|---|
| ref = proposed commit and an audit line for this effect (A→B) | `confirmed`, `attribution: audit_entry` |
| ref = proposed commit, no audit line, but the executor's own durable CAS result says `applied` | `confirmed`, `attribution: executor_reported_cas`, `audit_entry_missing: true` plus a warning that this rests on the executor's own report |
| ref = proposed commit, **no audit line and no executor result** (any cause: an interrupted update, an unlogged foreign write, a failed audit append whose result was lost) | **`unknown`**, `attribution: unattributed`, `desired_state_observed: true`; next action `inspect_desired_state_unattributed_no_retry`. The cause is not established and not claimed. Desired state alone is not attributable execution |
| ref = proposed commit but the executor reported its operation changed nothing (`mismatch`/`locked`/`aborted`) | `unknown` (contradictory) |
| ref = old, no reflog entries since claim, ref-file inode unchanged since claim, no lock (or only this effect's own leftover lock), no executor-reported `applied` | `not_applied` |
| ref = old, audit entry for this effect **and** later competing entries | `conflict` |
| ref = old, audit entry for this effect but no other writer (a partial update, an unlogged revert or a forged line cannot be told apart) | `unknown` |
| ref = old but the executor reported `applied` | `unknown` (contradictory) |
| ref = a third value | `conflict` (no automatic rollback) |
| competing writes since claim without our entry, reflog truncated/rewritten, ref-file rewritten without an explaining audit entry, foreign ref lock, repository missing/changed/unreadable, live executor | `unknown` |

`confirmed` (attributed) → work `finalizing / local-owner / action_applied_review_closure_summary_and_close` (ledgers still holding the schema-5 wording `action_applied_verify_and_close_task5_not_implemented` are repaired on the next mutating open by an audited `work.legacy_wording_repaired` transition that preserves phase, owner and deadline); every result and `task4-status` expose `attribution`, `desired_state_observed`, `audit_entry_missing`, `executor_result_recorded`, `warnings` and `closure_eligible` (true only for `confirmed` with `audit_entry` or `executor_reported_cas`; the database also refuses to set `confirmed` without such an attribution). Unattributed results can never become closure-eligible. `not_applied` → `blocked / effect_not_applied_new_review_required` (the consumed review stays consumed; create a new review). `conflict` → `blocked / inspect_conflicting_target_no_automatic_rollback`; no new review is allowed for that work. `unknown` → `blocked / inspect_ambiguous_effect_no_retry`; reprobes write only on a genuine transition, so deadlines are not postponed. There is no "retry anyway" command. A leftover `refs/heads/fixture.lock` is **never deleted automatically**; it also blocks real `git update-ref`. Remove it only after inspecting the repository and confirming no writer is running.

The reflog and ref inode are shared audit evidence for this **disposable, exclusively owned** fixture. A same-user writer can rewrite both; they support attribution here, not isolation, and would be insufficient for a shared or real repository.

## Freshness and bounded revalidation

Trusted evidence is fresh for 1 h from observation and is never extended. An open review whose bound evidence goes stale is retired as `expired` on the next mutating open/status (`review_or_evidence_expired_revalidate_then_new_review`); `review-create` then refuses with `revalidate_trusted_evidence`. `revalidate ARTIFACT_ID` is allowed only when the latest trusted initial or candidate evidence is stale, no review is open and no effect is applied/conflicting/unresolved. It re-verifies **the same** frozen initial and candidate artifacts and quiescence, reruns the trusted checks on the exact verified bytes, rescans both snapshots, and appends three new trusted rows (old rows untouched). Any failure blocks with a durable reason. At most 3 revalidations per attempt (then `revalidation_budget_exhausted_owner_decision`). There is no scheduler or automatic retry. A new review and a new decision are always required afterwards.

## Revocation

`revoke` opens the ledger **without** the coordinator lock (schema must be current) so that it can race a dispatch. SQLite serializes it against the claim transaction: committed before the claim, it blocks the dispatch (`review revoked`, intent `cancelled`); after the claim, its event payload lists the claimed effects with `too_late_to_guarantee_prevention: true`, and the effect is reconciled normally. The executor's post-claim recheck may still abort, but this is not guaranteed.

## Schema 4

Owner migrates 3 → 4 in one transaction under the coordinator lock with foreign-key checks. The work table is rebuilt to admit phases `reviewing`, `applying` and `finalizing` (mode stays `active`; there is no closure at schema 4, schema 6 adds `closed`); all rows, triggers and the due index are preserved. Task 3's one-trusted-row-per-kind index is replaced by a non-unique index so that bounded revalidation can append. New tables: `fixture_repo`, `review`, `review_evidence`, `decision`, `effect`, `effect_observation`, with immutability and legal-transition triggers, one open review and one unresolved effect per work. Unsupported newer schemas are refused without mutation by both `Owner::open` and lock-free `revoke`. Task 3 `validate` sets `create_exact_review`.

## Test-only seams

With the `fault-injection` feature only: barriers `after_review_commit`, `after_final_recheck` (inside the ref lock), `after_ref_rename` (ref renamed, audit not yet appended), `after_executor_result_commit`, `before_owner_key_rename`, `before_decision_commit`, `after_decision_commit`, `before_effect_claim` (before the claim transaction opens), `before_claim_commit`, `after_effect_claim`, `before_ref_cas`, `after_ref_cas`, `before_result_commit`, `after_result_commit`, `after_revalidation_checks`; `ASHER_TEST_CLOCK_OFFSET_MS` (forward clock offset for expiry tests) and `ASHER_TEST_CLOCK_OFFSET_FILE` (the same offset re-read on every clock call, so an already-delayed process sees time pass); `ASHER_TEST_EFFECT_JOURNAL` (append-only executor journal counting ref-update executions). Ordinary release builds contain none of them (`scripts/ab-verify.sh` fails if a release binary holds an `ASHER_TEST_` string).

Test runs need a writable temp directory with a short path (tests bind Unix sockets; 108-byte limit). The sandbox scripts set `TMPDIR=/var/tmp/ab-<id>/tmp` for that reason ([TOOLCHAIN.md](TOOLCHAIN.md)).

## Schema 5: attribution, write order and owner credential

Schema **5** (migration 005, additive) adds `effect_executor_result` and a trigger that refuses `confirmed` without attribution. Effects that an older ledger confirmed on desired state alone are flagged by an `effect.legacy_confirmation_unattributed` event (history is not rewritten; status reports them `closure_eligible: false`).

### Attribution

After recovery, desired state alone does not establish attributable execution. A ref that holds the proposed commit while no one else has logged a write does not show a "crash after rename": an unlogged same-user write, or an audit append that failed without any crash, produce the same observation. Such a result therefore stays `unknown` / `unattributed` with an explicit next responsibility and no retry path, and no cause is claimed. A crash between the ref rename and the audit append is therefore **unresolved by design**; the owner inspects the fixture repository.

### Write-order assessment (reflog before or after the rename)

Reversing the writes does not make the pair atomic; it swaps the failure window. Order as implemented (rename, then audit): a crash leaves *ref changed, audit missing* — `unknown/unattributed` unless the executor's own durable result exists. Reversed order (audit, then rename): a crash would leave *audit present, ref unchanged*, which would falsely suggest an applied update. Both windows are covered by tests in `tests/review.rs` (`p1_crash_between_ref_rename_and_audit_append_is_unresolved`, `p1_wiped_audit_after_cas_is_unresolved`, `f3_both_partial_write_orderings_reconcile_conservatively`): an audit entry with an unchanged ref and no competing writer is `unknown` (never `not_applied`, never `confirmed`). The order was kept because it keeps the only non-atomic window on the side that fails toward "ambiguous, unattributed" and because the executor's result commit adds a second, independent attribution source.

### Executor result vs readback

The executor's own CAS response is stored in `effect_executor_result` (explicit source `executor_in_process`) in its own small transaction immediately after the update and before any readback. Observations keep two separately sourced sections: `executor_result` (`source: executor_in_process`, or `source: none, status: not_recorded` when none exists) and `readback` (`source: coordinator_readback`). A crash before that commit loses the executor result; it is **never reconstructed** from the readback. A recorded `applied` that the readback contradicts is `unknown`. The executor's report is attribution evidence, not independent evidence.

### Owner credential lifecycle

- Creation is interruption-safe: a complete 32-byte private temporary file is written and fsynced, then moved into place with `renameat2(RENAME_NOREPLACE)`; an interrupted creation leaves no `owner.key` (an unused temporary at worst). The credential is created on first open only if it is **absent and no decision has ever been recorded**.
- Opening the ledger does not need a valid credential. A partial/invalid/missing credential is never regenerated or replaced on open, and signing operations (`decision-template`, `decide`) fail with an explicit error while it is not valid.
- Recovery and continuation operations (`task4-status`, `reconcile-effects`, `dispatch` of an already-recorded decision's effect, `revoke`, revalidation, `owner-key-status`) need no signing authority; recorded decisions and their effects remain valid.
- `owner-key-status` reports `valid|missing|invalid` (never key bytes). `owner-key-reset --replace-invalid` is the only replacement path: it refuses a valid credential, moves an invalid file aside as `owner.key.invalid-<ms>-<id>` (kept as evidence; the audit event holds only its length and SHA-256), creates a new key atomically, and writes `owner_key.replaced`/`owner_key.created`. After a reset, an identical retry of a decision signed with the old key cannot be re-authenticated and is refused; the recorded decision stays valid and visible in `task4-status`.
- Limit: the credential is readable by the same user. It authenticates the console against agents and role confusion, not a compromised account. A same-user process can also run `owner-key-reset` on an invalid credential.

# Gate 5 operations: durable attention, owner commands, manual sweep and closure

Same attended, disposable boundary. **There is no scheduler, service or background process.** Nothing is checked, delivered, escalated or recorded as overdue unless one of the commands below is running. Schema 6 (migration 006) is applied automatically on a mutating open; `inbox` is read-only and refuses a ledger older than schema 6 with an explicit message.

## Commands

| Command | Effect |
|---|---|
| `inbox STATE` | Read-only: live attention items with delivery facts, the work row, `last_sweep_at`, `sweep_stale` (older than 5 min or never), `background_progress: false`, and closed outcomes |
| `sweep STATE [--backend desktop]` | **One bounded manual sweep**: marks in-doubt delivery attempts delivery-unknown, backfills missing owner attention, records overdue obligations (never moves a deadline), records escalation, then delivers up to 20 due items through the backend. **Time-bounded as a whole**: one 30 s deadline covers preparation and delivery, and each delivery call also has an absolute 10 s budget. Every phase works in batches of at most 20, each committed in its own transaction (durable partial progress). The remaining time is checked before **every item**, also inside a batch, and the items already completed are committed when the deadline cuts the batch. At least one item runs per sweep. The bound is cooperative: a single uninterruptible operating-system or SQLite operation that is already in progress is not preempted, so this is not a hard real-time guarantee. Work not reached stays exactly as it was, with obligations open. The sweep reports `complete`, `stopped_in`, `budget_exhausted` and `deferred_by_budget`, and the next sweep continues where it stopped. Writes a `sweep_run` row |
| `command-template STATE ACTION TARGET ID EXPECTED_VERSION [PAYLOAD_JSON]` | Owner console: print a command signed with the Task 4 owner credential |
| `command STATE` (stdin) | Apply one signed owner command |
| `closure-summary STATE REQUEST_KEY OUTCOME` | Read-only evidence summary, blockers and `digest` for `succeeded\|failed\|rejected\|cancelled\|abandoned` |

Owner command actions: `ack` and `snooze` and `resolve` (target `attention:ID`; version = the attention item's `version`), `cancel`, `abandon`, `close` (target `work:ID`; version = the work row's `version`). Payloads: `ack` `{note?}`; `snooze` `{reason, duration_ms 1000..=3600000}`; `resolve` `{reason}`; `cancel` `{reason}`; `abandon` `{reason, residual_risk}`; `close` `{outcome, reason, evidence_digest}`. Unknown payload fields are rejected.

## What each fact means (they are not one linear state)

| Fact | Meaning | Where |
|---|---|---|
| queued | attention + outbox row exist, no attempt | `inbox` `delivery.status: queued` |
| attempted | an attempt row was committed **before** the backend call; outcome not yet recorded (or in doubt) | `attempted` |
| backend-accepted | the backend returned its documented receipt. For the desktop backend that is the notification daemon's returned notification ID. **It does not show a human saw anything** | `backend_accepted` (inbox always says `human_receipt: not established`) |
| delivery-unknown | the claim (10 s) expired with no receipt, or the backend reported a possible display without a receipt | `delivery_unknown` |
| failed | the backend reported failure / unavailability | `failed` |
| acknowledged | an authenticated owner `ack`; not approval, not resolution | attention `state: acknowledged` |
| resolved | superseded by a recorded responsibility change, closed with the work, or (for non-blocking notices only) an owner `resolve` | attention `state: resolved` + reason |

Redelivery reuses the **same attention ID**; a crash between display and receipt commit can show a duplicate, which is accepted. Redelivery never repeats a decision or an effect. Retry delays after the Nth failed/unknown attempt: 5 s, 30 s, 2 min, then 15 min; at most 8 attempts per escalation level and 3 levels, to the owner on the same desktop channel only. Exhaustion leaves the obligation open and visible. Escalation (severity → urgent) is recorded once at 5 min without backend acceptance and again at 15 min without acknowledgment, even while snoozed; a snooze only defers the delivery of the escalation. Acknowledgment stops further escalation.

## Attention is created with the transition

Every genuine work-responsibility change calls `attention::on_transition` inside the same transaction. Owner-directed responsibility (`next_owner = local-owner`) gets exactly one live attention item (`source_key resp:WORK:ACTION`); items for a superseded responsibility are resolved as `superseded` in that transaction (replacement atomic), and a changed deadline retires the overdue item for the old one. Work mode stays `active` or `closed` (the architecture's separate `attention` mode is not used: an open attention row linked by `work_id` is the "awaits explicit attention" state).

## Snooze

Owner-authorized, with a reason, 1 s to 1 h. It sets only `attention.snoozed_until`/`followup_at` and the attention version. Work, attempt, review, permit, evidence, cleanup and recovery deadlines, approval expiry and authority are untouched, the item stays unresolved and visible, and overdue/escalation recording continues. Delivery of that item is skipped until the snooze ends.

## Owner authorization

Commands are signed with the Task 4 owner credential under the distinct domain `asher-command-v1`, over `{protocol_version, command_id, action, target, expected_version, payload, actor}`. Agent identity, claims, notification metadata, delivery callbacks and arbitrary role labels have no path to these actions. Order: size/duplicate-key/envelope checks → actor and proof (audited `command.rejected`/`command.forgery`) → **replay lookup** (identical bytes return the recorded receipt as `existing`; changed bytes under the same ID are an audited `command.conflict`) → target/version checks (`command.stale`, `command.refused`) → one transaction that inserts the `owner_command` receipt and applies the transition. Task 4 key behavior is unchanged: an invalid/missing credential refuses signing and commands but not `inbox`, `sweep` or recovery; `owner-key-reset --replace-invalid` is the only replacement, after which an identical retry of a command signed with the old key is refused (the recorded receipt and transition stay).

## Closure

`closure-summary` is read-only. `close` (and `abandon`) are explicit owner commands; the coordinator recomputes the summary, requires the reviewed `evidence_digest` for `close`, refuses with the blockers if anything is unresolved, and re-derives the database part inside the closing transaction. That one transaction writes the command receipt, **retires any un-launched launch intent** (listed beforehand in the summary as `pending_launch_intents_retired_by_closure`; event `attempt.retired`), resolves the work's blocking attention, creates the **required nonblocking outcome notice** (non-success notices begin `NOT SUCCESS`, severity `action`; any residual risk is appended as `RESIDUAL RISK: …` and raises the severity to `urgent`), inserts `work_outcome`, closes the work (immutable afterwards) and writes the `work.closed` event. If the notice cannot be written (tested with a real SQL failure) nothing closes.

| Outcome | Minimum evidence the coordinator requires |
|---|---|
| succeeded | no cancellation request on record; cleaned attempt with current writer quiescence; review `consumed` with an approving owner decision; exactly the confirmed effect, observation `confirmed_applied`, `closure_eligible` (audit entry or executor-reported CAS attribution); required trusted evidence intact, conflict-free and fresh **as of the dispatch**; frozen artifacts verify; a **fresh** readback of the fixture ref still equals the applied commit |
| failed | cleaned attempt; failure evidence that is itself trusted **and fresh** (the latest trusted-validator `candidate_validation` is intact, conflict-free, has a still-valid artifact, and is within 1 h), or a `not_applied` effect; frozen artifacts verify; no open evidence obligation; no applied effect, confirmed or attributed-but-overwritten, that failure would conceal. A `conflict` is **not** evidence of failure. A cancellation excuses none of this. |
| rejected | review `rejected` by an authenticated reject decision on the exact review; cleaned attempt; no effect |
| cancelled | an owner `cancel` request on record; attempt and effects accounted for (none left intent/dispatching/unknown/unattributed-conflict); an applied effect, including an attributed-but-overwritten one, is listed as a residual risk and never undone; open evidence obligations are listed as `carried_forward_discrepancies` and become discrepancy responsibilities in the closing transaction |
| abandoned | no live hazard (an unattributed conflict is one); an attributed applied-then-overwritten effect is carried forward as a blocking discrepancy (`effect:ID:conflict_applied_overwritten`, with its attribution, observation and the pre-closure deadline; the same applies to `cancelled`); the owner's residual-risk statement is stored in the evidence, next to the coordinator's own residual risks (applied, attributed-but-overwritten or unattributed-confirmed effects); open evidence obligations are carried forward as for cancelled |

For every outcome: no unknown or in-flight effect, no unresolved attempt (the live-attempt slot is never released as if safe), no open review. Unknown, unattributed, legacy-quarantined and stale-required-evidence results can never be a success. `cancel` records the request and retires an open review (which cancels an un-dispatched approval); a claimed/unknown effect keeps its live obligation. The request is a **persistent execution fence**, never withdrawn implicitly: a second `cancel` is refused, and from then on `symbol-intent`, launch (`run-fake` of an intent), `reproduce`, `seal`, `validate`, `review-create`, `decide` and `dispatch` are refused for that work, and `succeeded` is no longer available. Unless a live hazard already owns the work, the responsibility becomes `blocked / local-owner / cancellation_requested_account_and_close` with a fresh deadline (a blocking attention item that `resolve` cannot clear); while a hazard owns it, no later forward-progress responsibility (seal, validate, review, decide, dispatch, launch) can replace the cancellation. Reconciliation, stop/cleanup of an already-accepted attempt, inspection and closure as `cancelled`/`failed`/`abandoned` remain available.

**Closed work is immutable and never launches.** A launch is refused for closed work, for a retired intent and after a cancellation request, by the coordinator before spawning and again by the supervisor inside the transaction that would record its receipt. Evidence that arrives after closure never rewrites the work row or the outcome, which stay immutable history.

**Post-closure discrepancies.** At every mutating open (and inside any transaction that finds adverse evidence for a closed work), the coordinator re-derives the work's adverse facts from durable rows. For a `succeeded` outcome it also does a fresh readback of the fixture ref. The facts are:
- an invalid artifact;
- conflicted evidence;
- an attempt that is not `cleaned` (other than an un-launched intent);
- an effect that is `dispatching`, `unknown` or an unattributed conflict;
- a succeeded effect whose ref no longer reads back as the applied commit, or cannot be read.

Each fact gets its **own** urgent, **blocking** responsibility: attention `source_key discrepancy:WORK:FACT`, reason `DISCREPANCY after closure …`, event `work.discrepancy_opened`. An acknowledgment of one fact never covers another, a later fact always gets its own new item, and an owner `resolve` is refused because the item is blocking. Only the coordinator resolves an item, and only when re-derivation or a readback shows the fact no longer holds. That observation is stored as the resolution and in `work.discrepancy_resolved`. An artifact invalidation is one-way, so no evidence in this prototype can resolve it; it stays open as a retained obligation. The inbox's `closed_outcomes` keep the historical `success` value and add `challenged` and `open_discrepancies`. Startup and every other command keep working. `abandon` with a live hazard is refused (audited `command.refused`, work untouched, deadline unchanged); no obligation-transfer mechanism exists in this prototype.

## Backends

`desktop`: in-process D-Bus client for `org.freedesktop.Notifications.Notify` on the session bus (`DBUS_SESSION_BUS_ADDRESS` `unix:path=…`, else `/run/user/UID/bus`); no subprocess. The connect is non-blocking (it completes at once or fails), and every read and write is re-armed against one absolute deadline per call, so a slow, silent, flooding or byte-trickling bus cannot hold the sweep (and the coordinator lock) past its budget. A failure or expiry before the request is written is `failed`; after it is written but before a reply it is `unknown`. Its receipt proves the daemon accepted the request and nothing more. `sink` (deterministic test sink) exists only in `fault-injection` builds and is refused by ordinary builds.

## Test-only seams (absent from ordinary builds)

Barriers `after_attempt_recorded`, `after_backend_send`, `before_closure_commit`; the sink (`ASHER_TEST_SINK_DIR`, mode file `accept|offline|lose_receipt|duplicate|slow:MS`); the budget overrides `ASHER_TEST_NOTIFY_BUDGET_MS` and `ASHER_TEST_SWEEP_BUDGET_MS`; and the Task 4 clock/journal seams. Ordinary release builds contain none of the `ASHER_TEST_` variables (checked by `scripts/ab-verify.sh`).

## Limits

No unattended sweeps, outage notification, waking the host, or delivery while the application is absent. SQLite and the desktop daemon share no atomic transaction. Desktop display can duplicate. The cooperative fake and same-user limits of Tasks 2–4 are unchanged. A same-user process can read the owner credential and edit the database.

# Gate 6 M1 operations: storage faults, recovery runs, diagnostics, backup and restore

Same attended, disposable boundary; no service, scheduler or observer. Schema 7 (migration 007, additive) is applied by the next mutating startup.

## Commands

| Command | Effect |
|---|---|
| `diagnose STATE` | **Read-only and bounded** (5 s; queries are interrupted from the deadline on; lists capped at 20). Takes no lock, never migrates, never changes the epoch, never writes `ledger.db` or `ledger.db-wal`, never creates `coordinator.lock`; it does not open a non-regular main file, WAL or shared-memory file, an empty main file beside a WAL (SQLite would discard that WAL) or a ledger with any `ledger.db-journal` beside it (SQLite would play a rollback journal back into the main file, or block on a FIFO); those are examined from directory metadata only. With no WAL and no lock holder it opens the database `immutable`, so not even `ledger.db-shm` is created. It runs the quick check, the foreign-key check and, while budget remains, the full `PRAGMA integrity_check` (which compares index content with table rows). It then runs the startup steps that precede the epoch commit — every pending additive migration, read-only admission and the epoch transaction — on a **private in-memory copy** of a ledger of at most 64 MiB, so any precondition that would make every ordinary startup fail before it begins is found by the real code rather than a list; nothing of that copy is ever written back. Always exits 0 with a JSON report. `status`: `absent`, `unavailable` (cannot be opened or read now: permissions, I/O, locked, not a regular file, or the 5 s budget ran out during the quick check), `unreadable` (not safely readable as a SQLite database: an empty main file, a rollback journal beside it, or a WAL or journal without its main file), `corrupt` (readable but the quick, full or foreign-key check fails), `incompatible` (not an Asher ledger, or a newer schema), `degraded` (readable and verified, but ordinary startup is blocked, refused or cannot begin — startup breaker, a ledger derived from a backup, an unsafe state root or a backup directory, a ledger this user cannot write, unreadable recovery records, an epoch that cannot advance to a value no owner session holds, or a failure of the in-memory startup dry run — the newest recovery run did not finish, or emergency records are not yet settled — every line of the emergency log, well formed or not, counts until a recovery pass has examined it under the same identity `recover` records it by, and on a ledger below schema 7 nothing can have examined them), `inconclusive` (the ledger opened and its quick and foreign-key checks passed and nothing wrong was found, but at least one check that `healthy` relies on — the full integrity check, the startup dry run, the recovery checks or the emergency-record check — did not run or did not finish because of the size bound or the time budget, or because the emergency log could not be read or exceeds its 256 KiB bound, so whether ordinary startup can proceed is not established; `checks_not_completed` lists them with the reason; a failed read of the emergency log is never reported as an empty log or as `emergency_records_not_settled: 0`) or `healthy`, which requires every one of those checks to have completed. A ledger over 64 MiB (main file plus WAL) is never dry-run and is therefore at best `inconclusive`, as is any diagnosis whose budget runs out before the full integrity check, the dry run or the recovery checks finish; a known problem is still reported as `degraded`, with the checks not done listed beside it. `startup_dry_run` and `integrity_check` say whether those ran to completion; plus `accessible`, file facts and digests, the `/proc/locks` holder of the coordinator lock, obligation counts, recent recovery runs, open recovery cases, the restore or snapshot record and the emergency log |
| `readback STATE T1\|T4\|T5\|T6\|T7\|S1\|S2 ID [STEP]` | Read-only check of whether a boundary's stable ID is visible: T1 `request_key`; T4 the `review_id` named in the fault (minted before COMMIT; present in any state, so a review retired after it committed still counts, and no other review of the attempt ever stands for it — an attempt ID answers `inconclusive` and lists the attempt's reviews); T5 `decision_id`; T6 effect (claimed); T7 effect, `STEP` `result_record` (default) or `executor_result`; S1 attempt (launch receipt); S2 attempt and `STEP` fact kind (facts of a kind are not individually identified; without a kind any fact counts, which is not specific). `observed`: `present`; `absent`, which is **never evidence that the transaction did not commit** (`establishes_absence: false`): a commit whose WAL sync failed stays invisible while any other connection holds the ledger and becomes visible once the last one closes, with no write in between; `inconclusive`; or `unavailable`, which never stands for absence either. Only `recover` settles an unknown outcome recorded in the emergency log (it reads back under the coordinator lock after its own commit, from which point such a commit can no longer appear) |
| `recover STATE` | One explicitly requested startup pass, **not subject to the startup breaker**; prints the recorded run and the recovery status. It runs exactly what ordinary startup runs (classification and reconciliation) and nothing more: it never dispatches, launches, retries, stops, notifies, closes, repairs or restores. On a ledger derived from a backup it records an accounting pass only (with its own epoch and owner session, like any pass; no domain row changes) |
| `backup STATE DEST` | Verified snapshot with SQLite's online backup API into a **new** private directory (`DEST` must not exist and must not lie inside any ledger root or backup). The source is read through a read-only connection and its content is never written (when it had no WAL, SQLite may create its empty WAL and shared-memory files, as any reader does). The copy is built under a staged, non-ledger name (`DEST/.snapshot-<id>.partial`) and becomes `DEST/snapshot.db` — deliberately not `ledger.db`, so a backup directory is never a state root — by an atomic rename only once it is one self-contained file carrying the current schema and a **snapshot marker in its own data** and has passed the full integrity check; `DEST/backup-manifest.json` is written after it and records its SHA-256, an order-independent logical digest of the schema and every row, schema and SQLite versions and row counts. If the backup fails, the partial copy is removed (and `DEST`, if empty) and the error says that no verified snapshot exists; if it is killed, only the staged file can remain, under a name no command treats as a backup or a ledger. Refuses a source that fails the full `PRAGMA integrity_check`. Artifact snapshots, fixture repositories, the owner credential and the emergency log are **not** included |
| `restore BACKUP TARGET` | Verifies the backup's file and logical digests against its manifest, then builds the restored ledger in a staging directory beside `TARGET` (`.<name>.restoring-<id>`) and renames it to `TARGET` only after the copy is verified, the restore recorded and a restore-verification recovery pass finished. `TARGET` must not exist and must not lie inside the backup, the backup's recorded source root, or any directory holding a ledger (including a WAL or journal whose main file is missing) or a backup. Both the snapshot and the restored copy must pass the full `PRAGMA integrity_check`; the verification reports its actual result. Prints the verification (including which referenced artifact snapshots exist at the source root), the pass and the open cases. If restore is interrupted, `TARGET` does not exist; the staging copy is left for inspection and is itself inspect-only |

**Ledgers derived from a backup are inspect-only.** A restored ledger, or any copy of `snapshot.db`, carries the snapshot marker. Ordinary startup on it is refused before any write (exit 6, `restored_inspect_only`), so it never launches, dispatches, decides, closes, sweeps or accepts work, and its rows are never reconciled into new facts (startup rules could otherwise, for example, record an effect that was applied after the backup as "never dispatched"). `diagnose`, `readback`, `list`, `get`, `inbox`, `check` and `attempts` read it; `recover` records an accounting pass; `revoke` and the supervisor refuse it without writing (act on the live source ledger instead: revoking there reports truthfully whether a claim already happened). External actions may have happened after the backup and its rows cannot show them; restoration authorizes no replay, resume or repair, and nothing in this prototype lifts the restriction.

## Exit codes and the classification line

On failure the first stderr line is the error message. A second line is JSON when the failure is classified:

| Exit | JSON | Meaning |
|---|---|---|
| 3 | `storage_fault.durability: not_committed` | The boundary's transaction did not commit (failure before COMMIT, or COMMIT refused as busy/constraint); the startup of the same call may have committed its own epoch, recovery run and reconciliation. A previous call with the same stable ID may have committed: retry only with the same identity |
| 4 | `storage_fault.durability: unknown` | A COMMIT itself failed (full, I/O, corruption): the commit record may already be in the WAL. **Do not assume it failed**, and do not read a `readback` that misses it as failure. `recover` settles it from the emergency record; otherwise resolve by resubmitting the same identity (T1, T5; T4 `review-create` returns the open review if the creation committed) or by ordinary reconciliation (T6, T7, S1, S2). Never repeat consequential work to find out |
| 5 | `storage_fault.durability: committed` | Something this call wrote committed and a later step failed — for example the T6 claim committed (and the ref update ran) but a T7 commit was then refused: `committed_before` names the committed boundary |
| 6 | `startup_refused.ledger` | Ordinary startup refused or failed: `corrupt`, `unreadable`, `incompatible`, `unavailable` (the database cannot be reached or written now: I/O, full, busy or a competing writer's lock, permissions), `degraded_startup_blocked`, `restored_inspect_only`, or a condition of the opened, verified ledger (`degraded_startup_failed`, `degraded_malformed_content`). `startup_began: false` means no epoch change, recovery run or reconciliation was committed; `preserved` says whether anything at all was written (a refusal writes nothing; a startup that could not begin may have committed only the additive migration, and says so; it may also say that whether the additive migration committed is unknown, because the schema version could not be read back after the failure — a failed read establishes nothing — and `startup_began` stays `false` then, since no epoch change, recovery run or reconciliation was committed). `"unknown"` means a startup COMMIT (the additive migration or the epoch transaction) failed in a way that leaves its outcome unknown: what it would have written may be in the ledger, `diagnose` or the next pass shows what landed, and the next pass marks such a run interrupted. `true` means the epoch advanced and reconciliation committed before the failure is kept; `preserved` says whether the failure was recorded on the run, could not be (the run stays `started` and the next pass marks it interrupted), or hit a storage fault so that it may or may not have been. Run `diagnose` |
| 1 | none | Any other refusal |

`storage_fault` also names the boundary, `step`, stage (`before_commit`, `commit`, `after_commit`), kind (`busy`, `full`, `io_err`, `corrupt`, `read_only`, `cant_open`, `constraint`, `post_commit`), the stable ID and `retry_consequential_work: false`. Boundaries: T1 accept (`submit`), T4 create review, T5 decide, T6 dispatch claim, T7 the executor's own result (`executor_result`) and the result record (`result_record`), S1 supervisor receipt, S2 supervisor lifecycle fact (step = fact kind). Other transactions (T2, T3, T8, T9, and retirement/refusal audits inside a boundary call) produce only the generic error message. The ref update runs only after the T6 commit returned OK and a process is created only after the S1 commit returned OK, so an unknown outcome there never performs the action. `run-fake` still reports the coordinator's reconcile view; an S1 fault is reported on the supervisor's stderr and in the emergency log.

## Emergency log

When a boundary outcome is `unknown` or `committed`, the CLI (and the supervisor, for S1 and for S2 at the failing fact) appends one JSON line to `STATE/emergency.jsonl` (private, append-only, at most 256 KiB; a torn earlier line never swallows a new one; when full, new incidents are reported on stderr only). It is best-effort secondary evidence, never a durable acknowledgment, and it may be lost with the disk. Each recovery run imports every record within the bound once, reads its stable ID back under the coordinator lock **after its own startup commit** (from then on a commit whose WAL sync failed can no longer appear: SQLite overwrites or invalidates WAL frames that no reader could see) and records the observation; a record whose read back was not conclusive (faulting process still alive, read back unavailable or not specific) is read back again by later runs and its case settles only on that evidence. A `partial` run settles nothing, and a ledger derived from a backup never settles a case it inherited and never settles a record by absence: a commit made after the backup cannot appear in it, so an absent read back there is recorded unresolved with an open case (the live source, if it exists, is the evidence). Recovery never resubmits, re-dispatches or relaunches anything. Import never rewrites or truncates the log; archiving it is a manual owner action.

## Recovery runs and the startup breaker

Every mutating startup records a recovery run (`recovery_run`) in the same transaction as its epoch: `started`, then exactly once `completed` (every examined obligation in `recovery_item`; the database refuses completion otherwise, and refuses inserting a run in any state but `started`), `partial` (more than 2000 obligations; unresolved ones are recorded first and nothing is resolved), `failed` (startup failed after it began) or `interrupted` (the process died; recorded by the next run). Unresolved obligations (unknown attempts; dispatching, unknown or conflicting effects; invalid artifacts of active work; open post-closure discrepancies; uncertain imported incidents; and, in a ledger derived from a backup, every obligation the backup cannot vouch for) get one open `recovery_case` each, carrying the responsibility already recorded for the work (owner, action, deadline, live blocking attention). A later complete run of an ordinary ledger resolves a case only when it no longer observes the condition under any key; the database lets only a running pass resolve a case and never a restore case. Recovery never changes a work's responsibility or deadline itself.

When the newest three runs (by insertion order, never by wall-clock time) failed, were interrupted or never finished, ordinary startup is **refused before any write** (exit 6, `degraded_startup_blocked`): repeating it could repeat the same failure without bound. Read-only commands (`list`, `get`, `inbox`, `check`, `attempts`, `diagnose`, `readback`) keep working. `revoke` also stays available (it runs no startup and only narrows authority), and so does an already launched supervisor; neither lets SQLite checkpoint the blocked ledger when it closes. `recover` runs one explicit pass; if it completes, ordinary startup resumes. If the cause persists (for example adverse ledger content), an explicit pass that fails after it began is recorded as failed; one that cannot begin at all (the migration, the admission read or the epoch transaction fails on the verified ledger) records nothing and exits 6 `degraded_startup_failed` with `startup_began: false`, and `diagnose` names that precondition where it can check it read-only. The ledger stays blocked until the owner restores a verified backup into a separate target or repairs the cause by an explicit, separately authorized action.

## Refused ledgers are preserved

No path opens a ledger SQLite would damage by opening: an empty main file beside a WAL, or any `ledger.db-journal` beside it (an Asher ledger is always in WAL mode, so a rollback journal is foreign or left by a crash before WAL mode was set; any ordinary open would play it back into `ledger.db`, and a FIFO there would block the open). A mutating open never creates a lock file where there is no ledger, verifies the ledger through an immutable view when no WAL exists, and keeps SQLite from checkpointing on close until the ledger is verified and admitted (until its epoch transaction commits). A corrupt, unreadable, incompatible, blocked or inspect-only ledger is therefore refused with `ledger.db`, `ledger.db-wal` and any journal byte for byte unchanged. Verification is the full `PRAGMA integrity_check` (not only the quick check), so a ledger whose index content disagrees with its rows is refused as corrupt before it can admit any mutation. With no WAL beside it a refused ledger gains no file at all, on every path (startup, lock-free and read-only commands all verify through an immutable view first); with a WAL present, verification needs an ordinary open, which may add SQLite's `ledger.db-shm` index (and, for startup, `coordinator.lock`). A ledger whose main file is missing beside its WAL is refused as `unreadable` and nothing is created. The lock-free paths apply the same pre-open and verification guards: `attempts` reads through a read-only connection that never checkpoints; `revoke` and the supervisor are refused in any ledger derived from a backup, and on a breaker-blocked ledger they let SQLite checkpoint it neither when they close nor automatically at their commits. Nothing initializes over an existing ledger. Do not delete, move or "repair" a refused ledger's files: preserve them, `diagnose`, and restore a verified backup into a separate target.

## Restore procedure

1. `diagnose STATE` and keep the damaged files untouched.
2. `restore BACKUP NEW_TARGET` (never inside the damaged state root). Read its verification, its artifact availability report and every open case: each obligation the backup cannot vouch for (active work, un-cleaned attempts, approved or in-flight effects with a read-only observation of the fixture ref and its audit entries, pending or authorized reviews) is an open case.
3. Reconcile the external world yourself against those cases. The restored ledger is inspect-only.
4. A restored ledger has no owner credential and no artifact snapshots. Adopting a restored ledger for ordinary use is not implemented in this prototype.

## Test-only seams (absent from ordinary builds)

`ASHER_TEST_STORAGE_FAULT=<T1|T4|T5|T6|T7|S1|S2>:<wal_write|wal_sync>:<full|ioerr>[:N]` registers a wrapping SQLite VFS that fails the Nth commit of that boundary once, inside SQLite's real commit path (`wal_sync:ioerr` writes the commit record and then reports an fsync failure: the genuinely uncertain commit); a malformed value aborts the process rather than inject nothing. Barriers `before_accept_begin`, `before_review_begin`, `before_decision_begin`, `before_supervisor_accept`, `after_storage_fault`, `after_migration_commit` (after a migration step's COMMIT returned), `during_recovery_run`, `during_restore`. `ASHER_TEST_DIAGNOSE_DELAY_MS` makes `diagnose` sleep that long immediately before the full integrity check, so a test can exhaust its budget after the quick check deterministically. The coordinator forwards the fault variable to the supervisor.

## Limits

Classification covers the seven documented boundaries; T2, T3, T8, T9 and side transactions inside a boundary call produce only the generic storage message (a failed COMMIT of such a side transaction is reported as the boundary's `not_committed`). `readback` alone can establish presence, never absence. The T7 `result_record` and S2 read backs are per step and per fact kind, not per commit, so a later commit of the same step can answer `present` for an imported incident (the effect or attempt itself is still examined and cased independently). Startup rules on ordinary ledgers are fail-closed: a transient storage or read failure during startup can retire an unconsumed approval or invalidate an artifact, and the recovery-run startup transactions add a little to the time the write lock is held (the supervisor's 50 ms fact writes can still lose a race and stay volatile). A clock regression or jump never stops startup; only an unreadable wall clock does (see the Gate 6 M2 part). Recovery accounting grows by one run and its items per mutating startup. Backups are local, exclude artifact snapshots and do not survive host or disk loss; the manifest and the snapshot marker detect accidents, not a same-user writer who rewrites them consistently. The startup breaker orders runs by rowid; a same-user writer who forges recovery history (for example a completed run at the largest rowid) can defeat or wedge it. The `diagnose` startup dry run covers only the steps up to the epoch commit (a failure after it is recorded as a failed run and bounded by the breaker), is skipped above 64 MiB and stops at the budget; the full integrity check runs at every ordinary open, which costs time proportional to the ledger.

# Gate 6 M2 operations: restart and time truthfulness

Same attended, disposable boundary. Schema 8 (migration 008, additive: `host_boot`, `time_floor`, `time_anomaly`, `evidence_gap`, append-only except the never-lowered floor).

## Host reboot

- The boot id has one reader (`src/host.rs`). Every startup compares it with the previous owner session's boot and, on a change, records a `host_boot` row and a `host.rebooted` event in the epoch transaction.
- A process identity (pid, start time, boot) from an earlier boot is never probed through `/proc` and never signalled: it is classified dead-by-reboot. An attempt whose supervisor was launched in an earlier boot and has no settling `cleanup_completed`/`unknown` fact becomes `unknown` with reason `host rebooted since launch (boot <old> -> <new>); exit unobserved: gap` and an `evidence_gap(host_rebooted)` row. No exit is fabricated and nothing is relaunched; artifacts and effects are inspected exactly as in Gate 6 M1.
- PID reuse (same boot, different start time; or another boot) never matches a recorded identity. Emergency records carry the full identity and report `faulting_process` (`earlier_boot`, `may_be_live`, `absent`, `unknown`). A record that carries only a PID is never liveness evidence.

## Time

- Supervisor startup, runtime and drain budgets are measured on CLOCK_BOOTTIME inside the supervisor, so suspended time counts against them. Boot-local values are never compared across boots.
- Authority deadlines (permits, intents, reviews and envelopes, decisions, dispatch claims) use `store::authority_time`: wall time checked against a durable floor (`time_floor`). A regression below the floor, or an unreadable wall or boot clock, yields *uncertain*, which every existing expiry path treats as expired (reasons gain `; time uncertain: <why>`). A rollback never renews or extends authority. A forward jump of more than 5 minutes beyond the boot-local lower bound is recorded but changes nothing (it can only shorten authority). New permits are refused while time is uncertain.
- Regressions, jumps and unreadable clocks are durable `time_anomaly` rows; `diagnose` shows `ledger.time` (floor, anomaly counts, latest anomaly, current verdict) without writing.
- Within the 1 s regression tolerance, authority uses the larger of the wall reading and the conservative lower bound (the floor plus same-boot boot-local time), so a sub-second step back never re-opens a deadline the ledger already passed. Evidence freshness for closure eligibility, the owner `close` command, the dispatch evidence check, Task 3 status, revalidation and `evidence-status` is judged against the same read-only verdict: under an uncertain time no evidence is fresh, so a rollback never refreshes stale evidence.
- Startup fails on time only when the wall clock is unreadable: it cannot stamp the epoch or the recovery run, so it is refused (exit 6) and nothing else changes, but first the `unreadable` anomaly is recorded durably under the existing epoch in its own transaction, so `diagnose` shows why. A regression or jump never stops startup. The manual sweep compares snooze ends, delivery claim ends, retry times, responsibility deadlines (`due_at`) and escalation age against the same verdict: under an uncertain time every such deadline counts as reached, so a rollback ends a snooze or claim and records overdue rather than extending or hiding it. The snooze end and `due_at` values that owner commands set, and display-only "expired"/overdue labels, still use plain wall time: they stamp or display, they do not authorize.

## Runtime budget without a verified supervisor

Reconcile has no signal path. When an attempt's workload was released but no verified current-boot supervisor exists (stale boot or supervisor gone), the reason records `runtime budget exceeded (...): recorded, not signalled` or `runtime budget unverifiable (time uncertain): recorded, not signalled`; the attempt is `unknown` and reaches the owner through the existing attention path. Only the live supervisor, after verifying its own child's identity, may stop on a runtime limit.

## Evidence loss

- Output above the limit: the supervisor writes `evidence_gap(output_truncated)` before the stop fact; `cleanup_completed` carries `bytes_captured` and `output_truncated`.
- `captured.log` missing or not the recorded size: `evidence_gap(capture_missing|capture_torn)` at startup reconcile; seal refuses at once.
- `captured.log` content: the supervisor hashes the bytes as it writes them and records `capture_sha256` in `cleanup_completed`. A capture whose SHA-256 differs, a cleanup record with no usable digest, two cleanup records that disagree about the capture, a capture that is not a regular file, or one longer than the output limit is `evidence_gap(capture_torn)` with the reason in `why`; an unreadable capture leaves quiescence unknown. Nothing but the supervisor ever writes the digest. An attempt whose cleanup record has no digest (written by an older build) cannot be sealed.
- An unparseable supervisor fact: `evidence_gap(fact_corrupt)` and the attempt becomes `unknown`.
- Any evidence gap on an attempt makes quiescence refuse, so no seal, validation pass, review, decision or closure can rest on it. `diagnose` shows `ledger.evidence_gaps` counts by kind.

## Test-only seams (absent from ordinary builds)

`ASHER_TEST_BOOT_ID_FILE` (replacement boot id, re-read each call: a simulated reboot), `ASHER_TEST_PROC_STAT_DIR` (per-PID replacement for `/proc/<pid>/stat`: simulated PID reuse), `ASHER_TEST_BOOTTIME_OFFSET_FILE` (added to CLOCK_BOOTTIME; with the same amount in the wall offset, a simulated suspend), the existing `ASHER_TEST_CLOCK_OFFSET_FILE` (may be negative to simulate rollback, or non-numeric to simulate an unreadable clock) and `ASHER_TEST_EXTRA_OUTPUT` (fake preparation exceeds its output limit). All are forwarded to the supervisor and helpers. No real reboot, suspend or clock change was performed in testing.

## Limits

- The durable floor and boot comparison detect a rollback relative to what this ledger has seen; a rollback that happened while the ledger was never opened, staying above the floor, is indistinguishable from real time. A same-user writer can edit the floor or anomaly rows.
- Across a reboot the floor carries only the last observed wall time, so time elapsed while the host was down is taken from the wall clock as read.
- Request and transport timeouts (D-Bus, lock waits, diagnose budgets) remain on CLOCK_MONOTONIC; they bound waiting, not authority.
- Evidence gaps are recorded once per attempt and kind; they are permanent and block that attempt's pass for good. Recovery from them is owner inspection, as for any `unknown` attempt.
