//! A foreground authenticated-SSH consumer, not a provider or observation owner.
use crate::{
    core::Pika,
    mobile_codex::Client,
    mobile_delivery::Journal,
    model::{Provider, Status},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    sync::mpsc,
    time::Duration,
};

const MAX_REQUEST: usize = 128 * 1024;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity {
    node_id: String,
    provider: Provider,
    thread_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    v: u32,
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}
struct Connection {
    identity: Identity,
    client: Client,
    assistant: Option<(std::path::PathBuf, crate::assistant_native::SharedBinding)>,
}
struct Handler<'a> {
    pika: &'a Pika,
    node: String,
    source: Option<crate::activity_feed::Source>,
    subscription: Option<crate::activity_feed::Subscription>,
    selected: Option<Connection>,
    journal: Journal,
    remote: Option<crate::mobile_remote::Remote>,
}

pub(crate) fn serve(pika: &Pika) -> Result<i32> {
    pika.store.initialize()?;
    let node = pika.store.ensure_local_node_id()?;
    let journal = Journal::open(pika.store.path())?;
    let mut handler = Handler {
        pika,
        node,
        source: None,
        subscription: None,
        selected: None,
        journal,
        remote: None,
    };
    let receiver = read_requests();
    let mut output = io::stdout().lock();
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(request)) => {
                emit_response(&mut output, &mut handler, &request)?;
            }
            Ok(Err(error)) => {
                emit(
                    &mut output,
                    json!({"v":1,"event":"connection/error","params":{"message":error}}),
                )?;
                return Ok(1);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        handler.publish_board(&mut output)?;
        if handler.pump_selected(&mut output)? {
            continue;
        }
        handler.pump_remote(&mut output)?;
    }
}

