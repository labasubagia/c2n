use clap::Parser;

use c2n::cli::app::{Cli, run};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli).await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}
