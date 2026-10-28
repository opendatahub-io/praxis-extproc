// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Development tasks for praxis-extproc.
//!
//! Task families include `cargo xtask fips` for release checks and
//! `cargo xtask e2e` for qualification-only integration runners.

#![allow(
    clippy::exit,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::unused_result_ok,
    clippy::unwrap_used,
    reason = "development tooling: it talks to a terminal and exits with a status"
)]
#![allow(let_underscore_drop, reason = "development tooling")]

mod e2e;
mod fips;

use std::ffi::OsString;

use clap::{Parser, Subcommand};

// -----------------------------------------------------------------------------
// CLI Definition
// -----------------------------------------------------------------------------

/// Top-level CLI for xtask development commands.
#[derive(Parser)]
#[command(name = "xtask", about = "praxis-extproc development tasks")]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// Available xtask subcommands.
#[derive(Subcommand)]
enum Command {
    /// FIPS build tooling: the compliance report and the Red Hat base image
    /// checks.
    Fips(fips::Args),

    /// Cross-component end-to-end qualification runners.
    E2e(Box<e2e::Args>),
}

// -----------------------------------------------------------------------------
// Entry Point
// -----------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse_from(default_native_e2e_scenario(std::env::args_os()));
    match cli.command {
        Command::Fips(args) => fips::run(args),
        Command::E2e(args) => e2e::run(*args),
    }
}

/// Insert the native lane when the caller invokes `e2e` without a scenario.
fn default_native_e2e_scenario(mut arguments: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut values = arguments.by_ref().collect::<Vec<_>>();
    let is_e2e = values.get(1).is_some_and(|value| value == "e2e");
    let has_explicit_scenario = values
        .get(2)
        .is_some_and(|value| !value.to_string_lossy().starts_with('-'));
    if is_e2e && !has_explicit_scenario {
        values.insert(2, OsString::from("grid-llmd"));
    }
    values
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::default_native_e2e_scenario;

    #[test]
    fn e2e_defaults_to_native_grid_llmd_and_keeps_explicit_scenarios() {
        let args = ["xtask", "e2e"].map(OsString::from);
        let result = default_native_e2e_scenario(args.into_iter());
        assert_eq!(result[2], "grid-llmd");

        let args = ["xtask", "e2e", "grid-maas"].map(OsString::from);
        let result = default_native_e2e_scenario(args.into_iter());
        assert_eq!(result[2], "grid-maas");

        let args = ["xtask", "e2e", "--mode", "plan"].map(OsString::from);
        let result = default_native_e2e_scenario(args.into_iter());
        assert_eq!(result[2], "grid-llmd");
    }
}
