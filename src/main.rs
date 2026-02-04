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
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "chunkah")]
#[command(about = "A generalized container image rechunker")]
struct Cli {
    /// Increase verbosity (-v for debug, -vv for trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build an OCI archive from a rootfs
    Build(Box<cmd_build::BuildArgs>),
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.verbose);
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "starting chunkah");

    // Set up a SIGINT handler that terminates the process. This is needed
    // because chunkah may run as PID 1 in a container, which can only receive
    // signals it has explicit handlers for. This avoids users having to add
    // e.g. --init to get Ctrl-C to behave as expected.
    ctrlc::set_handler(|| std::process::exit(130)).context("setting up signal handler")?;

    match cli.command {
        Command::Build(args) => cmd_build::run(&args)?,
    }

    Ok(())
}

fn init_tracing(verbose: u8) {
    let format = fmt::format().without_time().with_target(false).compact();

    // CLI -v flags take precedence, then RUST_LOG, then default to info
    let env_filter = match verbose {
        0 => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("chunkah=info")),
        1 => EnvFilter::new("chunkah=debug"),
        _ => EnvFilter::new("chunkah=trace"),
    };

    tracing_subscriber::fmt()
        .event_format(format)
        .with_writer(std::io::stderr)
        .with_env_filter(env_filter)
        .init();
}
