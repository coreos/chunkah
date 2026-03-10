mod cmd_build;
mod components;
mod ocibuilder;
#[allow(dead_code)]
mod packing;
mod scan;
mod tar;
mod utils;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

#[derive(Parser)]
#[command(name = "chunkah")]
#[command(about = "A generalized container image rechunker")]
struct Cli {
    /// Increase verbosity (-v for debug, -vv for trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Write trace-level logs to a file
    #[arg(long, value_name = "FILE", hide = true, global = true)]
    trace_logfile: Option<Utf8PathBuf>,

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

    init_tracing(cli.verbose, cli.trace_logfile.as_deref())?;
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

fn init_tracing(verbose: u8, trace_logfile: Option<&Utf8Path>) -> Result<()> {
    // CLI -v flags take precedence, then RUST_LOG, then default to info
    let stderr_filter = match verbose {
        0 => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("chunkah=info")),
        1 => EnvFilter::new("chunkah=debug"),
        _ => EnvFilter::new("chunkah=trace"),
    };

    let stderr_layer = fmt::layer()
        .event_format(fmt::format().without_time().with_target(false).compact())
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter);

    let file_layer = match trace_logfile {
        Some(path) => {
            let file = std::fs::File::create(path.as_std_path())
                .with_context(|| format!("creating trace logfile {path}"))?;
            Some(
                fmt::layer()
                    .with_writer(std::sync::Mutex::new(file))
                    .with_filter(EnvFilter::new("chunkah=trace")),
            )
        }
        None => None,
    };

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Ok(())
}
