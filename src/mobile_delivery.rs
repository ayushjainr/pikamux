//! Durable admission receipts in Pika's existing database, never automatic replay.
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::{path::Path, time::Duration};

pub(crate) struct Journal(Connection);
impl Journal {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_millis(500))?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS mobile_deliveries (id TEXT PRIMARY KEY, payload TEXT NOT NULL, outcome TEXT NOT NULL)")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS mobile_creation_reservations (node TEXT NOT NULL, project TEXT NOT NULL, name TEXT NOT NULL, operation TEXT NOT NULL, PRIMARY KEY(node,project,name))")?;
        Ok(Self(db))
    }
    /// Returns a prior receipt, or commits an unknown receipt before any send.
    pub(crate) fn begin(&mut self, id: &str, payload: &Value) -> Result<Option<Value>> {
        uuid::Uuid::parse_str(id).context("Use a stable UUID for this operation")?;
        let encoded = payload.to_string();
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT payload,outcome FROM mobile_deliveries WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((previous, outcome)) = prior {
            if previous != encoded {
                bail!("Operation identifier was reused for different work");
            }
            return Ok(Some(serde_json::from_str(&outcome)?));
        }
        tx.execute(
            "INSERT INTO mobile_deliveries VALUES (?,?,?)",
            params![id, encoded, r#"{"state":"unknown"}"#],
        )?;
        tx.commit()?;
        Ok(None)
    }
    pub(crate) fn finish(&self, id: &str, outcome: &Value) -> Result<()> {
        self.0.execute(
            "UPDATE mobile_deliveries SET outcome=? WHERE id=?",
            params![outcome.to_string(), id],
        )?;
        Ok(())
    }
    pub(crate) fn lookup(&self, id: &str) -> Result<Option<(Value, Value)>> {
        uuid::Uuid::parse_str(id)?;
        let row: Option<(String, String)> = self
            .0
            .query_row(
                "SELECT payload,outcome FROM mobile_deliveries WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(payload, outcome)| {
            Ok((
                serde_json::from_str(&payload)?,
                serde_json::from_str(&outcome)?,
            ))
        })
        .transpose()
    }
    pub(crate) fn reserve_creation(
        &self,
        node: &str,
        project: &str,
        name: &str,
        id: &str,
    ) -> Result<()> {
        let inserted = self.0.execute(
            "INSERT OR IGNORE INTO mobile_creation_reservations VALUES (?,?,?,?)",
            params![node, project, name, id],
        )?;
        if inserted == 0 {
            bail!(
                "A creation with this exact name and project is already unresolved; check its existing receipt"
            );
        }
        Ok(())
    }
    pub(crate) fn finish_creation(&self, id: &str) -> Result<()> {
        self.0.execute(
            "DELETE FROM mobile_creation_reservations WHERE operation=?",
            [id],
        )?;
        Ok(())
    }
}
