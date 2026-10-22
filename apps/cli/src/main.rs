//! `datareco` command-line tool. Uses the same engine crates as the desktop app.

use clap::Parser;

/// DataReco: disk editor and data recovery tool.
///
/// No commands yet. Disk commands arrive with Phase 1 (see docs/ROADMAP.md).
#[derive(Parser)]
#[command(name = "datareco", version, about, arg_required_else_help = true)]
struct Cli {}

fn main() {
    Cli::parse();
}
