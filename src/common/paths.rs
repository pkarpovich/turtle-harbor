use std::path::PathBuf;

use crate::common::ipc::Profile;

fn data_dir() -> PathBuf {
    let base = dirs::data_dir().expect("unable to resolve data directory");
    base.join("turtle-harbor")
}

pub fn socket_path() -> PathBuf {
    match Profile::current() {
        Profile::Development => PathBuf::from("/tmp/turtle-harbor.sock"),
        Profile::Production => {
            if cfg!(target_os = "linux") {
                if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
                    return PathBuf::from(runtime_dir).join("turtle-harbor.sock");
                }
            }
            data_dir().join("daemon.sock")
        }
    }
}

pub fn state_file() -> PathBuf {
    match Profile::current() {
        Profile::Development => PathBuf::from("/tmp/turtle-harbor-state.json"),
        Profile::Production => data_dir().join("state.json"),
    }
}

pub fn log_dir() -> PathBuf {
    match Profile::current() {
        Profile::Development => PathBuf::from("logs"),
        Profile::Production => {
            if cfg!(target_os = "macos") {
                let home = dirs::home_dir().expect("unable to resolve home directory");
                return home.join("Library/Logs/turtle-harbor");
            }
            data_dir().join("logs")
        }
    }
}

pub fn ensure_dir(path: &std::path::Path) -> std::io::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}
