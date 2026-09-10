//! Integration tests for the CLI front door (src/main.rs arg dispatch).
//! main() itself isn't unit-testable, so these spawn the built binary.

use std::process::{Command, Stdio};

#[test]
fn version_flag_prints_version_and_config_report() {
    let out = Command::new(env!("CARGO_BIN_EXE_zcode-consultant"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("spawn built binary");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let mut lines = stdout.lines();
    // first line stays the exact version (parseable by scripts)
    assert_eq!(
        lines.next(),
        Some(format!("zcode-consultant {}", env!("CARGO_PKG_VERSION")).as_str())
    );
    // followed by the effective-config report; its content depends on the
    // machine's own config file, so assert structure, not values
    let rest: Vec<&str> = lines.collect();
    for key in ["config:", "backend:", "reviewer:", "data:"] {
        assert!(rest.iter().any(|l| l.starts_with(key)), "missing {key} line in:\n{stdout}");
    }
}
