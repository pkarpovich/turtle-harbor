use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::*;
use futures::future;
use std::time::Duration;
use turtle_harbor::client::commands;
use turtle_harbor::common::config::{Config, Script};
use turtle_harbor::common::ipc::{Command, ProcessInfo, ProcessStatus, Response};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
    #[arg(short, long, default_value = "scripts.yml")]
    pub config: String,
}

#[derive(Subcommand)]
pub enum Commands {
    Up { script_name: Option<String> },
    Down { script_name: Option<String> },
    Ps,
    Logs { script_name: Option<String> },
}

pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}

pub fn format_status(status: ProcessStatus) -> ColoredString {
    match status {
        ProcessStatus::Running => "running".green(),
        ProcessStatus::Stopped => "stopped".red(),
        ProcessStatus::Failed => "failed".red().bold(),
    }
}

async fn execute_for_script<F>(name: &str, script: &Script, command_creator: F) -> Result<()>
where
    F: Fn(&str, &Script) -> Command + Copy,
{
    let cmd = command_creator(name, script);
    let response = commands::send_command(cmd).await?;
    handle_response(name, response);
    Ok(())
}

async fn process_command_for_scripts<F>(
    script_name: Option<String>,
    config: &Config,
    command_creator: F,
) -> Result<()>
where
    F: Fn(&str, &Script) -> Command + Copy,
{
    if let Some(name) = script_name {
        let script = config
            .scripts
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("Script {} not found", name))?;
        execute_for_script(&name, script, command_creator).await?;
    } else {
        let tasks: Vec<_> = config
            .scripts
            .iter()
            .map(|(name, script)| execute_for_script(name, script, command_creator))
            .collect();
        future::join_all(tasks).await;
    }
    Ok(())
}

fn handle_response(name: &str, response: Response) {
    match response {
        Response::Success => println!("Script {} executed successfully", name),
        Response::Error(e) => eprintln!("Error for script {}: {}", name, e),
        Response::Logs(logs) => {
            println!("Logs for {}:", name);
            if logs.is_empty() {
                println!("No logs available");
            } else {
                println!("{}", logs);
            }
        }
        _ => eprintln!("Unexpected response for script {}", name),
    }
}

fn print_process_list_table(processes: &[ProcessInfo]) {
    println!(
        "{:<20} {:<6} {:<10} {:<10} {:<8}",
        "NAME".bold(),
        "PID".bold(),
        "STATUS".bold(),
        "UPTIME".bold(),
        "RESTARTS".bold()
    );
    println!("{}", "-".repeat(60));
    for process in processes {
        println!(
            "{:<20} {:<6} {:<10} {:<10} {:<8}",
            process.name,
            process.pid,
            format_status(process.status.clone()),
            format_duration(process.uptime),
            process.restart_count
        );
    }
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        Commands::Up { script_name } => {
            process_command_for_scripts(script_name, &config, |name, script| Command::Up {
                name: name.to_string(),
                command: script.command.clone(),
                restart_policy: script.restart_policy.clone(),
                max_restarts: script.max_restarts,
            })
            .await?;
        }
        Commands::Down { script_name } => {
            process_command_for_scripts(script_name, &config, |name, _| Command::Down {
                name: name.to_string(),
            })
            .await?;
        }
        Commands::Logs { script_name } => {
            process_command_for_scripts(script_name, &config, |name, _| Command::Logs {
                name: name.to_string(),
            })
            .await?;
        }
        Commands::Ps => {
            let response = commands::send_command(Command::Ps).await?;
            match response {
                Response::ProcessList(processes) => print_process_list_table(&processes),
                Response::Error(e) => eprintln!("Error retrieving process list: {}", e),
                _ => eprintln!("Unexpected response for ps command"),
            }
        }
    }

    Ok(())
}
