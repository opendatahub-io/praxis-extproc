// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Error types for the ExtProc server.

// -----------------------------------------------------------------------------
// Error Types
// -----------------------------------------------------------------------------

/// Result alias for ExtProc operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

// -----------------------------------------------------------------------------
// Error
// -----------------------------------------------------------------------------

/// Errors produced during ExtProc operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration loading or parsing failed.
    #[error("config: {0}")]
    Config(String),

    /// Filter pipeline construction failed.
    #[error("pipeline: {0}")]
    Pipeline(String),

    /// gRPC transport error.
    #[error("grpc: {0}")]
    Grpc(#[from] tonic::transport::Error),
}
