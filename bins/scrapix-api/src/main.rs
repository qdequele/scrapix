use clap::Parser;

use scrapix_api::Args;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    scrapix_core::telemetry::init_tracing(args.verbose);
    scrapix_api::run(args).await
}
