//! Read-only identity probe and exact-profile attachment over an existing SSH route.
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    io::{self, IsTerminal},
    path::Path,
};

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    #[arg(long)]
    pub expected_node_id: String,
    #[arg(long)]
    pub expected_profile_id: String,
    #[arg(long, default_value = "personal")]
    pub scope: String,
    #[arg(long)]
    pub json: bool,
}

fn canonical_id(id: &str) -> Result<()> {
    if uuid::Uuid::parse_str(id)?.to_string() != id {
        bail!("Expected a canonical assistant identity");
    }
    Ok(())
}

fn read_identity(path: &Path, query: &str) -> Result<String> {
    if !path.is_file() {
        bail!("This machine has no existing assistant authority; no replacement was created");
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(50))?;
    Ok(db.query_row(query, [], |row| row.get(0))?)
}

fn verify(paths: &crate::paths::Paths, args: &Args) -> Result<std::path::PathBuf> {
    canonical_id(&args.expected_node_id)?;
    canonical_id(&args.expected_profile_id)?;
    let root = paths.state_dir.join("assistant");
    let node = read_identity(
        &paths.database,
        "SELECT value FROM meta WHERE key='fleet:node_id'",
    )?;
    let memory = root.join("memory.sqlite");
    if !memory.is_file() {
        bail!("Assistant profile is missing; no replacement was created");
    }
    crate::assistant_storage::existing_database(&memory)?;
    let profile = read_identity(
        &memory,
        "SELECT value FROM memory_meta WHERE key='profile_id'",
    )?;
    if node != args.expected_node_id || profile != args.expected_profile_id {
        bail!("Assistant authority/profile changed; explicitly rebind before connecting");
    }
    Ok(root)
}

fn probe(root: &Path, args: &Args) -> Result<Value> {
    let scope = crate::assistant::scope(&args.scope)?;
    let cached =
        crate::assistant_presentation::read_cached(root, &scope, crate::assistant::timestamp())?;
    if cached
        .as_ref()
        .is_some_and(|view| view.cue.profile != args.expected_profile_id)
    {
        bail!("Cached briefing belongs to a different assistant profile");
    }
    let mut value = json!({"protocol":crate::assistant_client::PROTOCOL,"version":crate::assistant_client::PROTOCOL_VERSION,"capabilities":[crate::assistant_client::CAPABILITY],"node_id":args.expected_node_id,"profile_id":args.expected_profile_id,"snapshot":cached});
    // The SSH JSON transport has a 16 KiB envelope. Never truncate JSON or
    // silently return a partial briefing as if it were complete.
    if serde_json::to_vec(&value)?.len() + 1 > crate::client_bridge::MAX_BRIDGE_MESSAGE_BYTES {
        value["snapshot"] = json!({"cue":value["snapshot"]["cue"],"notice":"Briefing is larger than the attachment preview. Open Pika for the full bounded pages."});
    }
    Ok(value)
}

pub(crate) fn run(args: Args) -> Result<i32> {
    let paths = crate::paths::Paths::discover()?;
    let root = verify(&paths, &args)?;
    if args.json {
        println!("{}", probe(&root, &args)?);
        return Ok(0);
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Interactive assistant attachment requires a terminal");
    }
    let mut client =
        crate::assistant_host::Client::attach_existing_profile(&root, &args.expected_profile_id)?;
    let snapshot = client.request(json!({"operation":"snapshot","scope":args.scope}))?;
    snapshot["profile_id"]
        .as_str()
        .filter(|id| *id == args.expected_profile_id)
        .context("Assistant profile changed during attachment; no action was submitted")?;
    verify(&paths, &args)?;
    crate::assistant::interactive_view(&mut client, &args.scope, None)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_identity_probe_does_not_create_a_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.sqlite");
        assert!(read_identity(&path, "SELECT 1").is_err());
        assert!(!path.exists());
    }
    #[test]
    fn uuid_validation_does_not_allow_aliases() {
        assert!(canonical_id("master").is_err());
        assert!(canonical_id("AAAAAAAA-0000-4000-8000-000000000001").is_err());
        assert!(canonical_id("aaaaaaaa-0000-4000-8000-000000000001").is_ok());
    }
}
