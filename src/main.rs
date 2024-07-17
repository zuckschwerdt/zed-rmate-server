//! rmate server for Zed.
//! CLI main.

use tracing::Level;
use tracing_subscriber::FmtSubscriber;

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
    // a builder for `FmtSubscriber`.
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(Level::INFO)
        // completes the builder.
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    dotenv().ok();
    let args = Args::parse();

    server::serve(args.bind, args.zed_bin, args.once).await?;

    Ok(())
}
