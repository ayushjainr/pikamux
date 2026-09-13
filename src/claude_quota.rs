//! Claude's documented status-line feed; persist quota only, never the input payload.
use crate::{expert_refresh::QuotaSnapshot, model::Provider};
use anyhow::{Result, bail};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
};

const CACHE: &str = ".pika-quota.json";
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_INPUT: u64 = 1024 * 1024;

pub(crate) fn configure(settings: &mut Value, executable: &Path) -> Result<()> {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let previous = settings.get("statusLine").filter(|v| !v.is_null());
    let command = previous
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str);
    if previous.is_some()
        && (previous.and_then(|v| v.get("type")).and_then(Value::as_str) != Some("command")
            || command.is_none())
    {
        bail!(
            "Claude statusLine is not a command; preserve it and review quota integration manually"
        );
    }
    let mut args = vec![
        executable.to_string_lossy().into_owned(),
        "_claude-statusline".into(),
    ];
    if let Some(command) = command {
        let parsed = shell_words::split(command).unwrap_or_default();
        if parsed.get(1).map(String::as_str) == Some("_claude-statusline") {
            args.extend(parsed.into_iter().skip(2));
        } else {
            args.extend(["--forward".into(), command.into()]);
        }
    }
    let mut line = previous
        .cloned()
        .unwrap_or_else(|| json!({"type":"command"}));
    line["command"] = json!(
        args.iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    );
    settings["statusLine"] = line;
    Ok(())
}

fn parse(value: &Value, at: f64) -> Option<QuotaSnapshot> {
    let weekly = value.get("rate_limits")?.get("seven_day")?;
    let used = weekly.get("used_percentage")?.as_f64()?;
    let reset = weekly.get("resets_at")?.as_i64()?;
    if !used.is_finite()
        || !(0.0..=100.0).contains(&used)
        || reset as f64 <= at
        || reset as f64 > at + 8.0 * 86400.0
    {
        return None;
    }
    Some(QuotaSnapshot {
        provider: Provider::Claude,
        used_percent: used,
        reset_at: reset,
        observed_at: at,
        source: "Claude status-line feed".into(),
    })
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn capture(home: &Path, input: &[u8], at: f64) -> Result<()> {
    if input.len() as u64 > MAX_INPUT {
        bail!("Status-line payload exceeds limit");
    }
    let value: Value = serde_json::from_slice(input)?;
    let Some(reading) = parse(&value, at) else {
        return Ok(());
    };
    let target = home.join(CACHE);
    // The provider owns its directory. Never create or redirect it from a callback.
    if !fs::symlink_metadata(home)?.is_dir() {
        bail!("Claude home is not a real directory");
    }
    if fs::symlink_metadata(&target).is_ok_and(|v| !v.is_file()) {
        bail!("Quota cache is not a regular file");
    }
    let mut lock_options = OpenOptions::new();
    lock_options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    lock_options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let lock = lock_options.open(home.join(".pika-quota.lock"))?;
    if !lock.metadata()?.is_file() || fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let stage = home.join(format!(".pika-quota-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = options.open(&stage)?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec(&reading)?)?;
        file.sync_all()?;
        fs::rename(&stage, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
}

pub(crate) fn read(home: &Path, at: f64) -> Option<QuotaSnapshot> {
    let path = home.join(CACHE);
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 4096 {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let row: QuotaSnapshot = serde_json::from_slice(&bytes).ok()?;
    if row.provider != Provider::Claude
        || !row.observed_at.is_finite()
        || !(0.0..=1800.0).contains(&(at - row.observed_at))
    {
        return None;
    }
    parse(
        &json!({"rate_limits":{"seven_day":{"used_percentage":row.used_percent,"resets_at":row.reset_at}}}),
        at,
    )?;
    Some(row)
}

#[cfg(unix)]
pub(crate) fn run(forward: Option<String>) -> Result<i32> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INPUT {
        return Ok(0);
    }
    if let Ok(paths) = crate::paths::Paths::discover() {
        // Advisory capture failure must never replace the user's status line with an error.
        let _ = capture(&paths.claude_home, &bytes, crate::quota::now());
    }
    if let Some(command) = forward {
        let mut child = std::process::Command::new("sh");
        child.args(["-c", &command]);
        let output = crate::fleet::run_bounded_command_cancellable(
            &mut child,
            Some(&bytes),
            std::time::Duration::from_secs(5),
            64 * 1024,
            4096,
            &crate::consult::CancellationToken::default(),
        )?;
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        return Ok(output.status.code().unwrap_or(1));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    const AT: f64 = 1_800_000_000.0;
    fn payload() -> Vec<u8> {
        serde_json::to_vec(&json!({"rate_limits":{"seven_day":{"used_percentage":28.0,"resets_at":AT as i64 + 86400}},"transcript_path":"private-path","session_id":"private-id","cwd":"private-project"})).unwrap()
    }
    #[test]
    fn documented_payload_is_collected_without_persisting_session_metadata() {
        let root = tempfile::tempdir().unwrap();
        capture(root.path(), &payload(), AT).unwrap();
        assert_eq!(read(root.path(), AT).unwrap().remaining_percent(), 72.0);
        let content = fs::read_to_string(root.path().join(CACHE)).unwrap();
        assert!(
            !content.contains("private-")
                && !content.contains("transcript")
                && !content.contains("session_id")
        );
        assert!(read(root.path(), AT + 1801.0).is_none());
    }
    #[test]
    fn missing_malformed_and_expired_quota_never_make_up_a_reading() {
        let root = tempfile::tempdir().unwrap();
        capture(root.path(), b"{}", AT).unwrap();
        assert!(!root.path().join(CACHE).exists());
        assert!(capture(root.path(), b"invalid", AT).is_err());
        assert!(capture(root.path(), &vec![0; MAX_INPUT as usize + 1], AT).is_err());
        let mut value: Value = serde_json::from_slice(&payload()).unwrap();
        value["rate_limits"]["seven_day"]["used_percentage"] = json!(101);
        capture(root.path(), &serde_json::to_vec(&value).unwrap(), AT).unwrap();
        assert!(!root.path().join(CACHE).exists());
        capture(root.path(), &payload(), AT).unwrap();
        assert!(read(root.path(), AT + 86400.0).is_none());
    }
    #[test]
    fn configuration_preserves_and_quotes_existing_command_without_nested_wrapping() {
        let prior = "printf '%s' \"hello $USER\"";
        let mut settings =
            json!({"model":"opus","statusLine":{"type":"command","command":prior,"padding":2}});
        configure(&mut settings, Path::new("/some path/pika")).unwrap();
        let args = shell_words::split(settings["statusLine"]["command"].as_str().unwrap()).unwrap();
        assert_eq!(
            args,
            vec!["/some path/pika", "_claude-statusline", "--forward", prior]
        );
        assert_eq!(settings["statusLine"]["padding"], 2);
        assert_eq!(settings["model"], "opus");
        let first = settings.clone();
        configure(&mut settings, Path::new("/some path/pika")).unwrap();
        assert_eq!(settings, first);
        let mut empty = json!({});
        configure(&mut empty, Path::new("/pika")).unwrap();
        assert!(
            !empty["statusLine"]["command"]
                .as_str()
                .unwrap()
                .contains("--forward")
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlink_cache_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("untouched");
        fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(CACHE)).unwrap();
        assert!(capture(root.path(), &payload(), AT).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "original");
    }
}
