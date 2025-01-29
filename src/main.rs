use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start all scripts
    Up,
    /// Stop all scripts
    Down,
    /// List running scripts
    Ps,
    /// Show script logs
    Logs {
        /// Script name
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Up => {
            println!("Starting scripts...");
            Ok(())
        }
        Commands::Down => {
            println!("Stopping scripts...");
            Ok(())
        }
        Commands::Ps => {
            println!("Listing scripts...");
            Ok(())
        }
        Commands::Logs { name } => {
            println!("Showing logs for {}", name);
            Ok(())
        }
    }
}
