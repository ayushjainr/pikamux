//! Windows is a view of one explicitly bound authority, never another assistant.
use crate::client_bridge::{ClientConfig, ClientNode, validate_client_ssh_target};
use crate::client_cli::ClientCliRuntime;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Write, path::Path, time::Duration};

pub const CAPABILITY: &str = "assistant-attach-v1";
pub const PROTOCOL: &str = "pika-assistant-attach";
pub const PROTOCOL_VERSION: i64 = 1;
const MAX_STATE: usize = 1024 * 1024;
const MAX_DRAFT: usize = 16 * 1024;
const MAX_SNAPSHOT: usize = crate::client_bridge::MAX_BRIDGE_MESSAGE_BYTES;

#[derive(clap::Args, Debug, Default)]
pub struct Args {
    /// Bind only this already paired node; requires its immutable profile ID.
    #[arg(long, requires = "profile")]
    pub bind_node: Option<String>,
    #[arg(long, requires = "bind_node")]
    pub profile: Option<String>,
    /// Refresh dated state without opening a window or calling a model.
    #[arg(long)]
    pub json: bool,
    /// Keep text locally as an unsent draft. Never automatically submitted.
    #[arg(long, conflicts_with = "clear_draft")]
    pub draft: Option<String>,
    #[arg(long)]
    pub clear_draft: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub node_id: String,
    pub profile_id: String,
    pub ssh_target: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default)]
    pub revision: u64,
    pub binding: Option<Binding>,
    /// When this client fetched the cache; source freshness stays in snapshot.
    pub cached_at: Option<u64>,
    pub snapshot: Option<Value>,
    pub draft: Option<String>,
}

fn exact_uuid(value: &str) -> Result<()> {
    if uuid::Uuid::parse_str(value)?.to_string() != value {
        bail!("Assistant identity must be a canonical UUID");
    }
    Ok(())
}

impl Binding {
    pub fn node<'a>(&self, config: &'a ClientConfig) -> Result<&'a ClientNode> {
        exact_uuid(&self.node_id)?;
        exact_uuid(&self.profile_id)?;
        validate_client_ssh_target(&self.ssh_target)?;
        let node = config
            .nodes
            .get(&self.node_id)
            .context("Assistant authority is no longer paired; no fallback was selected")?;
        if node.node_id != self.node_id || node.ssh_target != self.ssh_target {
            bail!("Assistant authority route changed; explicitly rebind before connecting");
        }
        Ok(node)
    }

    pub fn arguments(&self, snapshot: bool) -> Vec<String> {
        let mut args = vec![
            "_assistant-client".into(),
            "--expected-node-id".into(),
            self.node_id.clone(),
            "--expected-profile-id".into(),
            self.profile_id.clone(),
            "--scope".into(),
            "personal".into(),
        ];
        if snapshot {
            args.push("--json".into());
        }
        args
    }
}

pub fn terminal_command(binding: &Binding, config: &ClientConfig) -> Result<Vec<String>> {
    let node = binding.node(config)?;
    Ok(vec![
        "wt.exe".into(),
        "-w".into(),
        "new".into(),
        "new-tab".into(),
        "--title".into(),
        "Pika · your assistant".into(),
        "ssh.exe".into(),
        "-tt".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ClearAllForwardings=yes".into(),
        "-o".into(),
        "RemoteCommand=none".into(),
        "-o".into(),
        "ConnectTimeout=8".into(),
        node.ssh_target.clone(),
        crate::fleet::remote_pika_command(&binding.arguments(false))?,
    ])
}

