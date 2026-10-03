//! The native provider owns the conversation. Pika owns only its exact home
//! and the existing scoped continuity services exposed to that provider.
use crate::{
    assistant::Args,
    core::{LaunchContext, OpenReceipt, Pika},
    model::Provider,
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{IsTerminal, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub(crate) fn wants_native(args: &Args) -> bool {
    !args.json
        && !args.offline
        && args.remember.is_none()
        && args.decision.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

pub(crate) fn run(args: Args) -> Result<i32> {
    let ordinary = Pika::discover()?;
    let entry = select_entry(&ordinary, &args)?;
    let Entry {
        root,
        scope,
        profile_id,
        executable,
        max_calls,
    } = entry;
    let native_executable = executable.canonicalize()?;
    let (lock, mut binding, mut profile) =
        prepare_home(&root, &profile_id, &scope, &executable, &native_executable)?;
    let native = native_registry(&ordinary, &root, &native_executable, &mut profile)?;
    require_finished_shared_setup(&binding)?;
    let receipt = launch_bound_home(&native, &root, &native_executable, &profile, &mut binding)?;
    confirm_first_launch(&native, &root, &binding)?;
    let selection = crate::assistant_startup::Selection {
        profile_root: root,
        profile_id,
        scope,
        executable,
        max_calls,
    };
    if args.profile_root.is_none() || args.set_default {
        crate::assistant_startup::save(&crate::assistant_startup::path()?, &selection)?;
    }
    drop(lock);
    attach_native_receipt(&native, receipt)
}

fn attach_native_receipt(native: &Pika, receipt: OpenReceipt) -> Result<i32> {
    let receipt = match receipt.target {
        crate::core::OpenTarget::Session(session) => native.open_session(*session, true)?,
        crate::core::OpenTarget::Pending(pending) => {
            native.open_pending(&pending.launch_token, true)?
        }
    };
    Ok(receipt.exit_code)
}

fn require_finished_shared_setup(binding: &Binding) -> Result<()> {
    if binding.shared_creation && binding.thread_id.is_some() && !binding.shared_ready {
        bail!(
            "The assistant's exact UUID was created but its initial provider setup is unfinished. Recover that same identity before reopening; no replacement was created."
        );
    }
    Ok(())
}

fn launch_bound_home(
    native: &Pika,
    root: &Path,
    native_executable: &Path,
    profile: &crate::assistant_native_profile::LaunchProfile,
    binding: &mut Binding,
) -> Result<OpenReceipt> {
    let stored_session = binding
        .thread_id
        .as_deref()
        .map(|thread| native.store.get_session(Provider::Codex, thread))
        .transpose()?
        .flatten();
    Ok(
        if binding.thread_id.is_none() && native.store.get_pending(&binding.launch_token)?.is_none()
        {
            reserve_shared_thread(native, root, native_executable, profile, binding)?;
            native.new_shared_codex_home(
                "Pika",
                &binding.launch_token,
                binding.thread_id.as_deref().expect("reserved identity"),
            )?
        } else if binding.shared_creation
            && native.store.get_pending(&binding.launch_token)?.is_none()
            && binding.thread_id.is_some()
            && stored_session.is_none()
        {
            recover_reserved_home(native, profile, binding)?
        } else {
            open_exact(native, binding)?
        },
    )
}

fn recover_reserved_home(
    native: &Pika,
    profile: &crate::assistant_native_profile::LaunchProfile,
    binding: &Binding,
) -> Result<OpenReceipt> {
    let mut provider = crate::assistant_native_profile::connect_shared_provider(profile)?;
    let thread = binding.thread_id.as_deref().expect("saved shared identity");
    let loaded = provider.rpc(
        "thread/read",
        serde_json::json!({"threadId":thread,"includeTurns":true}),
    )?;
    if loaded["thread"]["id"] != thread {
        bail!("The private provider did not confirm the reserved assistant identity");
    }
    native.new_shared_codex_home("Pika", &binding.launch_token, thread)
}

fn prepare_home(
    root: &Path,
    profile_id: &str,
    scope: &str,
    executable: &Path,
    native_executable: &Path,
) -> Result<(
    File,
    Binding,
    crate::assistant_native_profile::LaunchProfile,
)> {
    let lock = launch_lock(root)?;
    let mut binding = bind_scope(root, profile_id, scope)?;
    require_epoch(root, &binding)?;
    crate::assistant_native_recovery::require_context(
        root,
        profile_id,
        scope,
        binding.thread_id.as_deref(),
    )?;
    binding.provider_executable = Some(executable.to_owned());
    save_binding(root, &binding)?;
    let mut profile = crate::assistant_native_profile::prepare(
        root,
        profile_id,
        scope,
        &std::env::current_exe()?,
        native_executable,
    )?;
    profile.environment.insert(
        "PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN".into(),
        binding.launch_token.clone(),
    );
    crate::assistant_native_profile::validate_provider(native_executable, &profile)?;
    if binding.thread_id.is_some()
        || crate::store::Store::at(root.join("native-registry/pika.db"))
            .get_pending(&binding.launch_token)?
            .is_none()
    {
        crate::assistant_native_profile::stage_shared_resume(root, &mut profile)?;
        crate::assistant_native_profile::validate_provider(native_executable, &profile)?;
    }
    Ok((lock, binding, profile))
}

struct Entry {
    root: PathBuf,
    scope: String,
    profile_id: String,
    executable: PathBuf,
    max_calls: u64,
}

fn select_entry(ordinary: &Pika, args: &Args) -> Result<Entry> {
    let selected =
        crate::assistant_startup::load(&crate::assistant_startup::path()?)?.filter(|selection| {
            args.profile_root
                .as_ref()
                .is_none_or(|root| root == &selection.profile_root)
        });
    let root = args
        .profile_root
        .clone()
        .or_else(|| {
            selected
                .as_ref()
                .map(|selection| selection.profile_root.clone())
        })
        .unwrap_or_else(|| ordinary.paths.state_dir.join("assistant"));
    let scope = if args.scope.is_empty() {
        selected
            .as_ref()
            .map(|selection| selection.scope.clone())
            .unwrap_or_else(|| "personal".into())
    } else {
        args.scope.clone()
    };
    crate::assistant::scope(&scope)?;
    let expected = args.expected_profile_id.as_deref().or_else(|| {
        selected
            .as_ref()
            .map(|selection| selection.profile_id.as_str())
    });
    if let Some(expected) = expected {
        // Missing selected authority must never create an empty replacement.
        crate::assistant_host::verify_existing_profile(&root, expected)?;
    }
    let memory = crate::assistant_memory::Store::open(root.join("memory.sqlite"))?;
    let profile_id = memory.profile_id().to_owned();
    drop(memory);
    let max_calls = configure_policy(&root, args, selected.as_ref())?;
    let executable = selected_executable(ordinary, args, selected.as_ref())?;
    Ok(Entry {
        root,
        scope,
        profile_id,
        executable,
        max_calls,
    })
}

fn configure_policy(
    root: &Path,
    args: &Args,
    selected: Option<&crate::assistant_startup::Selection>,
) -> Result<u64> {
    let max_calls = if args.no_call_limit {
        crate::assistant_policy::NO_CALL_LIMIT
    } else {
        args.max_calls
            .or_else(|| selected.map(|value| value.max_calls))
            .unwrap_or(crate::assistant_policy::NO_CALL_LIMIT)
    };
    if max_calls != crate::assistant_policy::NO_CALL_LIMIT {
        bail!(
            "Codex owns model billing in the native conversation and cannot enforce Pika's per-model-call ceiling. No conversation was launched; the saved limit was not changed."
        );
    }
    let mut policy = crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite"))?;
    let mut config = policy.config()?;
    config.max_total_calls = max_calls;
    policy.configure(&config)?;
    Ok(max_calls)
}

fn selected_executable(
    ordinary: &Pika,
    args: &Args,
    selected: Option<&crate::assistant_startup::Selection>,
) -> Result<PathBuf> {
    let executable = args
        .enable_codex
        .clone()
        .or_else(|| selected.map(|selection| selection.executable.clone()))
        .map(Ok)
        .unwrap_or_else(|| resolve_executable(&ordinary.config.executable(Provider::Codex)))?;
    if !executable.is_absolute() || !executable.is_file() {
        bail!(
            "Pika could not find Codex. Install Codex or reconnect its saved executable; your memory is unchanged."
        );
    }
    Ok(executable)
}

fn confirm_first_launch(native: &Pika, root: &Path, binding: &Binding) -> Result<()> {
    if binding.thread_id.is_none()
        && let Some((Provider::Codex, thread)) =
            native.store.get_launch_binding(&binding.launch_token)?
    {
        require_epoch(root, binding)?;
        let confirmed = Binding {
            thread_id: Some(thread),
            ..binding.clone()
        };
        crate::assistant_native_recovery::record_context(
            root,
            &binding.profile_id,
            &binding.scope,
            confirmed.thread_id.as_deref().expect("confirmed thread"),
        )?;
        save_binding(root, &confirmed)?;
    }
    Ok(())
}

fn resolve_executable(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    if path.components().count() != 1 {
        bail!("Saved Codex executable must be an absolute path");
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|parent| parent.join(path))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.canonicalize())
        .transpose()?
        .context("Codex is not installed. Install Codex, then open Pika again; no conversation was created.")
}

fn launch_lock(root: &Path) -> Result<File> {
    let path = root.join("native-launch.lock");
    crate::assistant_storage::file(&path)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    lock.try_lock_exclusive()
        .context("Pika is opening in another window. No second conversation was started.")?;
    Ok(lock)
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct Binding {
    profile_id: String,
    scope: String,
    launch_token: String,
    thread_id: Option<String>,
    memory_epoch: u64,
    #[serde(default)]
    provider_executable: Option<PathBuf>,
    #[serde(default)]
    shared_creation: bool,
    #[serde(default)]
    shared_ready: bool,
}

fn bind_scope(root: &Path, profile_id: &str, scope: &str) -> Result<Binding> {
    let path = root.join("native-binding.json");
    if path.try_exists()? {
        crate::assistant_storage::existing_file(&path)?;
        let existing: Binding = serde_json::from_slice(&std::fs::read(&path)?)?;
        if existing.profile_id != profile_id || existing.scope != scope {
            bail!(
                "Pika's native conversation belongs to a different profile or memory scope. Its context was not replaced."
            );
        }
        uuid::Uuid::parse_str(&existing.launch_token)?;
        if let Some(thread) = &existing.thread_id {
            uuid::Uuid::parse_str(thread)?;
        }
        return Ok(existing);
    }
    let binding = Binding {
        profile_id: profile_id.into(),
        scope: scope.into(),
        launch_token: uuid::Uuid::new_v4().to_string(),
        thread_id: previous_thread(root, profile_id, scope)?,
        memory_epoch: crate::assistant_memory::Store::open(root.join("memory.sqlite"))?
            .forget_epoch()?,
        provider_executable: None,
        shared_creation: false,
        shared_ready: false,
    };
    save_binding(root, &binding)?;
    Ok(binding)
}

fn save_binding(root: &Path, binding: &Binding) -> Result<()> {
    let path = root.join("native-binding.json");
    let temporary = root.join(format!(".native-binding-{}", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec(&binding)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Adopt only this profile's previously recorded exact provider thread. Read
/// recovery metadata, never search provider history by title.
fn previous_thread(root: &Path, profile_id: &str, scope: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let path = root.join("runtime.sqlite");
    if !path.try_exists()? {
        return Ok(None);
    }
    crate::assistant_storage::existing_database(&path)?;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let legacy: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='assistant_runtime_profile')",
        [],
        |row| row.get(0),
    )?;
    if !legacy {
        return Ok(None);
    }
    let row: Option<(String, Option<String>)> = db
        .query_row(
            "SELECT profile_id,thread_id FROM assistant_runtime_profile WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((actual, thread)) = row else {
        return Ok(None);
    };
    if actual != profile_id {
        bail!("Saved Pika conversation belongs to a different profile; no replacement was created");
    }
    require_legacy_context(&db, root, scope)?;
    if let Some(thread) = &thread {
        uuid::Uuid::parse_str(thread)?;
    }
    Ok(thread)
}

fn require_legacy_context(db: &rusqlite::Connection, root: &Path, scope: &str) -> Result<()> {
    let saved_scope: String = db.query_row(
        "SELECT scope FROM assistant_runtime_scope WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    if serde_json::from_str::<crate::assistant_memory::Scope>(&saved_scope)?
        != crate::assistant::scope(scope)?
    {
        bail!(
            "Saved Pika conversation belongs to another memory scope; no replacement was created"
        );
    }
    let blocked: bool = db.query_row(
        "SELECT blocked FROM assistant_runtime_guard WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    let saved_epoch: u64 = db.query_row(
        "SELECT epoch FROM assistant_runtime_epoch WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    let current_epoch =
        crate::assistant_memory::Store::open(root.join("memory.sqlite"))?.forget_epoch()?;
    let unknown: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM assistant_runtime_turns WHERE state IN ('reserved','dispatch_intent','in_flight','unknown'))", [], |row| row.get(0))?;
    if blocked || unknown || saved_epoch != current_epoch {
        bail!(
            "Pika has unresolved earlier work. Its exact conversation was retained; no automatic replay or replacement occurred"
        );
    }
    Ok(())
}

fn hook_launch_lock(root: &Path) -> Result<Option<File>> {
    match launch_lock(root) {
        Ok(lock) => Ok(Some(lock)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|cause| cause.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn record_thread(
    root: &Path,
    profile_id: &str,
    scope: &str,
    launch_token: &str,
    thread_id: &str,
) -> Result<()> {
    // A first SessionStart can occur while the launcher still owns its short
    // launch lock. The exact reducer's durable binding is already sufficient
    // for reopening; the launcher pins it after readback, or the next entry
    // resolves the same token. Never block the provider waiting on its parent.
    let lock = hook_launch_lock(root)?;
    let mut binding = bind_scope(root, profile_id, scope)?;
    let parsed = uuid::Uuid::parse_str(thread_id)?.to_string();
    require_epoch(root, &binding)?;
    if let Some(expected) = &binding.thread_id {
        if expected != &parsed {
            bail!("Native Pika hook reported another provider conversation");
        }
        crate::assistant_native_recovery::record_context(root, profile_id, scope, &parsed)?;
        return Ok(());
    }
    if binding.launch_token != launch_token {
        bail!("Native Pika hook does not own the reserved first launch");
    }
    let registry = crate::store::Store::at(root.join("native-registry/pika.db"));
    if registry.get_launch_binding(launch_token)? != Some((Provider::Codex, parsed.clone())) {
        bail!("Native Pika conversation has not been bound by the existing exact launch reducer");
    }
    crate::assistant_native_recovery::record_context(root, profile_id, scope, &parsed)?;
    binding.thread_id = Some(parsed);
    if lock.is_some() {
        save_binding(root, &binding)?;
    }
    Ok(())
}

pub(crate) fn fresh_context(
    root: &Path,
    profile_id: &str,
    scope: &str,
    request: &str,
) -> Result<serde_json::Value> {
    let _lock = launch_lock(root)?;
    let binding = bind_scope(root, profile_id, scope)?;
    if binding.shared_creation && binding.thread_id.is_none() {
        bail!(
            "Native assistant creation is unresolved. Recover its exact provider identity before requesting fresh context; no replacement was created."
        );
    }
    if let Some(receipt) =
        crate::assistant_native_recovery::completed_receipt(root, profile_id, scope, request)?
    {
        let registry = crate::store::Store::at(root.join("native-registry/pika.db"));
        if recorded_thread(&registry, &binding)?.as_deref() == receipt["retired_thread"].as_str() {
            finish_retirement(root, &registry, &binding)?;
        }
        return Ok(receipt);
    }
    let registry = crate::store::Store::at(root.join("native-registry/pika.db"));
    let thread = recorded_thread(&registry, &binding)?;
    let result = crate::assistant_native_recovery::fresh_context(
        root,
        profile_id,
        scope,
        request,
        thread.as_deref(),
    )?;
    if result["recovered"] != true || result["retired_thread"].as_str() != thread.as_deref() {
        bail!("Native recovery did not confirm retirement of the exact conversation");
    }
    finish_retirement(root, &registry, &binding)?;
    Ok(result)
}

// Repeating a completed receipt repairs only interrupted local bookkeeping for
// that same retired UUID. It cannot reset a later native conversation.
fn finish_retirement(root: &Path, registry: &crate::store::Store, binding: &Binding) -> Result<()> {
    let next = Binding {
        profile_id: binding.profile_id.clone(),
        scope: binding.scope.clone(),
        launch_token: uuid::Uuid::new_v4().to_string(),
        thread_id: None,
        memory_epoch: crate::assistant_memory::Store::open(root.join("memory.sqlite"))?
            .forget_epoch()?,
        provider_executable: binding.provider_executable.clone(),
        shared_creation: false,
        shared_ready: false,
    };
    if let Some(thread) = recorded_thread(registry, binding)? {
        registry.untrack_session(Provider::Codex, &thread)?;
    }
    save_binding(root, &next)?;
    crate::assistant_control::Controller::attach(root)?.recovered()?;
    Ok(())
}

fn native_registry(
    ordinary: &Pika,
    root: &Path,
    executable: &Path,
    profile: &mut crate::assistant_native_profile::LaunchProfile,
) -> Result<Pika> {
    let registry = root.join("native-registry");
    crate::assistant_storage::directory(&registry)?;
    let mut paths = ordinary.paths.clone();
    paths.database = registry.join("pika.db");
    paths.codex_home = root.join("provider-home");
    crate::assistant_storage::database(&paths.database)?;
    profile.environment.insert(
        "PIKA_DB_PATH".into(),
        paths.database.to_string_lossy().into_owned(),
    );
    profile.environment.insert(
        "PIKA_ASSISTANT_BOARD_DB_PATH".into(),
        ordinary.paths.database.to_string_lossy().into_owned(),
    );
    profile.environment.insert(
        "PIKA_STATE_HOME".into(),
        ordinary.paths.state_dir.to_string_lossy().into_owned(),
    );
    profile.environment.insert(
        "PIKA_CONFIG_HOME".into(),
        ordinary.paths.config_dir.to_string_lossy().into_owned(),
    );
    let mut config = ordinary.config.clone();
    config
        .provider_executables
        .insert("codex".into(), executable.to_string_lossy().into_owned());
    let store = crate::store::Store::from_paths(&paths);
    store.initialize()?;
    Ok(
        Pika::with_components(paths, config, store, ordinary.tmux.clone()).with_launch_context(
            LaunchContext {
                cwd: profile.cwd.clone(),
                environment: profile.environment.clone(),
                arguments: profile.argv.clone(),
            },
        ),
    )
}

fn reserve_shared_thread(
    native: &Pika,
    root: &Path,
    executable: &Path,
    profile: &crate::assistant_native_profile::LaunchProfile,
    binding: &mut Binding,
) -> Result<()> {
    if binding.shared_creation {
        bail!(
            "Assistant creation has an unknown outcome. No second conversation was created; retain this profile for exact recovery."
        );
    }
    if !native.store.list_sessions()?.is_empty() || !native.store.list_pending()?.is_empty() {
        bail!("The native assistant registry already has an identity; no replacement was created");
    }
    crate::assistant_native_profile::start_shared_provider(executable, profile)?;
    let mut provider = crate::assistant_native_profile::connect_shared_provider(profile)?;
    // Durable intent precedes dispatch. Any failure without an exact UUID stays
    // unknown and never retries thread/start automatically.
    binding.shared_creation = true;
    save_binding(root, binding)?;
    let result = provider.rpc(
        "thread/start",
        serde_json::json!({
            "cwd":profile.cwd,"model":"gpt-6-luna","approvalPolicy":"never",
            "ephemeral":false,"persistExtendedHistory":true,
        }),
    )?;
    let thread = result["thread"]["id"]
        .as_str()
        .context("Private provider omitted the new assistant identity; creation is unknown")?;
    binding.thread_id = Some(uuid::Uuid::parse_str(thread)?.to_string());
    save_binding(root, binding)?;
    materialize_reserved_thread(&mut provider, thread)?;
    finish_shared_reservation(native, root, binding, thread)
}

fn materialize_reserved_thread(
    provider: &mut crate::mobile_codex::Client,
    thread: &str,
) -> Result<()> {
    // Codex does not materialize a fresh persistent thread until its first
    // turn. Its archive API explicitly persists it without inference; restore
    // only this just-created UUID before any terminal or user turn is admitted.
    provider.rpc("thread/archive", serde_json::json!({"threadId":thread}))?;
    let restored = provider.rpc("thread/unarchive", serde_json::json!({"threadId":thread}))?;
    if restored["thread"]["id"] != thread {
        bail!("Private provider did not restore the newly reserved identity");
    }
    let resumed = provider.rpc(
        "thread/resume",
        serde_json::json!({"threadId":thread,"excludeTurns":true}),
    )?;
    if resumed["thread"]["id"] != thread {
        bail!("Private provider did not rejoin the newly reserved identity");
    }
    Ok(())
}

fn finish_shared_reservation(
    native: &Pika,
    root: &Path,
    binding: &mut Binding,
    thread: &str,
) -> Result<()> {
    if !native
        .store
        .bind_launch(&binding.launch_token, Provider::Codex, thread)?
    {
        bail!("Private assistant creation collided with another exact launch binding");
    }
    crate::assistant_native_recovery::record_context(
        root,
        &binding.profile_id,
        &binding.scope,
        thread,
    )?;
    binding.shared_ready = true;
    save_binding(root, binding)?;
    Ok(())
}

fn open_exact(native: &Pika, binding: &Binding) -> Result<OpenReceipt> {
    if native.store.get_pending(&binding.launch_token)?.is_some() {
        return native.open_pending(&binding.launch_token, false);
    }
    if let Some(thread) = recorded_thread(&native.store, binding)? {
        if let Some(session) = native.store.get_session(Provider::Codex, &thread)? {
            return native.open_session(session, false);
        }
        let candidate = crate::providers::Providers::new(&native.paths, &native.config)
            .find(Provider::Codex, &thread)
            .into_iter()
            .find(|candidate| candidate.session_id == thread)
            .context(
                "Pika's recorded conversation is unavailable. No new conversation was substituted.",
            )?;
        let mut session = crate::core::session_from_candidate(&candidate);
        session.cwd = native
            .launch_working_directory()
            .map(|path| path.to_string_lossy().into_owned());
        native.store.upsert_session(&session, false)?;
        return native.open_session(session, false);
    }
    if native.store.get_pending(&binding.launch_token)?.is_some() {
        return native.open_pending(&binding.launch_token, false);
    }
    if !native.store.list_sessions()?.is_empty() || !native.store.list_pending()?.is_empty() {
        bail!(
            "Pika's reserved native home is missing from its registry. No second conversation was created."
        );
    }
    native.new_session_with_token("Pika", Provider::Codex, false, &binding.launch_token)
}

fn recorded_thread(store: &crate::store::Store, binding: &Binding) -> Result<Option<String>> {
    let recorded = store.get_launch_binding(&binding.launch_token)?;
    if let Some((provider, thread)) = &recorded
        && (*provider != Provider::Codex
            || binding
                .thread_id
                .as_ref()
                .is_some_and(|expected| expected != thread))
    {
        bail!("Pika's exact launch binding changed; no replacement conversation was opened");
    }
    Ok(binding
        .thread_id
        .clone()
        .or(recorded.map(|(_, thread)| thread)))
}

fn require_epoch(root: &Path, binding: &Binding) -> Result<()> {
    if binding.memory_epoch
        != crate::assistant_memory::Store::open(root.join("memory.sqlite"))?.forget_epoch()?
    {
        bail!(
            "Pika's backing conversation contains invalidated context. Exit it and request fresh-context; no old conversation was resumed."
        );
    }
    Ok(())
}

pub(crate) fn require_current_context(root: &Path, profile_id: &str, scope: &str) -> Result<()> {
    let path = root.join("native-binding.json");
    crate::assistant_storage::existing_file(&path)?;
    let binding: Binding = serde_json::from_slice(&std::fs::read(path)?)?;
    if binding.profile_id != profile_id || binding.scope != scope {
        bail!("Native context belongs to another profile or scope");
    }
    crate::assistant_native_helpers::invalidate_unavailable_consultations(root, scope)?;
    require_epoch(root, &binding)?;
    let thread = recorded_thread(
        &crate::store::Store::at(root.join("native-registry/pika.db")),
        &binding,
    )?;
    crate::assistant_native_recovery::require_context(root, profile_id, scope, thread.as_deref())
}

/// The provider selected for this exact private home, not whichever unrelated
/// profile is currently the global board default. This never starts a provider.
pub(crate) fn provider_executable(root: &Path, profile_id: &str, scope: &str) -> Result<PathBuf> {
    crate::assistant_host::verify_existing_profile(root, profile_id)?;
    let binding = bind_scope(root, profile_id, scope)?;
    if binding.shared_creation && !binding.shared_ready {
        bail!("The assistant's provider setup is unfinished; no helper was admitted");
    }
    let executable = binding.provider_executable.context("This native profile has no recorded provider executable; open its exact conversation to connect it.")?;
    if !executable.is_absolute() || !executable.is_file() {
        bail!("This native profile's selected Codex executable is unavailable");
    }
    Ok(executable)
}

pub(crate) fn bound_thread(root: &Path, profile_id: &str, scope: &str) -> Result<String> {
    require_current_context(root, profile_id, scope)?;
    let binding = bind_scope(root, profile_id, scope)?;
    recorded_thread(
        &crate::store::Store::at(root.join("native-registry/pika.db")),
        &binding,
    )?
    .context("Native conversation has no exact recorded provider UUID")
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct SharedBinding {
    pub socket_path: PathBuf,
    pub thread_id: String,
    pub profile_id: String,
    pub scope: String,
    pub memory_epoch: u64,
    pub launch_token: String,
}

/// Resolve only the existing private provider endpoint. This never starts a
/// daemon, creates a conversation, or changes a running terminal's route.
pub(crate) fn shared_binding(root: &Path, profile_id: &str, scope: &str) -> Result<SharedBinding> {
    crate::assistant_host::verify_existing_profile(root, profile_id)?;
    require_current_context(root, profile_id, scope)?;
    let binding = bind_scope(root, profile_id, scope)?;
    if binding.shared_creation && !binding.shared_ready {
        bail!("The assistant's provider setup is unfinished; no mobile action was admitted");
    }
    let thread_id = bound_thread(root, profile_id, scope)?;
    let socket_path = crate::assistant_native_profile::shared_socket(root).canonicalize()
        .context("The assistant is not on its private shared connection yet. Exit its terminal normally, then reopen Pika; the running assistant was left untouched.")?;
    require_private_socket(&socket_path)?;
    std::os::unix::net::UnixStream::connect(&socket_path)
        .context("The private assistant provider is unavailable; no replacement was started")?;
    Ok(SharedBinding {
        socket_path,
        thread_id,
        profile_id: profile_id.into(),
        scope: scope.into(),
        memory_epoch: binding.memory_epoch,
        launch_token: binding.launch_token,
    })
}

fn require_private_socket(socket_path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = std::fs::symlink_metadata(socket_path)?;
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!("The assistant's private provider socket is not owner-only");
    }
    for ancestor in socket_path.ancestors().skip(1) {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        let sticky_root = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && !sticky_root)
        {
            bail!("The assistant provider socket has an untrusted parent");
        }
    }
    Ok(())
}

pub(crate) fn require_shared_binding(root: &Path, expected: &SharedBinding) -> Result<()> {
    let current = shared_binding(root, &expected.profile_id, &expected.scope)?;
    if current != *expected {
        bail!("The assistant's exact context generation or private endpoint changed");
    }
    Ok(())
}

pub(crate) fn require_generation(
    root: &Path,
    profile_id: &str,
    scope: &str,
    token: &str,
) -> Result<()> {
    let path = root.join("native-binding.json");
    crate::assistant_storage::existing_file(&path)?;
    let binding: Binding = serde_json::from_slice(&std::fs::read(path)?)?;
    if binding.profile_id != profile_id || binding.scope != scope || binding.launch_token != token {
        bail!("This native assistant process belongs to a retired context generation");
    }
    Ok(())
}

pub(crate) fn require_thread(
    root: &Path,
    profile_id: &str,
    scope: &str,
    session_id: &str,
) -> Result<()> {
    let path = root.join("native-binding.json");
    crate::assistant_storage::existing_file(&path)?;
    let binding: Binding = serde_json::from_slice(&std::fs::read(path)?)?;
    if binding.profile_id != profile_id
        || binding.scope != scope
        || recorded_thread(
            &crate::store::Store::at(root.join("native-registry/pika.db")),
            &binding,
        )?
        .as_deref()
            != Some(session_id)
    {
        bail!("Native hook is not this profile's exact main conversation");
    }
    Ok(())
}

#[cfg(test)]
#[path = "assistant_native_mcp_tests.rs"]
mod mcp_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn registry_fixture(root: &Path) -> crate::store::Store {
        let parent = root.join("native-registry");
        crate::assistant_storage::directory(&parent).unwrap();
        let store = crate::store::Store::at(parent.join("pika.db"));
        store.initialize().unwrap();
        store
    }

    #[test]
    fn exact_identity_survives_rename_and_rejects_another_hook_or_launch_binding() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        crate::assistant_storage::directory(&root).unwrap();
        let profile = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
            .unwrap()
            .profile_id()
            .to_owned();
        let mut binding = bind_scope(&root, &profile, "personal").unwrap();
        let store = registry_fixture(&root);
        let thread = uuid::Uuid::new_v4().to_string();
        let session = crate::core::session_from_candidate(&crate::model::Candidate {
            provider: Provider::Codex,
            session_id: thread.clone(),
            name: Some("Pika renamed".into()),
            cwd: Some(root.display().to_string()),
            branch: None,
            transcript_path: None,
            model: None,
            updated_at: 1.0,
            live: false,
            pid: None,
            source: "fixture".into(),
            parent_session_id: None,
            created_at: 1.0,
            lifecycle_status: None,
        });
        store.upsert_session(&session, false).unwrap();
        store
            .bind_launch(&binding.launch_token, Provider::Codex, &thread)
            .unwrap();
        assert_eq!(
            recorded_thread(&store, &binding).unwrap().as_deref(),
            Some(thread.as_str())
        );
        require_thread(&root, &profile, "personal", &thread).unwrap();
        assert!(
            require_thread(
                &root,
                &profile,
                "personal",
                &uuid::Uuid::new_v4().to_string()
            )
            .is_err()
        );
        assert!(require_thread(&root, &profile, "project:other", &thread).is_err());
        let lock = launch_lock(&root).unwrap();
        record_thread(&root, &profile, "personal", &binding.launch_token, &thread).unwrap();
        drop(lock);
        record_thread(&root, &profile, "personal", &binding.launch_token, &thread).unwrap();
        binding = bind_scope(&root, &profile, "personal").unwrap();
        assert_eq!(binding.thread_id.as_deref(), Some(thread.as_str()));
        store.delete_launch_binding(&binding.launch_token).unwrap();
        store
            .bind_launch(&binding.launch_token, Provider::Claude, &thread)
            .unwrap();
        assert!(recorded_thread(&store, &binding).is_err());
    }

    #[test]
    fn unknown_shared_creation_never_replays_or_replaces_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        crate::assistant_storage::directory(&root).unwrap();
        let profile = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
            .unwrap()
            .profile_id()
            .to_owned();
        let mut binding = bind_scope(&root, &profile, "personal").unwrap();
        binding.shared_creation = true;
        save_binding(&root, &binding).unwrap();
        let before = std::fs::read(root.join("native-binding.json")).unwrap();
        let native = Pika::with_components(
            crate::paths::Paths::discover().unwrap(),
            crate::config::Config::default(),
            registry_fixture(&root),
            crate::tmux::Tmux::with_executable("/usr/bin/false", Some("never-used".into())),
        );
        let launch = crate::assistant_native_profile::LaunchProfile {
            cwd: root.clone(),
            environment: Default::default(),
            argv: Vec::new(),
        };
        let error = reserve_shared_thread(
            &native,
            &root,
            Path::new("/usr/bin/false"),
            &launch,
            &mut binding,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown outcome"));
        assert!(
            shared_binding(&root, &profile, "personal")
                .unwrap_err()
                .to_string()
                .contains("unfinished")
        );
        assert!(
            fresh_context(
                &root,
                &profile,
                "personal",
                &uuid::Uuid::new_v4().to_string()
            )
            .unwrap_err()
            .to_string()
            .contains("unresolved")
        );
        assert_eq!(
            std::fs::read(root.join("native-binding.json")).unwrap(),
            before
        );
        assert!(native.store.list_pending().unwrap().is_empty());
        assert!(native.store.list_sessions().unwrap().is_empty());
        assert!(!root.join("provider-home").exists());
    }

    #[test]
    fn recorded_missing_thread_never_becomes_a_fresh_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        crate::assistant_storage::directory(&root).unwrap();
        let mut binding = bind_scope(&root, &uuid::Uuid::new_v4().to_string(), "personal").unwrap();
        binding.thread_id = Some(uuid::Uuid::new_v4().to_string());
        let mut paths = crate::paths::Paths::discover().unwrap();
        paths.codex_home = root.join("empty-provider");
        paths.database = root.join("native-registry/pika.db");
        let store = registry_fixture(&root);
        let native = Pika::with_components(
            paths,
            crate::config::Config::default(),
            store,
            crate::tmux::Tmux::with_executable("/usr/bin/false", Some("never-used".into())),
        );
        let error = open_exact(&native, &binding).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("No new conversation was substituted")
        );
        assert!(native.store.list_pending().unwrap().is_empty());
        assert!(native.store.list_sessions().unwrap().is_empty());
    }
    #[test]
    fn scope_binding_never_reinterprets_existing_native_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        crate::assistant_storage::directory(&root).unwrap();
        let profile = uuid::Uuid::new_v4().to_string();
        bind_scope(&root, &profile, "personal").unwrap();
        let before = std::fs::read(root.join("native-binding.json")).unwrap();
        bind_scope(&root, &profile, "personal").unwrap();
        assert!(bind_scope(&root, &profile, "project:other").is_err());
        assert!(bind_scope(&root, &uuid::Uuid::new_v4().to_string(), "personal").is_err());
        assert_eq!(
            before,
            std::fs::read(root.join("native-binding.json")).unwrap()
        );
    }
    #[test]
    fn first_launch_has_single_owner_and_lock_survives_contenders() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        crate::assistant_storage::directory(&root).unwrap();
        let first = launch_lock(&root).unwrap();
        assert!(launch_lock(&root).is_err());
        drop(first);
        launch_lock(&root).unwrap();
    }

    #[test]
    fn completed_retirement_repairs_local_bookkeeping_without_resetting_later_context() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("profile");
        let _control = crate::assistant_control::Controller::open(&root).unwrap();
        let profile = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
            .unwrap()
            .profile_id()
            .to_owned();
        let mut binding = bind_scope(&root, &profile, "personal").unwrap();
        let registry = registry_fixture(&root);
        let retired = uuid::Uuid::new_v4().to_string();
        binding.thread_id = Some(retired.clone());
        save_binding(&root, &binding).unwrap();
        crate::assistant_native_recovery::record_context(&root, &profile, "personal", &retired)
            .unwrap();
        let request = uuid::Uuid::new_v4().to_string();
        // Crash boundary: verified provider retirement has committed, but the
        // launch binding still points at that exact old UUID.
        let db = rusqlite::Connection::open(root.join("runtime.sqlite")).unwrap();
        db.execute("UPDATE assistant_native_contexts SET state='retired',retirement_id=? WHERE thread_id=?", rusqlite::params![request,retired]).unwrap();
        drop(db);
        let receipt = fresh_context(&root, &profile, "personal", &request).unwrap();
        assert_eq!(receipt["retired_thread"], retired);
        let mut next = bind_scope(&root, &profile, "personal").unwrap();
        assert!(next.thread_id.is_none());
        assert_ne!(next.launch_token, binding.launch_token);
        assert!(require_generation(&root, &profile, "personal", &binding.launch_token).is_err());
        let bytes = std::fs::read(root.join("native-binding.json")).unwrap();
        fresh_context(&root, &profile, "personal", &request).unwrap();
        assert_eq!(
            std::fs::read(root.join("native-binding.json")).unwrap(),
            bytes
        );
        let later = uuid::Uuid::new_v4().to_string();
        next.thread_id = Some(later.clone());
        save_binding(&root, &next).unwrap();
        crate::assistant_native_recovery::record_context(&root, &profile, "personal", &later)
            .unwrap();
        let bytes = std::fs::read(root.join("native-binding.json")).unwrap();
        fresh_context(&root, &profile, "personal", &request).unwrap();
        assert_eq!(
            std::fs::read(root.join("native-binding.json")).unwrap(),
            bytes
        );
        assert!(registry.list_sessions().unwrap().is_empty());
        require_current_context(&root, &profile, "personal").unwrap();
    }
}
