//! Alyosha, a general-purpose personal assistant for NixOS.
//!
//! The design lives in `docs/`. This crate implements build step 1 of
//! `docs/README.md`: one turn through the in-memory Agent State, the Agent
//! Loop, the Tool Registry, and the Tool Executor, driven by any
//! [`llm_client::ModelClient`]. The real model client, the Context Manager,
//! the Environment Interface, and the built-in tools come in later steps.

pub mod acp;
pub mod agent_loop;
pub mod agent_state;
pub mod approvals;
pub mod clock;
pub mod context_manager;
pub mod environment;
pub mod gateway_admin;
pub mod jobs;
pub mod llm_client;
pub mod mcp;
pub mod memory;
pub mod memory_import;
pub mod session;
pub mod session_store;
pub mod skills;
pub mod tools;
pub mod user_notes;

use std::{future::Future, pin::Pin};

/// A boxed future that can move between threads, for async functions behind
/// trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
