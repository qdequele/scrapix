//! # Scrapix Core
//!
//! Core types, traits, and utilities for the Scrapix web crawler.
//!
//! This crate provides the foundational components used across all Scrapix services:
//! - Configuration schemas
//! - Document types
//! - Error types
//! - Core traits
//! - Billing types (accounts, API keys, usage tracking)

pub mod ack;
pub mod billing;
pub mod config;
pub mod document;
pub mod error;
pub mod job_spec;
pub mod metrics;
pub mod telemetry;
pub mod traits;
pub mod url_glob;

pub use ack::Ack;
pub use billing::*;
pub use config::*;
pub use document::*;
pub use error::*;
pub use job_spec::JobSpec;
