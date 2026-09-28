//! Unified Scrapix binary
//!
//! Run individual services or delegate to the CLI:
//!
//! ```bash
//! scrapix api          # Run the API server
//! scrapix frontier     # Run the frontier service
//! scrapix crawler      # Run the crawler worker
//! scrapix content      # Run the content worker
//! scrapix all          # Run everything in a single process
//! scrapix crawl ...    # CLI commands (delegated to scrapix-cli)
//! ```

mod all;

use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Scrapix — high-performance web crawler and search indexer
#[derive(Parser, Debug)]
#[command(name = "scrapix")]
#[command(version, about = "Scrapix web crawler — unified binary")]
#[command(propagate_version = true)]
struct Cli {
    // Global options of the CLI commands, mirrored from `scrapix_cli::Cli`
    // (whose own parser never runs here) and passed through to it.
    /// API server URL
    #[arg(long, env = "SCRAPIX_API_URL", global = true)]
    api_url: Option<String>,

    /// API key for authentication
    #[arg(long, env = "SCRAPIX_API_KEY", global = true)]
    api_key: Option<String>,

    /// Output as JSON (default: human-friendly text)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the API server
    Api(scrapix_api::Args),

    /// Run the frontier service
    Frontier(scrapix_frontier_service::Args),

    /// Run the crawler worker
    Crawler(scrapix_worker_crawler::Args),

    /// Run the content worker
    Content(scrapix_worker_content::Args),

    /// Run all services in a single process (API + Frontier + Crawler + Content)
    All(all::AllArgs),

    // --- All other commands are forwarded to scrapix-cli ---
    /// Scrape a single URL
    #[command(flatten)]
    Cli(scrapix_cli::Commands),
}

fn init_tracing(verbose: bool) {
    let log_level = if verbose { "debug" } else { "info" };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // --- Service commands ---
        Command::Api(args) => {
            init_tracing(args.verbose);
            scrapix_api::run(args).await
        }
        Command::Frontier(args) => {
            init_tracing(args.verbose);
            scrapix_frontier_service::run(args).await
        }
        Command::Crawler(args) => {
            init_tracing(args.verbose);
            scrapix_worker_crawler::run(args).await
        }
        Command::Content(args) => {
            init_tracing(args.verbose);
            scrapix_worker_content::run(args).await
        }
        Command::All(args) => {
            init_tracing(args.verbose);
            all::run_all(args).await
        }

        // --- CLI commands: delegate to scrapix-cli ---
        Command::Cli(cmd) => {
            let exit_code = scrapix_cli::run(scrapix_cli::Cli {
                api_url: cli.api_url,
                api_key: cli.api_key,
                json: cli.json,
                command: cmd,
            })
            .await;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
    }
}
