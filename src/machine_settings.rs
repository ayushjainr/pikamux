//! Shared connection-details surface for native boards and Windows clients.
//! Listing is cache-only; Retry uses the existing exact-node fleet operation.
use crate::{
    fleet::{FleetManager, SshTransport},
    onboarding::Screen,
    store::Store,
};
use anyhow::Result;

pub(crate) fn connections(store: &Store, ui: &Screen) -> Result<()> {
    let nodes = store.list_nodes()?;
    if nodes.is_empty() {
        return ui.details(
            "Machine connections",
            "No other machines connected yet.\nChoose Connect a machine in Settings to add one.",
        );
    }
    let labels = nodes
        .iter()
        .map(|node| format!("{} · {}", node.alias, node.status))
        .collect::<Vec<_>>();
    let Some(selection) = ui.select("Machine connections", "Choose a machine for details or to retry. No connection is made just by opening this list.", &labels, false)? else { return Ok(()); };
    let Some(node) = selection.first().and_then(|index| nodes.get(*index)) else {
        return Ok(());
    };
    let details = format!(
        "{}\nSaved status: {}\n{}",
        node.alias,
        node.status,
        node.last_error
            .as_deref()
            .unwrap_or("No connection error recorded.")
    );
    if ui.choice(
        "Connection details",
        &details,
        &["Back", "Retry connection"],
    )? == Some(1)
    {
        ui.progress("Connecting", &format!("Checking {}…", node.alias))?;
        match FleetManager::new(store, SshTransport::default()).refresh_node(&node.node_id) {
            Ok(_) => ui.details(
                "Machine connected",
                &format!("{} connected. Its board snapshot was verified.", node.alias),
            )?,
            Err(error) => ui.details(
                "Connection unavailable",
                &format!("{}\n{error}\nSaved connections were kept.", node.alias),
            )?,
        }
    }
    Ok(())
}
