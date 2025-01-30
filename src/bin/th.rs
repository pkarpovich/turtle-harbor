use clap::{Parser, Subcommand};
use turtle_harbor::common::ipc::{Command, Response, ProcessStatus};
use turtle_harbor::client::commands;
use turtle_harbor::common::config::Config;
use std::time::Duration;
use colored::*;

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
    Up {
        script_name: Option<String>,
    },
    Down {
        script_name: Option<String>,
    },
    Ps,
    Logs {
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

fn format_status(status: ProcessStatus) -> colored::ColoredString {
    match status {
        ProcessStatus::Running => "running".green(),
        ProcessStatus::Stopped => "stopped".red(),
        ProcessStatus::Failed => "failed".red().bold(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    let command = match cli.command {
        Commands::Up { script_name } => {
            match script_name {
                Some(name) => {
                    let script = config.scripts.get(&name)
                        .ok_or_else(|| anyhow::anyhow!("Script {} not found in config", name))?;
                    Command::Up {
                        name: name.clone(),
                        command: script.command.clone(),
                        restart_policy: script.restart_policy.clone(),
                        max_restarts: script.max_restarts,
                    }
                }
                None => {
                    return Err(anyhow::anyhow!("Script name is required"));
                }
            }
        }
        Commands::Down { script_name } => {
            match script_name {
                Some(name) => Command::Down { name },
                None => return Err(anyhow::anyhow!("Script name is required")),
            }
        }
        Commands::Ps => Command::Ps,
        Commands::Logs { script_name } => {
            match script_name {
                Some(name) => Command::Logs { name },
                None => return Err(anyhow::anyhow!("Script name is required")),
            }
        }
    };

    match commands::send_command(command).await? {
        Response::ProcessList(processes) => {
            println!("{}", "-".repeat(70));
            println!("| {:<15} | {:<6} | {:<7} | {:<10} | {:<8} |",
                     "NAME", "PID", "STATUS", "UPTIME", "RESTARTS");
            println!("{}", "-".repeat(70));

            for process in processes {
                println!("| {:<15} | {:<6} | {} | {:<10} | {:<8} |",
                         process.name,
                         process.pid,
                         format_status(process.status),
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