use crate::{model::Provider, paths::Paths};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub default_provider: Provider,
    pub alerts: String,
    pub peek_lines: usize,
    pub provider_executables: BTreeMap<String, String>,
    pub provider_runtime_path: Option<String>,
    pub client_bridges: Vec<serde_json::Value>,
    pub codex_worker_originators: Vec<String>,
    pub opencode_worker_title_prefixes: Vec<String>,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            default_provider: Provider::Codex,
            alerts: "tmux".into(),
            peek_lines: 200,
            provider_executables: BTreeMap::new(),
            provider_runtime_path: None,
            client_bridges: Vec::new(),
            codex_worker_originators: vec!["agentic_fund".into(), "quant_agent_autonomy".into()],
            opencode_worker_title_prefixes: vec!["agentic-fund:".into(), "quant-agent:".into()],
            additional: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        if !paths.config.is_file() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&paths.config)
            .with_context(|| format!("cannot read {}", paths.config.display()))?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in {}", paths.config.display()))?;
        if !value.is_object() {
            bail!("{} must contain a JSON object", paths.config.display());
        }
        serde_json::from_value(value)
            .with_context(|| format!("invalid Pika configuration in {}", paths.config.display()))
    }

    pub fn write(&self, paths: &Paths) -> Result<()> {
        secure_directory(&paths.config_dir)?;
        let name = paths
            .config
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("config.json");
        let temporary = paths
            .config
            .with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&temporary)
            .with_context(|| format!("cannot create {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut output, self)?;
        output.write_all(b"\n")?;
        output.sync_all()?;
        fs::rename(&temporary, &paths.config)
            .with_context(|| format!("cannot replace {}", paths.config.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn executable(&self, provider: Provider) -> String {
        self.provider_executables
            .get(provider.as_str())
            .cloned()
            .unwrap_or_else(|| provider.as_str().to_owned())
    }
}

fn secure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("cannot create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_round_trip() {
        let value = r#"{"version":1,"future":{"enabled":true}}"#;
        let config: Config = serde_json::from_str(value).unwrap();
        assert_eq!(config.additional["future"]["enabled"], true);
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["future"]["enabled"], true);
    }
}
