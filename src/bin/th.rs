use std::path::PathBuf;
use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::*;
use std::time::Duration;
use turtle_harbor::client::{commands, service};
use turtle_harbor::client::error::handle_error;
use turtle_harbor::common::error::Error;
use turtle_harbor::common::ipc::{Command, ProcessInfo, ProcessStatus, Response};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
    #[arg(short, long, default_value = "scripts.yml")]
    pub config: PathBuf,
}

#[derive(Subcommand)]
pub enum Commands {
    Up { script_name: Option<String> },
    Down { script_name: Option<String> },
    Ps,
    Logs {
        script_name: Option<String>,
        #[arg(short = 'n', long, default_value = "100")]
        tail: u32,
        #[arg(short, long)]
        follow: bool,
    },
    Reload,
    Install {
        #[arg(long)]
        http_port: Option<u16>,
    },
    Uninstall,
}

pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}

pub fn format_status(status: ProcessStatus, exit_code: Option<i32>) -> ColoredString {
    match status {
        ProcessStatus::Running => "running".green(),
        ProcessStatus::Stopped => "exited (0)".dimmed(),
        ProcessStatus::Restarting => "restarting".yellow(),
        ProcessStatus::Failed => {
            let code = exit_code.unwrap_or(-1);
            format!("failed ({})", code).red().bold()
        }
    }
}

fn print_process_list_table(processes: &[ProcessInfo]) {
    if processes.is_empty() {
        println!("No scripts registered. Run 'th up' to start scripts.");
        return;
    }

    println!(
        "{:<20} {:<8} {:<16} {:<10} {:<8}",
        "NAME".bold(),
        "PID".bold(),
        "STATUS".bold(),
        "UPTIME".bold(),
        "RESTARTS".bold()
    );
    println!("{}", "-".repeat(66));
    for process in processes {
        let pid = if process.pid > 0 {
            process.pid.to_string()
        } else {
            "-".to_string()
        };
        println!(
            "{:<20} {:<8} {:<16} {:<10} {:<8}",
            process.name,
            pid,
            format_status(process.status, process.exit_code),
            format_duration(process.uptime),
            process.restart_count
        );
    }
}

#[tokio::main]
pub async fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        match e.downcast::<Error>() {
            Ok(err) => handle_error(err),
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let config_path = std::fs::canonicalize(&cli.config).unwrap_or(cli.config);

    match cli.command {
        Commands::Up { script_name } => {
            let response = commands::send_command(Command::Up { name: script_name, config_path }).await?;
            match response {
                Response::Success => println!("Scripts started successfully"),
                Response::Error(e) => eprintln!("Error: {}", e),
                _ => eprintln!("Unexpected response"),
            }
        }
        Commands::Down { script_name } => {
            let response = commands::send_command(Command::Down { name: script_name }).await?;
            match response {
                Response::Success => println!("Scripts stopped successfully"),
                Response::Error(e) => eprintln!("Error: {}", e),
                _ => eprintln!("Unexpected response"),
            }
        }
        Commands::Ps => {
            let response = commands::send_command(Command::Ps).await?;
            match response {
                Response::ProcessList(processes) => print_process_list_table(&processes),
                Response::Error(e) => eprintln!("Error retrieving process list: {}", e),
                _ => eprintln!("Unexpected response for ps command"),
            }
        }
        Commands::Logs {
            script_name,
            tail,
            follow,
        } => {
            if follow && script_name.is_none() {
                eprintln!("Error: --follow requires a script name");
                std::process::exit(1);
            }

            let cmd = Command::Logs {
                name: script_name.clone(),
                tail,
                follow,
            };

            if follow {
                commands::send_command_follow(cmd, |chunk| {
                    print!("{}", chunk);
                })
                .await?;
            } else {
                let response = commands::send_command(cmd).await?;
                let label = script_name.as_deref().unwrap_or("all scripts");
                match response {
                    Response::Logs(logs) if logs.is_empty() => {
                        println!("No logs available for {}", label);
                    }
                    Response::Logs(logs) => {
                        if script_name.is_some() {
                            println!("=== Logs for {} ===\n{}", label, logs);
                        } else {
                            println!("{}", logs);
                        }
                    }
                    Response::Error(e) => eprintln!("Error: {}", e),
                    _ => eprintln!("Unexpected response"),
                }
            }
        }
        Commands::Reload => {
            let response = commands::send_command(Command::Reload).await?;
            match response {
                Response::Success => println!("Configuration reloaded"),
                Response::Error(e) => eprintln!("Reload error: {}", e),
                _ => eprintln!("Unexpected response"),
            }
        }
        Commands::Install { http_port } => {
            service::install(http_port)?;
        }
        Commands::Uninstall => {
            service::uninstall()?;
        }
    }

    Ok(())
}
