#[test]
fn compact_v1_pairing_qr_fits_normal_terminal_width() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let full = serde_json::json!({"v":1,"address":"100.100.100.100","ssh_port":22,"username":"ayushjain","node_id":"12345678-1234-4234-8234-123456789abc","ssh_host_key":format!("ssh-ed25519 {}", "A".repeat(68)),"pair_port":54321,"tls_sha256":"a".repeat(64),"token":"a".repeat(43),"expires_at":1791010000});
    let minimal = serde_json::json!({"v":1,"address":"100.100.100.100","pair_port":54321,"tls_sha256":"a".repeat(64),"token":"a".repeat(43),"expires_at":1791010000});
    for (name, uri) in [
        (
            "full-json",
            format!(
                "pika://pair?data={}",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&full).unwrap())
            ),
        ),
        (
            "minimal-json",
            format!(
                "pika://pair?data={}",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&minimal).unwrap())
            ),
        ),
        (
            "compact-bootstrap",
            format!(
                "pika://pair/v1/100.100.100.100:54321#{}",
                URL_SAFE_NO_PAD.encode(std::array::from_fn::<u8, 64, _>(|i| (i * 73 + 19) as u8))
            ),
        ),
    ] {
        let qr = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::H)
            .unwrap();
        let width = qr.width() + 8;
        eprintln!(
            "{name}: bytes={} QRquietzonewidth={width} terminalrows={}",
            uri.len(),
            width.div_ceil(2)
        );
        if name == "compact-bootstrap" {
            assert!(
                width <= 80,
                "Compact QR must not wrap in an 80-column terminal"
            );
            assert!(
                width.div_ceil(2) + 4 <= 40,
                "Compact QR and instructions must fit 40 rows"
            );
        }
    }
}