fn validate_snapshot(binding: &Binding, value: Value) -> Result<Value> {
    if value["protocol"].as_str() != Some(PROTOCOL)
        || value["version"].as_i64() != Some(PROTOCOL_VERSION)
        || !value["capabilities"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(CAPABILITY)))
    {
        bail!("Chosen authority does not advertise compatible exact assistant attachment");
    }
    if value["node_id"].as_str() != Some(&binding.node_id)
        || value["profile_id"].as_str() != Some(&binding.profile_id)
        || !(value["snapshot"].is_object() || value.get("snapshot") == Some(&Value::Null))
    {
        bail!("Assistant authority/profile did not match; cached state was not replaced");
    }
    if serde_json::to_vec(&value)?.len() + 1 > MAX_SNAPSHOT {
        bail!("Assistant snapshot exceeded the cache bound");
    }
    Ok(value["snapshot"].clone())
}

fn refresh(
    runtime: &mut impl ClientCliRuntime,
    binding: &Binding,
    config: &ClientConfig,
) -> Result<Value> {
    let node = binding.node(config)?;
    let reply = runtime.ssh_json(
        &node.ssh_target,
        &binding.arguments(true),
        &json!({}),
        "ssh.exe",
        Duration::from_secs(12),
    )?;
    validate_snapshot(binding, reply)
}

pub fn run(
    runtime: &mut impl ClientCliRuntime,
    args: Args,
    output: &mut impl Write,
) -> Result<i32> {
    let (config, mut state) = prepare_state(runtime, &args)?;
    if save_draft(runtime, &mut state, &args)? {
        writeln!(
            output,
            "Local draft only; nothing sent. {}",
            state.draft.as_deref().unwrap_or("Draft cleared.")
        )?;
        return Ok(0);
    }
    if args.bind_node.is_some() {
        writeln!(output, "{}", serde_json::to_string_pretty(&state)?)?;
        return Ok(0);
    }
    refresh_and_open(runtime, &args, &config, &mut state, output)
}

fn prepare_state(
    runtime: &mut impl ClientCliRuntime,
    args: &Args,
) -> Result<(ClientConfig, State)> {
    let config = runtime.load_config()?;
    let mut state = runtime.load_assistant_state()?;
    if let Some(node_id) = args.bind_node.as_ref() {
        bind(
            runtime,
            &config,
            &mut state,
            node_id,
            args.profile.as_deref().context("Profile required")?,
        )?;
    }
    state.binding.as_ref().context("Choose your assistant authority explicitly: pika pika --bind-node NODE_UUID --profile PROFILE_UUID. Pair the host with pika setup first; pika status shows full node IDs. Read profile_id from pika pika --json on your chosen host.")?;
    Ok((config, state))
}

fn refresh_and_open(
    runtime: &mut impl ClientCliRuntime,
    args: &Args,
    config: &ClientConfig,
    state: &mut State,
    output: &mut impl Write,
) -> Result<i32> {
    let binding = state
        .binding
        .clone()
        .context("Assistant authority is not bound")?;
    match refresh(runtime, &binding, config) {
        Ok(snapshot) => {
            update_cache(state, snapshot);
            runtime.save_assistant_state(state)?;
        }
        Err(error) => {
            show_offline(output, state, &error.to_string(), args.json)?;
            return Ok(1);
        }
    }
    present_online(runtime, args, state, &binding, output)?;
    Ok(0)
}

fn present_online(
    runtime: &mut impl ClientCliRuntime,
    args: &Args,
    state: &State,
    binding: &Binding,
    output: &mut impl Write,
) -> Result<()> {
    if args.json {
        writeln!(
            output,
            "{}",
            serde_json::to_string_pretty(&json!({"availability":"online","state":state}))?
        )?;
    } else {
        let current_pairings = runtime.load_config()?;
        runtime.launch_assistant(&terminal_command(binding, &current_pairings)?)?;
        writeln!(
            output,
            "Assistant window launched for {} / {}. Check that window for verified attachment. Local drafts are never automatically sent.",
            binding.node_id, binding.profile_id
        )?;
    }
    Ok(())
}

