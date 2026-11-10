use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub id: String,
    pub principal: String,
    pub request_key: String,
    pub request_hash: String,
    pub permit_id: String,
    pub mode: String,
    pub phase: String,
    pub version: i64,
    pub next_owner: String,
    pub next_action: String,
    pub due_at: i64,
    pub accepted_at: i64,
}
impl Work {
    pub fn is_accounted(&self) -> bool {
        let closed = self.mode == "closed"
            && self.phase == "done"
            && self.next_owner == "none"
            && self.next_action == "none";
        let active = self.mode == "active"
            && matches!(
                self.phase.as_str(),
                "queued"
                    | "preparing"
                    | "reconciling"
                    | "blocked"
                    | "reviewing"
                    | "applying"
                    | "finalizing"
            )
            && !self.next_owner.is_empty()
            && !self.next_action.is_empty();
        (closed || active) && self.version > 0 && self.due_at >= self.accepted_at
    }
    /// Closed work has no deadline to miss.
    pub fn overdue(&self, now: i64) -> bool {
        self.mode == "active" && self.due_at < now
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub disposition: Disposition,
    pub work: Work,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Accepted,
    Existing,
}
