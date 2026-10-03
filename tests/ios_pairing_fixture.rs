//! Explicitly requested disposable pairing board, never provider inference.
#[test]
#[ignore = "requires an explicitly supplied owned disposable pairing root"]
fn prepare_owned_pairing_board() {
    use pikamux::{
        model::{Provider, Session, Status},
        store::Store,
    };
    let root = std::path::PathBuf::from(std::env::var("PIKA_QR_FIXTURE_ROOT").unwrap())
        .canonicalize()
        .unwrap();
    assert!(
        root.starts_with("/private/tmp")
            && root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pika-qr-native.")
    );
    let store = Store::at(root.join("pika.db"));
    store.initialize().unwrap();
    let node = store.ensure_local_node_id().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let row: Session = serde_json::from_value(serde_json::json!({
        "provider":"codex","session_id":"12345678-1234-4234-8234-123456789abc",
        "name":"Paired Synthetic Board","cwd":root,"branch":null,"transcript_path":null,
        "tmux_session":null,"tmux_pane":null,"root_pid":null,"status":"READY","unread":false,
        "model":null,"source":"manual","managed":false,"error":null,"attention_reason":null,
        "created_at":now,"updated_at":now,"last_event_at":now,"last_activity_at":now,
        "live":false,"attached":false,"home_state":"unknown","cpu_percent":null,"rss_kb":null,"input_tokens":null,
        "output_tokens":null,"cached_input_tokens":null,"cache_write_tokens":null,"total_tokens":null,
        "estimated_cost_usd":null,"active_thread_id":null
    })).unwrap();
    assert_eq!(row.provider, Provider::Codex);
    assert_eq!(row.status, Status::Ready);
    store.watch_named_session(&row).unwrap();
    println!("PAIRING_BOARD node={node} name=Paired Synthetic Board");
}
