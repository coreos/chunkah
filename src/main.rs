mod cmd_build;
mod components;
mod ocibuilder;
#[allow(dead_code)]
mod packing;
mod scan;
mod tar;
mod utils;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "chunkah")]
#[command(about = "A generalized container image rechunker")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build an OCI archive from a rootfs
    Build(Box<cmd_build::BuildArgs>),
}

fn main() -> Result<()> {
    // Set up a SIGINT handler that terminates the process. This is needed
    // because chunkah may run as PID 1 in a container, which can only receive
    // signals it has explicit handlers for. This avoids users having to add
    // e.g. --init to get Ctrl-C to behave as expected.
    ctrlc::set_handler(|| std::process::exit(130)).context("setting up signal handler")?;

    let cli = Cli::parse();

    match cli.command {
        Command::Build(args) => cmd_build::run(&args)?,
    }

    Ok(())
}
