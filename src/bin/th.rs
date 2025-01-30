use clap::{Parser, Subcommand};
use std::time::Duration;
use turtle_harbor::client::commands;
use turtle_harbor::common::ipc::{Command, ProcessStatus, Response};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, default_value = "scripts.yml")]
    config: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Start scripts
    Up {
        /// Script name
        script_name: Option<String>,
    },
    /// Stop scripts
    Down {
        /// Script name
        script_name: Option<String>,
    },
    /// List scripts
    Ps,
    /// Show logs
    Logs {
        /// Script name
        script_name: Option<String>,
    },
}

fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}

fn format_status(status: ProcessStatus) -> String {
    use colored::*;
    let status_str = match status {
        ProcessStatus::Running => "running".green(),
        ProcessStatus::Stopped => "stopped".red(),
        ProcessStatus::Failed => "failed".red().bold(),
    };
    format!("{:<7}", status_str)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let command = match cli.command {
        Commands::Up { script_name } => Command::Up { script_name },
        Commands::Down { script_name } => Command::Down { script_name },
        Commands::Ps => Command::Ps,
        Commands::Logs { script_name } => Command::Logs { script_name },
    };

    match commands::send_command(command).await? {
        Response::ProcessList(processes) => {
            println!("{}", "-".repeat(70));

            println!(
                "| {:<15} | {:<6} | {:<8} | {:<10} | {:<8} |",
                "NAME", "PID", "STATUS", "UPTIME", "RESTARTS"
            );

            println!("{}", "-".repeat(70));

            for process in processes {
                println!(
                    "| {:<15} | {:<6} | {:<8} | {:<10} | {:<8} |",
                    process.name,
                    process.pid,
                    format_status(process.status).to_string(),
                    format_duration(process.uptime),
                    process.restart_count
                );
            }

            println!("{}", "-".repeat(70));
        }
        Response::Success => println!("Command executed successfully"),
        Response::Error(e) => eprintln!("Error: {}", e),
        Response::Logs(logs) => {
            if logs.is_empty() {
                println!("No logs available");
            } else {
                println!("{}", logs);
            }
        }
    }

    Ok(())
}
