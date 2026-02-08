use std::path::PathBuf;
use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::*;
use futures::future;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use turtle_harbor::client::commands;
use turtle_harbor::client::error::handle_error;
use turtle_harbor::common::config::{Config, Script};
use turtle_harbor::common::error::Error;
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

fn init_cli_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("turtle_harbor=info,cli=info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(true)
        .init();
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
        tracing::info!("Executing command for script {}", name);
        let script = config
            .scripts
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("Script {} not found", name))?;
        execute_for_script(&name, script, command_creator).await?;
    } else {
        tracing::info!("Executing command for all scripts");
        let tasks = config.scripts.iter().map(|(name, script)| {
            tracing::debug!("Preparing task for script {}", name);
            execute_for_script(name, script, command_creator)
        });
        let results = future::join_all(tasks).await;
        if let Some(err) = results.into_iter().find_map(|r| r.err()) {
            return Err(err);
        }
    }
    Ok(())
}

fn handle_response(name: &str, response: Response) {
    match response {
        Response::Success => tracing::info!("Script {} executed successfully", name),
        Response::Error(e) => tracing::error!("Error for script {}: {}", name, e),
        Response::Logs(logs) => {
            if logs.is_empty() {
                tracing::info!("No logs available for {}", name);
            } else {
                println!("=== Logs for {} ===\n{}", name, logs);
            }
        }
        _ => tracing::error!("Unexpected response for script {}", name),
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
pub async fn main() {
    init_cli_logging();
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
    let config = Config::load(&cli.config)?;

    match cli.command {
        Commands::Up { script_name } => {
            process_command_for_scripts(script_name, &config, |name, script| {
                let path = std::fs::canonicalize(&script.command)
                    .unwrap_or_else(|_| PathBuf::from(&script.command));

                Command::Up {
                    restart_policy: script.restart_policy.clone(),
                    command: path.to_string_lossy().to_string(),
                    max_restarts: script.max_restarts,
                    name: name.to_string(),
                    cron: script.cron.clone(),
                }
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
