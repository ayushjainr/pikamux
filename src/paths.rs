use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub config: PathBuf,
    pub database: PathBuf,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub opencode_data_home: PathBuf,
    pub opencode_config_home: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = directories::BaseDirs::new().context("cannot determine home directory")?;
        let config_dir = std::env::var_os("PIKA_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|p| PathBuf::from(p).join("pika")))
            .unwrap_or_else(|| home.home_dir().join(".config/pika"));
        let state_dir = std::env::var_os("PIKA_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("pika")))
            .unwrap_or_else(|| home.home_dir().join(".local/state/pika"));
        let database = std::env::var_os("PIKA_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("pika.db"));
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.home_dir().join(".codex"));
        let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.home_dir().join(".claude"));
        let opencode_data_home = std::env::var_os("OPENCODE_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_DATA_HOME").map(|p| PathBuf::from(p).join("opencode"))
            })
            .unwrap_or_else(|| home.home_dir().join(".local/share/opencode"));
        let opencode_config_home = std::env::var_os("OPENCODE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME").map(|p| PathBuf::from(p).join("opencode"))
            })
            .unwrap_or_else(|| home.home_dir().join(".config/opencode"));
        Ok(Self {
            config: config_dir.join("config.json"),
            config_dir,
            state_dir,
            database,
            codex_home,
            claude_home,
            opencode_data_home,
            opencode_config_home,
        })
    }
}
