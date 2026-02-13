use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "macos")]
const LABEL: &str = "com.turtle-harbor.daemon";
#[cfg(target_os = "linux")]
const SERVICE_NAME: &str = "turtle-harbor";

fn turtled_bin_path() -> PathBuf {
    let current_exe = std::env::current_exe().expect("unable to resolve current executable path");
    current_exe.parent().unwrap().join("turtled")
}

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    dirs::home_dir()
        .expect("unable to resolve home directory")
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", LABEL))
}

#[cfg(target_os = "macos")]
fn generate_plist(turtled: &std::path::Path, http_port: Option<u16>) -> String {
    let log_dir = crate::common::paths::log_dir();
    let mut args = format!(
        "        <string>{}</string>",
        turtled.display()
    );
    if let Some(port) = http_port {
        args.push_str(&format!(
            "\n        <string>--http-port</string>\n        <string>{}</string>",
            port
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_dir}/daemon.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/daemon.stderr.log</string>
</dict>
</plist>
"#,
        label = LABEL,
        args = args,
        log_dir = log_dir.display(),
    )
}

#[cfg(target_os = "linux")]
fn unit_path() -> PathBuf {
    dirs::home_dir()
        .expect("unable to resolve home directory")
        .join(".config/systemd/user")
        .join(format!("{}.service", SERVICE_NAME))
}

#[cfg(target_os = "linux")]
fn generate_unit(turtled: &std::path::Path, http_port: Option<u16>) -> String {
    let mut exec_start = turtled.display().to_string();
    if let Some(port) = http_port {
        exec_start.push_str(&format!(" --http-port {}", port));
    }

    format!(
        r#"[Unit]
Description=Turtle Harbor daemon
After=network.target

[Service]
Type=simple
ExecStart={exec_start}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#,
        exec_start = exec_start,
    )
}

pub fn install(http_port: Option<u16>) -> anyhow::Result<()> {
    let turtled = turtled_bin_path();
    if !turtled.exists() {
        anyhow::bail!(
            "turtled binary not found at {}. Ensure it's in the same directory as th.",
            turtled.display()
        );
    }

    install_platform(&turtled, http_port)
}

#[cfg(target_os = "macos")]
fn install_platform(turtled: &std::path::Path, http_port: Option<u16>) -> anyhow::Result<()> {
    let plist = plist_path();
    let content = generate_plist(turtled, http_port);

    crate::common::paths::ensure_dir(plist.parent().unwrap())?;

    let already_loaded = Command::new("launchctl")
        .args(["list", LABEL])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if already_loaded {
        println!("Stopping existing service...");
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}", unsafe { libc::getuid() }), plist.to_str().unwrap()])
            .status();
    }

    let mut file = std::fs::File::create(&plist)?;
    file.write_all(content.as_bytes())?;
    println!("Wrote {}", plist.display());

    let status = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{}", unsafe { libc::getuid() }), plist.to_str().unwrap()])
        .status()?;

    if !status.success() {
        anyhow::bail!("launchctl bootstrap failed");
    }

    println!("Service installed and started.");
    println!("  Stop:      launchctl bootout gui/$(id -u) {}", plist.display());
    println!("  Uninstall: th uninstall");
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_platform(turtled: &std::path::Path, http_port: Option<u16>) -> anyhow::Result<()> {
    let unit = unit_path();
    let content = generate_unit(turtled, http_port);

    crate::common::paths::ensure_dir(unit.parent().unwrap())?;

    std::fs::write(&unit, content)?;
    println!("Wrote {}", unit.display());

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()?;
    if !reload.success() {
        anyhow::bail!("systemctl daemon-reload failed");
    }

    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", SERVICE_NAME])
        .status()?;
    if !enable.success() {
        anyhow::bail!("systemctl enable failed");
    }

    println!("Service installed and started.");
    println!("  Status:    systemctl --user status {}", SERVICE_NAME);
    println!("  Uninstall: th uninstall");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    uninstall_platform()
}

#[cfg(target_os = "macos")]
fn uninstall_platform() -> anyhow::Result<()> {
    let plist = plist_path();
    if !plist.exists() {
        println!("Service is not installed.");
        return Ok(());
    }

    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}", unsafe { libc::getuid() }), plist.to_str().unwrap()])
        .status();

    std::fs::remove_file(&plist)?;
    println!("Service stopped and removed.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_platform() -> anyhow::Result<()> {
    let unit = unit_path();
    if !unit.exists() {
        println!("Service is not installed.");
        return Ok(());
    }

    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", SERVICE_NAME])
        .status();

    std::fs::remove_file(&unit)?;

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    println!("Service stopped and removed.");
    Ok(())
}
