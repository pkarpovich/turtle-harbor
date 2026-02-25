use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "pkarpovich/turtle-harbor";
const GITHUB_API: &str = "https://api.github.com";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
const ASSET_SUFFIX: &str = "aarch64-unknown-linux-gnu";
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const ASSET_SUFFIX: &str = "aarch64-apple-darwin";
#[cfg(all(target_arch = "x86_64", target_os = "macos"))]
const ASSET_SUFFIX: &str = "x86_64-apple-darwin";
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
const ASSET_SUFFIX: &str = "x86_64-unknown-linux-gnu";

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn normalize_version(v: &str) -> &str {
    v.strip_prefix('v').unwrap_or(v)
}

fn bin_dir() -> PathBuf {
    std::env::current_exe()
        .expect("unable to resolve current executable path")
        .parent()
        .expect("executable has no parent directory")
        .to_path_buf()
}

async fn fetch_latest_release() -> anyhow::Result<Release> {
    let url = format!("{}/repos/{}/releases/latest", GITHUB_API, REPO);
    let client = reqwest::Client::new();
    let release: Release = client
        .get(&url)
        .header("User-Agent", format!("turtle-harbor/{}", CURRENT_VERSION))
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(release)
}

async fn download_asset(url: &str) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::Client::new();
    let bytes = client
        .get(url)
        .header("User-Agent", format!("turtle-harbor/{}", CURRENT_VERSION))
        .header("Accept", "application/octet-stream")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    Ok(bytes.to_vec())
}

fn extract_binaries(tar_gz_data: &[u8], dest: &Path) -> anyhow::Result<()> {
    let decoder = flate2::read::GzDecoder::new(tar_gz_data);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if file_name != "th" && file_name != "turtled" {
            continue;
        }

        let dest_path = dest.join(file_name);
        let mut content = Vec::new();
        entry.read_to_end(&mut content)?;
        std::fs::write(&dest_path, &content)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(0o755))?;
        }
    }

    Ok(())
}

fn stop_daemon() {
    let socket_path = crate::common::paths::socket_path();
    if !socket_path.exists() {
        println!("Daemon not running (no socket)");
        return;
    }

    println!("Stopping scripts...");
    let bin = bin_dir().join("th");
    let _ = Command::new(&bin).arg("down").status();

    println!("Stopping daemon...");
    let _ = Command::new("pkill").arg("turtled").status();

    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let alive = Command::new("pgrep")
            .arg("turtled")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !alive {
            println!("Daemon stopped");
            return;
        }
    }

    println!("Force killing daemon...");
    let _ = Command::new("pkill").args(["-9", "turtled"]).status();
    std::thread::sleep(std::time::Duration::from_millis(500));
}

fn replace_binaries(temp_dir: &Path, target_dir: &Path) -> anyhow::Result<()> {
    for name in ["th", "turtled"] {
        let src = temp_dir.join(name);
        if !src.exists() {
            anyhow::bail!("extracted archive missing {}", name);
        }
        let dest = target_dir.join(name);
        println!("Installing {} -> {}", name, dest.display());

        let status = Command::new("sudo")
            .args(["cp", "-f"])
            .arg(&src)
            .arg(&dest)
            .status()?;

        if !status.success() {
            anyhow::bail!("sudo cp failed for {}", name);
        }
    }
    Ok(())
}

fn restart_daemon() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let status = Command::new("systemctl")
            .args(["--user", "restart", "turtle-harbor"])
            .status();

        if status.map(|s| s.success()).unwrap_or(false) {
            println!("Daemon restarted via systemd");
            return Ok(());
        }
    }

    let turtled = bin_dir().join("turtled");
    println!("Starting daemon: {}", turtled.display());
    Command::new(&turtled)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    Ok(())
}

pub async fn update() -> anyhow::Result<()> {
    println!("Current version: {}", CURRENT_VERSION);
    println!("Checking for updates...");

    let release = fetch_latest_release().await?;
    let latest = normalize_version(&release.tag_name);

    if latest == CURRENT_VERSION {
        println!("Already up to date ({})", CURRENT_VERSION);
        return Ok(());
    }

    println!("New version available: {} -> {}", CURRENT_VERSION, latest);

    let expected_name = format!("turtle-harbor-{}.tar.gz", ASSET_SUFFIX);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == expected_name)
        .ok_or_else(|| anyhow::anyhow!("no binary found for this platform ({})", ASSET_SUFFIX))?;

    println!("Downloading {}...", asset.name);
    let data = download_asset(&asset.browser_download_url).await?;
    println!("Downloaded {} bytes", data.len());

    let temp_dir = tempfile::tempdir()?;
    extract_binaries(&data, temp_dir.path())?;

    let target_dir = bin_dir();
    stop_daemon();
    replace_binaries(temp_dir.path(), &target_dir)?;
    restart_daemon()?;

    let output = Command::new(target_dir.join("turtled"))
        .arg("--version")
        .output();

    match output {
        Ok(out) => {
            let version = String::from_utf8_lossy(&out.stdout);
            println!("Updated successfully: {}", version.trim());
        }
        Err(_) => println!("Updated to {}", latest),
    }

    Ok(())
}
