//! rmate server for Zed.
//! CLI main.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

use clap::Parser;
use dotenv::dotenv;
use std::path::PathBuf;

use std::error::Error;

mod protocol;
mod server;

#[derive(Debug, Parser)]
#[command(author, version, about)]
/// A simple proof-of-concept rmate server for Zed.
///
/// Handles rmate TCP connections and uses Zed with tmp files.
struct Args {
    /// Sets the executable path for the Zed CLI binary
    #[arg(short, long, env = "ZED_BIN", default_value = "/usr/local/bin/zed")]
    zed_bin: PathBuf,

    /// Sets a custom rmate server address
    #[arg(short, long, env = "RMATE_BIND", default_value = "127.0.0.1:52698")]
    bind: String,

    /// End the server when Zed closes
    #[arg(short, long, env = "RMATE_ONCE")]
    once: bool,
}

// Main

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // SubscriberBuilder for configuring a formatting subscriber.
    tracing_subscriber::fmt()
        // Parsing an EnvFilter from the default environment variable (RUST_LOG)
        .with_env_filter(
            EnvFilter::builder()
                // default to log all spans/events with a level of INFO or higher.
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        // Install this Subscriber as the global default.
        .init();

    dotenv().ok();
    let args = Args::parse();

    server::serve(args.bind, args.zed_bin, args.once).await?;

    Ok(())
}
