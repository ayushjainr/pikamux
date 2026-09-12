#[test]
fn shipped_third_party_notices_cover_binary_distribution_obligations() {
    let notices = include_str!("../THIRD_PARTY.md");
    assert!(notices.contains("Cargo.lock SHA-256"));
    assert!(notices.contains("Declared authors"));
    assert!(notices.contains("## Included notices and license texts"));
    assert!(notices.contains("Apache License"));
    assert!(notices.contains("Mozilla Public License Version 2.0"));
    assert!(notices.contains("UNICODE LICENSE V3"));
    assert!(notices.contains("The author disclaims copyright to this source code"));
    assert!(notices.contains("Copyright notices for The Rust Standard Library"));
    assert!(notices.contains("compiler_builtins-0.1.158"));
    assert!(notices.contains("self-contained musl libc"));
    assert!(notices.contains("musl libc"));
    assert!(notices.contains("Rich Felker"));
    assert!(notices.contains("| `libsqlite3-sys` | `0.35.0` | `MIT` |"));
    assert!(notices.contains("| `windows-sys` | `0.61.2` | `MIT OR Apache-2.0` |"));
    assert!(!notices.contains(env!("CARGO_MANIFEST_DIR")));
}
