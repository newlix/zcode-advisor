//! Integration tests for the CLI front door (src/main.rs arg dispatch).
//! main() itself isn't unit-testable, so these spawn the built binary.

use std::process::{Command, Stdio};

#[test]
fn version_flag_prints_version_and_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_zcode-consultant"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("spawn built binary");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert_eq!(
        stdout.trim(),
        format!("zcode-consultant {}", env!("CARGO_PKG_VERSION"))
    );
}
