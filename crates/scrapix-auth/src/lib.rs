//! Scrapix Auth
//!
//! The account/identity types the engine attaches to a request. Credential
//! verification lives in the Lab; the engine only resolves answers.

pub mod types;

pub use types::{AuthenticatedAccount, AuthenticatedUser, Limits};
