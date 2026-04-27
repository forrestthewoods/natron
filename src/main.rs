use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = natron::cli::Cli::parse();
    natron::cli::run(cli)
}