fn bind(
    runtime: &mut impl ClientCliRuntime,
    config: &ClientConfig,
    state: &mut State,
    node_id: &str,
    profile_id: &str,
) -> Result<()> {
    let node = config
        .nodes
        .get(node_id)
        .context("Choose an already paired node")?;
    let binding = Binding {
        node_id: node_id.into(),
        profile_id: profile_id.into(),
        ssh_target: node.ssh_target.clone(),
    };
    if state.binding.as_ref().is_some_and(|old| old != &binding) && state.draft.is_some() {
        bail!(
            "Clear your unsent draft before changing assistant authority (copy it first if needed)"
        );
    }
    let snapshot = refresh(runtime, &binding, config)?;
    *state = State {
        revision: state.revision,
        binding: Some(binding),
        draft: state.draft.take(),
        ..State::default()
    };
    update_cache(state, snapshot);
    runtime.save_assistant_state(state)
}

fn update_cache(state: &mut State, snapshot: Value) {
    state.snapshot = Some(snapshot);
    state.cached_at = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
}

fn save_draft(runtime: &mut impl ClientCliRuntime, state: &mut State, args: &Args) -> Result<bool> {
    if args
        .draft
        .as_ref()
        .is_some_and(|draft| draft.len() > MAX_DRAFT)
    {
        bail!("Draft exceeds 16 KiB");
    }
    if args.draft.is_none() && !args.clear_draft {
        return Ok(false);
    }
    state.draft = args.draft.clone();
    runtime.save_assistant_state(state)?;
    Ok(true)
}

fn show_offline(
    output: &mut impl Write,
    state: &State,
    reason: &str,
    json_output: bool,
) -> Result<()> {
    if json_output {
        writeln!(
            output,
            "{}",
            serde_json::to_string_pretty(
                &json!({"availability":"offline","error":reason,"state":state,"actions_queued":false})
            )?
        )?;
        return Ok(());
    }
    writeln!(
        output,
        "Assistant unavailable: {reason:?}\nOFFLINE · cached state only (cached_at is the last fetch in Unix UTC seconds; source freshness remains inside snapshot); no actions queued, no alternate authority, no local assistant.\n{}",
        serde_json::to_string_pretty(state)?
    )?;
    Ok(())
}

fn connection(path: &Path) -> Result<rusqlite::Connection> {
    crate::assistant_storage::database(path)?;
    let db = rusqlite::Connection::open(path)?;
    db.busy_timeout(Duration::from_millis(500))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS assistant_client_state(id INTEGER PRIMARY KEY CHECK(id=1), body TEXT NOT NULL)")?;
    Ok(db)
}

pub fn load(path: &Path) -> Result<State> {
    use rusqlite::OptionalExtension;
    let db = connection(path)?;
    let body: Option<String> = db
        .query_row(
            "SELECT body FROM assistant_client_state WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(body) = body else {
        return Ok(State::default());
    };
    if body.len() > MAX_STATE {
        bail!("Assistant client state exceeds bound");
    }
    Ok(serde_json::from_str(&body)?)
}

pub fn save(path: &Path, state: &mut State) -> Result<()> {
    use rusqlite::OptionalExtension;
    let mut db = connection(path)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT body FROM assistant_client_state WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let revision = previous
        .map(|body| serde_json::from_str::<State>(&body))
        .transpose()?
        .map_or(0, |state| state.revision);
    if revision != state.revision {
        bail!(
            "Assistant client state changed in another window; reload before retrying. Nothing was sent."
        );
    }
    let mut next = state.clone();
    next.revision = revision
        .checked_add(1)
        .context("Assistant client revision exhausted")?;
    let body = serde_json::to_string(&next)?;
    if body.len() > MAX_STATE {
        bail!("Assistant client state exceeds bound");
    }
    tx.execute("INSERT INTO assistant_client_state VALUES(1,?) ON CONFLICT(id) DO UPDATE SET body=excluded.body", [body])?;
    tx.commit()?;
    *state = next;
    Ok(())
}
