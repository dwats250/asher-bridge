use crate::store::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERSION: u32 = 1;
pub const MAX_REQUEST: usize = 65536;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitWork {
    pub protocol_version: u32,
    pub request_key: String,
    pub fixture_id: String,
    pub success_criteria: String,
    pub source_evidence: Vec<String>,
    pub permit_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitScope {
    pub fixture_id: String,
    pub action: String,
    pub max_attempts: u32,
    pub max_runtime_ms: u64,
    pub network: bool,
}
impl SubmitWork {
    pub fn validate(&self) -> Result<(), Error> {
        if self.protocol_version != VERSION {
            return Err(Error::Invalid("unsupported protocol"));
        }
        if !valid_id(&self.request_key)
            || !valid_fixture(&self.fixture_id)
            || !valid_id(&self.permit_id)
            || self.success_criteria.trim().is_empty()
            || self.success_criteria.len() > 8192
            || self.source_evidence.is_empty()
            || self.source_evidence.len() > 64
            || self
                .source_evidence
                .iter()
                .any(|s| s.is_empty() || s.len() > 1024)
        {
            return Err(Error::Invalid("invalid or unbounded request fields"));
        }
        if serde_json::to_vec(self)?.len() > MAX_REQUEST {
            return Err(Error::Invalid("request too large"));
        }
        Ok(())
    }
    pub fn canonical(&self) -> Result<String, Error> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }
}
impl PermitScope {
    pub fn validate(&self) -> Result<(), Error> {
        if !valid_fixture(&self.fixture_id)
            || self.action != "prepare_fixture"
            || self.max_attempts != 1
            || self.max_runtime_ms == 0
            || self.max_runtime_ms > 60000
            || self.network
        {
            return Err(Error::Invalid(
                "only bounded offline fixture preparation is supported",
            ));
        }
        Ok(())
    }
}
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}
pub fn valid_fixture(s: &str) -> bool {
    s.strip_prefix("fixture:").is_some_and(valid_id)
}
pub fn fingerprint(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
