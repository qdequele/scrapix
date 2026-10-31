//! Scrapix Lab
//!
//! The engine's client for the Lab's internal API (`{LAB_URL}/internal/*`),
//! shared by the API server and the workers that need the same lookups.

mod client;

pub use client::{
    log_lab_error, AiTarget, Available, CredentialKind, Identity, InstanceInfo, LabClient,
    LabError, MeiliTarget, Timing,
};

#[cfg(any(test, feature = "testing"))]
pub use client::testing;
