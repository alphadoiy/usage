#[cfg(unix)]
#[test]
fn external_magpie_contracts() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/magpie.py"
        ))
        .arg(env!("CARGO_BIN_EXE_usage"))
        .output()
        .expect("python3 is required for Agent boundary contracts");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