fn read_requests() -> mpsc::Receiver<std::result::Result<Request, String>> {
    let (sender, receiver) = mpsc::sync_channel(8);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let mut bytes = Vec::new();
            match (&mut input)
                .take((MAX_REQUEST + 1) as u64)
                .read_until(b'\n', &mut bytes)
            {
                Ok(0) => break,
                Ok(_) if bytes.len() > MAX_REQUEST => {
                    let _ = sender.send(Err("Request exceeds its bound".into()));
                    break;
                }
                Ok(_) => {
                    if sender
                        .send(serde_json::from_slice::<Request>(&bytes).map_err(|e| e.to_string()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    receiver
}

fn emit_response(
    output: &mut impl Write,
    handler: &mut Handler<'_>,
    request: &Request,
) -> Result<()> {
    let id = &request.id;
    let result = if request.v != 1 || uuid::Uuid::parse_str(id).is_err() {
        Err(anyhow::anyhow!("Unsupported envelope or request identity"))
    } else {
        handler.handle(request)
    };
    match result {
        Ok(result) => emit(output, json!({"v":1,"id":id,"result":result})),
        Err(error) => emit(
            output,
            json!({"v":1,"id":id,"error":{"code":if matches!(request.method.as_str(),"conversation/send"|"conversation/create"|"conversation/answer"|"conversation/approve"){"rejected_before_dispatch"}else{"unavailable"},"message":error.to_string(),"retryable":false}}),
        ),
    }
}
fn emit_board_pages(
    output: &mut impl Write,
    revision: u64,
    observed_at: Option<f64>,
    total: usize,
    items: Vec<Value>,
    health: Vec<String>,
) -> Result<()> {
    let mut remaining = items.into_iter().peekable();
    let mut index = 0;
    let mut shown = 0;
    loop {
        let mut page = Vec::new();
        let mut bytes = serde_json::to_vec(&health)?.len() + 4096;
        while let Some(item) = remaining.peek() {
            let size = serde_json::to_vec(item)?.len() + 1;
            if !page.is_empty() && bytes + size > 512 * 1024 {
                break;
            }
            if bytes + size > 1024 * 1024 {
                bail!("One board row exceeds the mobile frame bound");
            }
            bytes += size;
            page.push(remaining.next().expect("peeked"));
        }
        shown += page.len();
        let complete = remaining.peek().is_none();
        emit(
            output,
            json!({"v":1,"event":"board/snapshot","params":{"revision":revision,"observedAt":observed_at,"page":{"index":index,"complete":complete},"coverage":{"total":total,"shown":shown,"partial":!complete},"items":page,"health":health}}),
        )?;
        if complete {
            return Ok(());
        }
        index += 1;
    }
}
fn emit(output: &mut impl Write, value: Value) -> Result<()> {
    let bytes = serde_json::to_vec(&value)?;
    if bytes.len() > 16 * 1024 * 1024 {
        bail!("Mobile response exceeds its bound");
    }
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
fn active_turn(turns: &Value) -> Option<&str> {
    turns["data"]
        .as_array()?
        .iter()
        .find(|turn| turn["status"] == "inProgress")?["id"]
        .as_str()
}
impl Handler<'_> {
    fn publish_board(&mut self, output: &mut impl Write) -> Result<()> {
        let Some(snapshot) = self
            .subscription
            .as_mut()
            .and_then(|subscription| subscription.take())
        else {
            return Ok(());
        };
        let total = snapshot.items.len();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let observed_at = snapshot.observed_at.or_else(|| {
            crate::assistant_observation::load(
                &self.pika.paths.state_dir.join("activity-feed"),
                now as i64,
            )
            .ok()
            .flatten()
            .map(|p| p.sampled_at as f64)
        });
        let hosts = self.host_times(&snapshot);
        let items = snapshot
            .items
            .into_iter()
            .map(|item| {
                let sample = item
                    .node_id
                    .as_ref()
                    .and_then(|node| hosts.get(node).copied().flatten());
                board_item(item, &self.node, observed_at, sample, now)
            })
            .collect();
        emit_board_pages(
            output,
            snapshot.revision,
            observed_at,
            total,
            items,
            snapshot.health,
        )
    }

    fn host_times(
        &self,
        snapshot: &crate::activity_feed::Snapshot,
    ) -> std::collections::BTreeMap<String, Option<(f64, f64)>> {
        let mut hosts = std::collections::BTreeMap::new();
        for item in &snapshot.items {
            if let Some(node) = &item.node_id
                && !hosts.contains_key(node)
            {
                hosts.insert(
                    node.clone(),
                    self.pika
                        .store
                        .get_remote_board_projection(node, 64 * 1024, 1)
                        .ok()
                        .flatten()
                        .map(|p| (p.source_captured_at, p.remote_captured_at)),
                );
            }
        }
        hosts
    }

    fn pump_selected(&mut self, output: &mut impl Write) -> Result<bool> {
        let Some(selected) = self.selected.as_mut() else {
            return Ok(false);
        };
        if let Err(error) = selected.client.poll() {
            let identity = selected.identity.clone();
            self.selected = None;
            emit(
                output,
                json!({"v":1,"event":"conversation/disconnected","params":{"identity":identity,"message":error.to_string()}}),
            )?;
            return Ok(true);
        }
        for frame in std::mem::take(&mut selected.client.events) {
            emit(
                output,
                json!({"v":1,"event":"conversation/event","params":{"identity":selected.identity,"method":frame["method"],"params":frame["params"],"requestId":frame.get("id")}}),
            )?;
        }
        Ok(false)
    }

    fn pump_remote(&mut self, output: &mut impl Write) -> Result<()> {
        let Some(remote) = self.remote.as_mut() else {
            return Ok(());
        };
        match remote.poll(&self.pika.store) {
            Ok(events) => {
                for event in events {
                    emit(output, event)?;
                }
            }
            Err(error) => {
                let node = remote.node_id().to_owned();
                self.remote = None;
                emit(
                    output,
                    json!({"v":1,"event":"connection/error","params":{"nodeId":node,"message":error.to_string()}}),
                )?;
            }
        }
        Ok(())
    }
    fn identity(&self, params: &Value) -> Result<Identity> {
        let identity: Identity = serde_json::from_value(params["identity"].clone())?;
        uuid::Uuid::parse_str(&identity.node_id)?;
        if !crate::providers::Providers::valid_id(identity.provider, &identity.thread_id) {
            bail!("Invalid provider conversation identity");
        }
        if identity.node_id != self.node {
            bail!("Conversation content forwarding to this fleet host is not yet supported");
        }
        Ok(identity)
    }
    fn require_watched(&self, identity: &Identity) -> Result<()> {
        if let Some(selected) = self.selected.as_ref().filter(|s| s.identity == *identity) {
            if let Some((root, binding)) = &selected.assistant {
                return crate::assistant_native::require_shared_binding(root, binding);
            }
        }
        let session = self
            .pika
            .store
            .list_sessions()?
            .into_iter()
            .find(|s| {
                s.provider == identity.provider && s.provider_thread_id() == identity.thread_id
            })
            .context("This exact conversation is not watched")?;
        // A terminal-home warning does not invalidate independently proven
        // loaded socket ownership. Competing live owners still fail closed.
        if session.status == Status::OpenTwice {
            bail!("Conversation ownership needs recovery on its computer");
        }
        Ok(())
    }
    fn selected(&mut self, identity: &Identity) -> Result<&mut Client> {
        self.require_watched(identity)?;
        let selected = self
            .selected
            .as_mut()
            .filter(|s| s.identity == *identity)
            .context("Open this exact conversation before sending")?;
        selected.client.require_owner()?;
        Ok(&mut selected.client)
    }
    fn relay(&mut self, request: &Request) -> Result<Option<Value>> {
        let params = &request.params;
        if matches!(
            request.method.as_str(),
            "conversation/open"
                | "conversation/history"
                | "conversation/send"
                | "conversation/answer"
                | "conversation/approve"
                | "conversation/receipt"
                | "conversation/requestStatus"
                | "conversation/candidates"
                | "conversation/adopt"
                | "conversation/create"
                | "projects/list"
        ) {
            let target = params["identity"]["nodeId"]
                .as_str()
                .or_else(|| params["nodeId"].as_str());
            if let Some(node) = target.filter(|node| *node != self.node) {
                uuid::Uuid::parse_str(node)?;
                if self
                    .remote
                    .as_ref()
                    .is_none_or(|remote| remote.node_id() != node)
                {
                    self.remote = Some(crate::mobile_remote::Remote::connect(
                        &self.pika.store,
                        node,
                    )?);
                }
                let result = match self.remote.as_mut().expect("connected").request(&self.pika.store,&json!({"v":request.v,"id":request.id,"method":request.method,"params":request.params})) {
                    Ok(result) => result,
                    Err(error) if error.downcast_ref::<crate::mobile_remote::OutcomeUnknown>().is_some() => {
                        return Ok(Some(json!({"identity":params.get("identity"),"state":"unknown","clientMessageId":params.get("clientMessageId"),"clientOperationId":params.get("clientOperationId").cloned().unwrap_or_else(||json!(request.id)),"message":"Remote receipt was lost after dispatch; reconcile the original operation ID before retrying"})));
                    }
                    Err(error) => return Err(error),
                };
                if matches!(
                    request.method.as_str(),
                    "conversation/open" | "conversation/create"
                ) {
                    self.selected = None;
                }
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    fn handle(&mut self, request: &Request) -> Result<Value> {
        if let Some(result) = self.relay(request)? {
            return Ok(result);
        }
        match request.method.as_str() {
            "hello" => Ok(
                json!({"nodeId":self.node,"version":crate::VERSION,"capabilities":{"board":true,"codexShared":true,"assistant":true,"create":true,"adopt":true},"providers":Provider::ALL.map(|provider|json!({"id":provider,"availability":if provider==Provider::Codex{"conditional"}else{"unverified"},"reason":if provider==Provider::Codex{"Requires an already-loaded shared server"}else{"Existing running conversation access is not verified"}}))}),
            ),
            "board/subscribe" => {
                if self.source.is_none() {
                    let source = crate::activity_observer::start(self.pika)?;
                    self.subscription = Some(source.subscribe());
                    self.source = Some(source);
                }
                Ok(json!({"subscribed":true}))
            }
            "nodes/list" => Ok(
                json!({"items":std::iter::once(json!({"nodeId":self.node,"name":"This machine","local":true})).chain(self.pika.store.list_nodes()?.into_iter().map(|node|json!({"nodeId":node.node_id,"name":node.alias,"local":false,"status":node.status}))).collect::<Vec<_>>()}),
            ),
            "conversation/open" => self.open_conversation(request),
            "conversation/history" => self.history(request),
            "conversation/send" => self.send_message(request),
            "conversation/answer" | "conversation/approve" => self.answer_request(request),
            "conversation/requestStatus" => self.request_status(request),
            "conversation/receipt" => self.receipt(request),
            "conversation/candidates" => self.candidates(request),
            "conversation/adopt" => self.adopt(request),
            "projects/list" => self.projects(request),
            "assistant/open" => self.open_assistant(),
            "conversation/create" => self.create(request),
            _ => bail!("Unsupported mobile method"),
        }
    }

    fn open_conversation(&mut self, request: &Request) -> Result<Value> {
        let params = &request.params;
        let identity = self.identity(params)?;
        self.require_watched(&identity)?;
        if identity.provider != Provider::Codex {
            bail!("This provider's existing-conversation access is not verified");
        }
        let socket = self
            .pika
            .paths
            .codex_home
            .join("app-server-control/app-server-control.sock");
        let mut client = Client::connect(&socket, &identity.thread_id)?;
        let thread = client.rpc(
            "thread/read",
            json!({"threadId":identity.thread_id,"includeTurns":false}),
        )?["thread"]
            .clone();
        let turns = client.rpc(
            "thread/turns/list",
            json!({"threadId":identity.thread_id,"limit":10,"itemsView":"full"}),
        )?;
        self.selected = Some(Connection {
            identity: identity.clone(),
            client,
            assistant: None,
        });
        self.remote = None;
        Ok(
            json!({"identity":identity,"activeTurnId":active_turn(&turns),"capabilities":{"read":true,"send":true,"answer":true,"approvalTypes":["userInput","commandOnce","fileChangeOnce"]},"thread":thread,"turns":turns}),
        )
    }

    fn history(&mut self, request: &Request) -> Result<Value> {
        let params = &request.params;
        let identity = self.identity(params)?;
        let cursor = params["cursor"].clone();
        let page = self.selected(&identity)?.rpc(
            "thread/turns/list",
            json!({"threadId":identity.thread_id,"cursor":cursor,"limit":10,"itemsView":"full"}),
        )?;
        Ok(json!({"identity":identity,"turns":page}))
    }

    fn send_message(&mut self, request: &Request) -> Result<Value> {
        let params = &request.params;
        let identity = self.identity(params)?;
        let id = params["clientMessageId"]
            .as_str()
            .context("Message ID is required")?;
        let text = params["text"]
            .as_str()
            .filter(|text| !text.trim().is_empty() && text.len() <= 65536)
            .context("Message must contain text within the bound")?;
        self.selected(&identity)?.require_loaded()?;
        if let Some(prior) = self
            .journal
            .begin(id, &json!({"method":request.method,"params":params}))?
        {
            return Ok(prior);
        }
        let client = self.selected(&identity)?;
        client.require_loaded()?;
        let input = json!([{"type":"text","text":text}]);
        let result = if let Some(turn) = params["expectedTurnId"].as_str() {
            client.rpc("turn/steer",json!({"threadId":identity.thread_id,"expectedTurnId":turn,"clientUserMessageId":id,"input":input}))
        } else {
            client.rpc(
                "turn/start",
                json!({"threadId":identity.thread_id,"clientUserMessageId":id,"input":input}),
            )
        };
        let outcome = match result {
            Ok(result) => {
                json!({"identity":identity,"clientMessageId":id,"state":"accepted","turnId":result.get("turnId").or_else(||result.get("turn").and_then(|t|t.get("id")))})
            }
            Err(error) => {
                json!({"identity":identity,"clientMessageId":id,"state":if error.downcast_ref::<crate::mobile_codex::Rejected>().is_some(){"rejected"}else{"unknown"},"message":error.to_string()})
            }
        };
        Ok(self.persist_outcome(id, outcome))
    }

    fn answer_request(&mut self, request: &Request) -> Result<Value> {
        let params = &request.params;
        let identity = self.identity(params)?;
        let turn = params["turnId"]
            .as_str()
            .context("Question turn ID is required")?;
        let item = params["itemId"]
            .as_str()
            .context("Question item ID is required")?;
        let id = &params["requestId"];
        if !(id.is_string() || id.as_i64().is_some()) {
            bail!("Invalid provider request ID");
        }
        self.selected(&identity)?;
        if let Some(prior) = self.journal.begin(
            &request.id,
            &json!({"method":request.method,"params":params}),
        )? {
            return Ok(prior);
        }
        let result = if request.method == "conversation/answer" {
            self.selected(&identity)?
                .answer(id, turn, item, params["answers"].clone())
        } else {
            self.selected(&identity)?.approve(
                id,
                turn,
                item,
                params["decision"]
                    .as_str()
                    .context("Approval decision is required")?,
            )
        };
        let outcome = match result {
            Ok(()) => json!({"identity":identity,"state":"submitted"}),
            Err(error) => {
                json!({"identity":identity,"state":"unknown","message":error.to_string()})
            }
        };
        Ok(self.persist_outcome(&request.id, outcome))
    }

    fn request_status(&mut self, request: &Request) -> Result<Value> {
        let params = &request.params;
        let identity = self.identity(params)?;
        let id = &params["requestId"];
        if !(id.is_string() || id.as_i64().is_some()) {
            bail!("Invalid native request ID");
        }
        let turn = params["turnId"]
            .as_str()
            .context("Exact turn is required")?;
        let item = params["itemId"]
            .as_str()
            .context("Exact item is required")?;
        let state = self.selected(&identity)?.request_status(id, turn, item)?;
        Ok(
            json!({"identity":identity,"requestId":id,"turnId":turn,"itemId":item,"state":state,"meaning":"Resolution means the native request closed, not proof the submitted decision was accepted"}),
        )
    }

    fn receipt(&mut self, request: &Request) -> Result<Value> {
        let params = &request.params;
        if params.get("identity").is_none() {
            return self.creation_receipt(params);
        }
        let identity = self.identity(params)?;
        let id = params["clientMessageId"]
            .as_str()
            .or_else(|| params["clientOperationId"].as_str())
            .context("Stable operation ID is required")?;
        let Some((payload, outcome)) = self.journal.lookup(id)? else {
            return Ok(json!({"identity":identity,"state":"not-found"}));
        };
        if payload["params"]["identity"] != serde_json::to_value(&identity)? {
            bail!("Receipt belongs to a different exact conversation");
        }
        if payload["method"] == "conversation/send"
            && matches!(outcome["state"].as_str(), Some("accepted" | "unknown"))
        {
            return self.reconcile_delivery(params, &identity, id, outcome);
        }
        Ok(outcome)
    }

    fn reconcile_delivery(
        &mut self,
        params: &Value,
        identity: &Identity,
        id: &str,
        mut outcome: Value,
    ) -> Result<Value> {
        let cursor = params
            .get("cursor")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_str()
                    .context("Receipt cursor must be the provider's exact string")
            })
            .transpose()?;
        let page = self.selected(identity)?.rpc(
            "thread/turns/list",
            json!({"threadId":identity.thread_id,"limit":10,"itemsView":"full","cursor":cursor}),
        )?;
        if page["data"].as_array().is_some_and(|turns| {
            turns.iter().any(|turn| {
                turn["items"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item["type"] == "userMessage" && item["clientId"] == id)
                })
            })
        }) {
            outcome = json!({"identity":identity,"clientMessageId":id,"state":"delivered"});
            self.journal.finish(id, &outcome)?;
        } else {
            // One read-only page at a time; exhaustion never authorizes replay.
            outcome["nextCursor"] = page["nextCursor"].clone();
            outcome["searchComplete"] = json!(page["nextCursor"].is_null());
        }
        Ok(outcome)
    }

    fn creation_receipt(&mut self, params: &Value) -> Result<Value> {
        if params["nodeId"] != self.node {
            bail!("Creation receipt node identity changed");
        }
        let id = params["clientOperationId"]
            .as_str()
            .context("Creation operation ID is required")?;
        let Some((payload, mut outcome)) = self.journal.lookup(id)? else {
            return Ok(json!({"state":"not-found"}));
        };
        if payload["method"] != "conversation/create" || payload["params"]["nodeId"] != self.node {
            bail!("Receipt belongs to a different operation");
        }
        if outcome["state"] == "unknown"
            && let Some(identity) = self.certified_creation(id)?
        {
            // This is the existing core's atomic native-home certification.
            outcome = self.persist_outcome(
                id,
                json!({"identity":identity,"clientOperationId":id,"state":"created"}),
            );
            if outcome["state"] == "created" {
                let _ = self.journal.finish_creation(id);
            }
        }
        Ok(outcome)
    }

    fn certified_creation(&self, id: &str) -> Result<Option<Identity>> {
        if self.pika.store.get_pending(id)?.is_some() {
            return Ok(None);
        }
        let Some((provider, thread)) = self.pika.store.get_launch_binding(id)? else {
            return Ok(None);
        };
        if provider != Provider::Codex {
            return Ok(None);
        }
        let watched = self.pika.store.list_sessions()?.iter().any(|row| {
            row.managed && row.provider == provider && row.provider_thread_id() == thread
        });
        Ok(watched.then(|| Identity {
            node_id: self.node.clone(),
            provider,
            thread_id: thread,
        }))
    }

    fn candidates(&mut self, request: &Request) -> Result<Value> {
        // Explicit Add includes removed identities, like the shared catalog.
        let watched: std::collections::BTreeSet<_> = self
            .pika
            .store
            .list_sessions()?
            .into_iter()
            .flat_map(|row| {
                let mut ids = vec![(row.provider, row.session_id)];
                if let Some(active) = row.active_thread_id {
                    ids.push((row.provider, active));
                }
                ids
            })
            .collect();
        let providers = crate::providers::Providers::new(&self.pika.paths, &self.pika.config);
        let mut candidates: Vec<_> = Provider::ALL
            .into_iter()
            .flat_map(|provider| providers.import_candidates(provider))
            .filter(|row| !watched.contains(&(row.provider, row.session_id.clone())))
            .collect();
        // A previously watched exact identity needs no guessed human-rename
        // evidence. Still require native provider existence before offering Add.
        for removed in self.pika.store.list_untracked_sessions()? {
            if let Some(mut candidate) = providers
                .find(removed.provider, removed.provider_thread_id())
                .into_iter()
                .find(|candidate| candidate.session_id == removed.provider_thread_id())
                .filter(|candidate| {
                    !watched.contains(&(candidate.provider, candidate.session_id.clone()))
                })
            {
                candidate.name = removed.name.or(candidate.name);
                candidates.push(candidate);
            }
        }
        candidates.sort_by_key(|c| format!("{}:{}", c.provider, c.session_id));
        candidates.dedup_by(|a, b| a.provider == b.provider && a.session_id == b.session_id);
        selector_page(&request.params,candidates.into_iter().map(|c|(format!("{}:{}",c.provider,c.session_id),json!({"identity":{"nodeId":self.node,"provider":c.provider,"threadId":c.session_id},"name":c.name,"project":c.cwd}))).collect())
    }

    fn adopt(&mut self, request: &Request) -> Result<Value> {
        let identity = self.identity(&request.params)?;
        let candidate = crate::providers::Providers::new(&self.pika.paths, &self.pika.config)
            .find(identity.provider, &identity.thread_id)
            .into_iter()
            .find(|c| c.session_id == identity.thread_id)
            .context("Selected conversation no longer exists; reopen Add")?;
        self.pika.adopt_candidate(&candidate)?;
        if let Some(source) = &self.source {
            let _ = source.refresh().try_send(());
        }
        Ok(json!({"identity":identity,"state":"added"}))
    }

    fn projects(&mut self, request: &Request) -> Result<Value> {
        let mut projects: Vec<_> = self
            .pika
            .store
            .list_sessions()?
            .into_iter()
            .filter_map(|s| s.cwd)
            .collect();
        projects.sort();
        projects.dedup();
        selector_page(&request.params,projects.into_iter().map(|path|(path.clone(),json!({"id":path,"name":std::path::Path::new(&path).file_name().map(|v|v.to_string_lossy().into_owned()).unwrap_or_else(||path.clone()),"nodeId":self.node}))).collect())
    }

    fn open_assistant(&mut self) -> Result<Value> {
        let selection = crate::assistant_startup::load(&crate::assistant_startup::path()?)?
            .context("Choose the existing main Pika assistant on its computer first")?;
        let binding = crate::assistant_native::shared_binding(
            &selection.profile_root,
            &selection.profile_id,
            &selection.scope,
        )?;
        let identity = Identity {
            node_id: self.node.clone(),
            provider: Provider::Codex,
            thread_id: binding.thread_id.clone(),
        };
        let mut client = Client::connect(&binding.socket_path, &binding.thread_id)?;
        let thread = client.rpc(
            "thread/read",
            json!({"threadId":identity.thread_id,"includeTurns":false}),
        )?["thread"]
            .clone();
        let turns = client.rpc(
            "thread/turns/list",
            json!({"threadId":identity.thread_id,"limit":10,"itemsView":"full"}),
        )?;
        crate::assistant_native::require_shared_binding(&selection.profile_root, &binding)?;
        self.selected = Some(Connection {
            identity: identity.clone(),
            client,
            assistant: Some((selection.profile_root, binding.clone())),
        });
        self.remote = None;
        Ok(
            json!({"identity":identity,"activeTurnId":active_turn(&turns),"assistant":{"profileId":binding.profile_id,"scope":binding.scope,"memoryEpoch":binding.memory_epoch},"capabilities":{"read":true,"send":true,"answer":true,"approvalTypes":["userInput","commandOnce","fileChangeOnce"]},"thread":thread,"turns":turns}),
        )
    }

    fn creation_intent(&self, params: &Value) -> Result<CreationIntent> {
        if params["nodeId"] != self.node {
            bail!("Choose this exact owning node before creating a conversation");
        }
        let provider: Provider = serde_json::from_value(params["provider"].clone())?;
        if provider != Provider::Codex {
            bail!("Phone-accessible creation is not verified for this provider");
        }
        let id = params["clientOperationId"]
            .as_str()
            .context("Stable creation ID is required")?
            .to_owned();
        let name = params["name"]
            .as_str()
            .filter(|n| !n.trim().is_empty() && n.len() <= 120 && !n.chars().any(char::is_control))
            .context("Choose a conversation name within the bound")?
            .to_owned();
        let project = params["projectId"]
            .as_str()
            .context("Choose an existing project")?;
        let cwd = self.creation_project(project)?;
        Ok(CreationIntent { id, name, cwd })
    }

    fn creation_project(&self, project: &str) -> Result<std::path::PathBuf> {
        let cwd = std::path::Path::new(project).canonicalize()?;
        let permitted = self
            .pika
            .store
            .list_sessions()?
            .into_iter()
            .filter_map(|s| s.cwd)
            .filter_map(|p| std::path::Path::new(&p).canonicalize().ok())
            .any(|p| p == cwd);
        if !cwd.is_dir() || !permitted {
            bail!("Project is no longer in this node's configured board");
        }
        Ok(cwd)
    }

    fn create(&mut self, request: &Request) -> Result<Value> {
        let intent = self.creation_intent(&request.params)?;
        if let Some(prior) = self.journal.begin(
            &intent.id,
            &json!({"method":request.method,"params":request.params}),
        )? {
            return Ok(prior);
        }
        if let Err(error) = self.journal.reserve_creation(
            &self.node,
            &intent.cwd.to_string_lossy(),
            &intent.name,
            &intent.id,
        ) {
            return Ok(self.persist_outcome(&intent.id,json!({"clientOperationId":intent.id,"state":"rejected","message":error.to_string()})));
        }
        let mut identity = None;
        let mut attempted = false;
        let result = self.create_home(&intent, &mut identity, &mut attempted);
        let outcome = self.creation_outcome(&intent.id, result, identity, attempted);
        Ok(self.persist_outcome(&intent.id, outcome))
    }

    fn create_home(
        &mut self,
        intent: &CreationIntent,
        created: &mut Option<Identity>,
        attempted: &mut bool,
    ) -> Result<Value> {
        if !self.pika.resolve_local(&intent.name)?.is_empty() {
            bail!("This name already identifies an existing conversation");
        }
        let socket = self
            .pika
            .paths
            .codex_home
            .join("app-server-control/app-server-control.sock");
        if !socket.exists() {
            start_creation_daemon(self.pika)?;
        }
        let mut client = Client::connect_server(&socket)?;
        *attempted = true;
        let result = client.rpc(
            "thread/start",
            json!({"cwd":intent.cwd,"excludeTurns":true}),
        )?;
        let thread = result["thread"]["id"]
            .as_str()
            .context("Provider did not return the created identity")?;
        uuid::Uuid::parse_str(thread)?;
        let identity = Identity {
            node_id: self.node.clone(),
            provider: Provider::Codex,
            thread_id: thread.into(),
        };
        *created = Some(identity.clone());
        self.journal.finish(&intent.id,&json!({"identity":identity,"clientOperationId":intent.id,"state":"unknown","message":"Exact provider thread created; native home initialization is pending"}))?;
        client.thread = thread.into();
        materialize_created_thread(&mut client, thread)?;
        self.launch_creation_home(intent, thread, &socket)?;
        // Name only after certification: an early provider name would make
        // the core's unchanged existing-name guard reject this UUID's own home.
        client.rpc(
            "thread/name/set",
            json!({"threadId":thread,"name":intent.name}),
        )?;
        self.selected = Some(Connection {
            identity: identity.clone(),
            client,
            assistant: None,
        });
        self.remote = None;
        if let Some(source) = &self.source {
            let _ = source.refresh().try_send(());
        }
        Ok(json!({"identity":identity,"clientOperationId":intent.id,"state":"created"}))
    }

    fn launch_creation_home(
        &self,
        intent: &CreationIntent,
        thread: &str,
        socket: &std::path::Path,
    ) -> Result<()> {
        let native = self
            .pika
            .clone()
            .with_launch_context(crate::core::LaunchContext {
                cwd: intent.cwd.clone(),
                environment: std::collections::BTreeMap::from([(
                    "CODEX_HOME".into(),
                    self.pika.paths.codex_home.to_string_lossy().into_owned(),
                )]),
                arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
            });
        native.new_shared_codex_home(&intent.name, &intent.id, thread)?;
        Ok(())
    }

    fn creation_outcome(
        &mut self,
        id: &str,
        result: Result<Value>,
        identity: Option<Identity>,
        attempted: bool,
    ) -> Value {
        match result {
            Ok(result) => {
                let _ = self.journal.finish_creation(id);
                result
            }
            Err(error) => {
                let rejected = !attempted
                    || (identity.is_none()
                        && error
                            .downcast_ref::<crate::mobile_codex::Rejected>()
                            .is_some());
                if rejected {
                    let _ = self.journal.finish_creation(id);
                }
                json!({"identity":identity,"clientOperationId":id,"state":if rejected{"rejected"}else{"unknown"},"message":error.to_string()})
            }
        }
    }

    // Receipt failures after provider mutation must never invite new-ID replay.
    fn persist_outcome(&mut self, id: &str, outcome: Value) -> Value {
        if let Err(error) = self.journal.finish(id, &outcome) {
            let mut uncertain = outcome;
            uncertain["state"] = json!("unknown");
            uncertain["message"] = json!(format!(
                "Receipt persistence failed; do not replay: {error}"
            ));
            uncertain["clientOperationId"] = json!(id);
            uncertain
        } else {
            outcome
        }
    }
}

struct CreationIntent {
    id: String,
    name: String,
    cwd: std::path::PathBuf,
}

fn board_item(
    item: crate::monitor::BoardItem,
    node: &str,
    observed_at: Option<f64>,
    sample: Option<(f64, f64)>,
    now: f64,
) -> Value {
    let cached_at = sample.map(|times| times.0).or(observed_at);
    let source_at = sample.map(|times| times.1).or(observed_at);
    json!({"identity":{"nodeId":item.node_id.as_deref().unwrap_or(node),"provider":item.session.provider,"threadId":item.session.provider_thread_id()},"name":item.session.display_name(),"machine":item.node_name.unwrap_or_else(||"This machine".into()),"status":item.session.status.to_string(),"unread":item.session.unread,"stale":item.stale || observation_stale(cached_at, now),"observedAt":source_at,"cachedAt":cached_at,"model":item.session.model,"project":item.session.cwd,"lastEventAt":item.session.last_event_at})
}

fn observation_stale(cached_at: Option<f64>, now: f64) -> bool {
    cached_at.is_none_or(|at| !at.is_finite() || at > now || now - at > 60.0)
}

fn materialize_created_thread(client: &mut Client, thread: &str) -> Result<()> {
    // Only a just-created empty UUID uses the provider's archive materializer.
    client.rpc("thread/archive", json!({"threadId":thread}))?;
    client.rpc("thread/unarchive", json!({"threadId":thread}))?;
    client.rpc(
        "thread/resume",
        json!({"threadId":thread,"excludeTurns":true}),
    )?;
    Ok(())
}

fn selector_page(params: &Value, items: Vec<(String, Value)>) -> Result<Value> {
    let limit = params
        .get("limit")
        .map(|v| {
            v.as_u64()
                .filter(|n| *n > 0 && *n <= 256)
                .context("Selector page size must be 1 through 256")
        })
        .transpose()?
        .unwrap_or(64) as usize;
    let cursor = params
        .get("cursor")
        .map(|v| {
            v.as_str()
                .context("Selector cursor must be an exact stable key")
        })
        .transpose()?;
    let total = items.len();
    let mut remaining = items
        .into_iter()
        .filter(|(key, _)| cursor.is_none_or(|cursor| key.as_str() > cursor))
        .peekable();
    let mut page = Vec::new();
    let mut last = None;
    for _ in 0..limit {
        let Some((key, item)) = remaining.next() else {
            break;
        };
        last = Some(key);
        page.push(item);
    }
    let partial = remaining.peek().is_some();
    Ok(
        json!({"coverage":{"total":total,"shown":page.len(),"partial":partial},"items":page,"nextCursor":if partial{last}else{None}}),
    )
}

fn start_creation_daemon(pika: &Pika) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(pika.config.executable(Provider::Codex))
        .args(["app-server", "daemon", "start"])
        .env("CODEX_HOME", &pika.paths.codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("The provider could not start its shared server");
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Provider startup outcome is unknown; creation was not replayed");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_fresh_observation_is_not_future_but_unknown_and_expired_are_stale() {
        assert!(!observation_stale(Some(100.75), 100.9));
        assert!(!observation_stale(Some(40.75), 100.75));
        assert!(observation_stale(Some(100.91), 100.9));
        assert!(observation_stale(None, 100.9));
        assert!(observation_stale(Some(40.89), 100.9));
        assert!(observation_stale(Some(f64::NAN), 100.9));
    }
    #[test]
    fn board_pages_preserve_all_rows_and_one_atomic_revision() {
        let items: Vec<_> = (0..600)
            .map(|n| json!({"n":n,"text":"x".repeat(2048)}))
            .collect();
        let mut wire = Vec::new();
        emit_board_pages(&mut wire, 42, Some(17.0), 600, items, vec!["cached".into()]).unwrap();
        let pages: Vec<Value> = wire
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                assert!(line.len() < 1024 * 1024);
                serde_json::from_slice(line).unwrap()
            })
            .collect();
        assert!(pages.len() > 1);
        let mut rows = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            assert_eq!(page["params"]["revision"], 42);
            assert_eq!(page["params"]["page"]["index"], index);
            assert_eq!(page["params"]["page"]["complete"], index + 1 == pages.len());
            rows.extend(
                page["params"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["n"].as_u64().unwrap()),
            );
        }
        assert_eq!(rows, (0..600).collect::<Vec<_>>());
        assert_eq!(
            pages.last().unwrap()["params"]["coverage"]["partial"],
            false
        );
    }
}
