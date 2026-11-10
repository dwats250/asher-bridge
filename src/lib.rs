//! Attended fixture ledger, cooperative fake-process lifecycle, trusted fixture evidence,
//! frozen candidates, exact review and one fixture-ref effect with reconciliation.
//! No real agents, notifications or work closure.
pub mod model;
pub mod protocol;
pub mod store;
pub use store::{Error, Owner, Reader};

pub mod execution;
pub mod host;
pub mod process_scope;
pub mod supervisor;

pub mod artifact;
pub mod evidence;
pub mod validation;

pub mod attention;
pub mod command;
pub mod dbus;
pub mod effect;
pub mod gitfixture;
pub mod outcome;
pub mod review;

// Gate 6 M1: storage-fault classification, recovery accounting, diagnostics and restore.
pub mod diagnose;
#[cfg(feature = "fault-injection")]
mod faultvfs;
pub mod recovery;
pub mod restore;
pub mod storage;
