//! One explicitly approved live dream on the existing disposable authority.
//! This helper never creates a second authority/driver or discovers credentials.
use std::{
    collections::HashSet,
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

pub(super) fn run(root: &Path, codex: &Path) {
    assert_eq!(
        std::env::var("PIKA_LIVE_ACCEPTANCE").as_deref(),
        Ok("approved")
    );
    assert!(root.is_absolute() && codex.is_absolute());
    // The native MCP process has already started the production host and its
    // sole maintenance driver. Refuse to substitute an in-process driver.
    let connection = UnixStream::connect(root.join("view.sock"))
        .expect("live dream requires the existing disposable authority");
    drop(connection);
    let selected = crate::assistant::scope("personal").unwrap();
    let mut memory = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
    let before = crate::assistant_maintenance::dreams(&memory, &selected).unwrap();
    let previous: HashSet<String> = before["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["id"].as_str().unwrap().to_owned())
        .collect();
    let mut control = crate::assistant_control::Controller::attach(root).unwrap();
    assert!(
        !crate::assistant_native_turns::busy(root).unwrap(),
        "finish the native foreground turn before live maintenance"
    );
    // One background call, not an ongoing subscription. The existing worker
    // timeout is 120 seconds; the guard revokes further admission on all exits.
    control
        .approve_maintenance(
            "personal",
            codex,
            crate::assistant_policy::NO_CALL_LIMIT,
            1,
            1,
        )
        .unwrap();
    let _cleanup = DisableMaintenance(root);
    crate::assistant_maintenance::configure(&mut memory, &selected, 3600, true, now()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut next_report = Instant::now();
    loop {
        let receipts = crate::assistant_maintenance::dreams(&memory, &selected).unwrap();
        for run in receipts["runs"].as_array().unwrap() {
            if previous.contains(run["id"].as_str().unwrap()) {
                continue;
            }
            match run["state"].as_str().unwrap() {
                "completed" => {
                    assert_eq!(run["prior_memory_generation"], false);
                    println!("LIVE_DREAM_RECEIPT {run}");
                    // Empty output is a legitimate no-change result, not proof
                    // of new guidance. The next native turn tests usefulness.
                    return;
                }
                "failed" | "unknown" | "invalidated" => {
                    let profile = memory.profile_id().to_owned();
                    let mut client =
                        crate::assistant_host::Client::attach_existing_profile(root, &profile)
                            .unwrap();
                    let details = client.request(
                        serde_json::json!({"operation":"maintenance_status","scope":"personal"}),
                    );
                    println!("LIVE_DREAM_FAILURE_DETAIL {details:?}");
                    let result: Option<String> = memory
                        .connection
                        .query_row(
                            "SELECT result FROM maintenance_jobs WHERE id=?",
                            [run["id"].as_str().unwrap()],
                            |row| row.get(0),
                        )
                        .unwrap();
                    println!("LIVE_DREAM_SYNTHETIC_RESULT {result:?}");
                    panic!("live dream did not complete; no retry: {run}")
                }
                _ => {}
            }
        }
        assert!(
            Instant::now() < deadline,
            "live dream exceeded 120 seconds; admission revoked, no retry. Receipts: {receipts}"
        );
        if Instant::now() >= next_report {
            println!(
                "LIVE_DREAM_PROGRESS {}",
                crate::assistant_maintenance::status(&memory, &selected).unwrap()
            );
            next_report = Instant::now() + Duration::from_secs(10);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

struct DisableMaintenance<'a>(&'a Path);
impl Drop for DisableMaintenance<'_> {
    fn drop(&mut self) {
        // This fixture contains no user state or unrelated background work.
        let result = (|| -> anyhow::Result<()> {
            let mut control = crate::assistant_control::Controller::attach(self.0)?;
            control.disable()?;
            let mut memory = crate::assistant_memory::Store::open(self.0.join("memory.sqlite"))?;
            crate::assistant_maintenance::configure(
                &mut memory,
                &crate::assistant::scope("personal")?,
                3600,
                false,
                now(),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!(
                "LIVE_DREAM_CLEANUP_FAILED {error}; fixture owner must stop its authority before deleting the profile"
            );
            if !std::thread::panicking() {
                panic!("live dream admission could not be revoked: {error}");
            }
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
