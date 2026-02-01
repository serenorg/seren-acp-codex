// ABOUTME: Error types for seren-acp-codex
// ABOUTME: Defines error variants for Codex connection and protocol errors

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Codex connection error: {0}")]
    CodexConnection(String),

    #[error("Codex protocol error: {0}")]
    CodexProtocol(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
