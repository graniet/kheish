use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

fn cli_bin() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_kheish-daemon") {
        return Ok(PathBuf::from(path));
    }
    let fallback =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/kheish-daemon");
    if fallback.exists() {
        return Ok(fallback);
    }
    Err(anyhow!(
        "failed to locate kheish-daemon binary via CARGO_BIN_EXE_kheish-daemon or target/debug"
    ))
}

fn assert_raw_generated_key(output: &std::process::Output) -> Result<()> {
    assert!(
        output.status.success(),
        "command failed with status {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.trim().is_empty(),
        "expected empty stderr, got: {stderr}"
    );
    let stdout = std::str::from_utf8(&output.stdout)?;
    let generated = stdout
        .strip_suffix("\r\n")
        .or_else(|| stdout.strip_suffix('\n'))
        .unwrap_or(stdout);
    assert!(
        !generated.is_empty(),
        "expected one generated key on stdout"
    );
    assert!(
        !generated.starts_with('{') && !generated.starts_with('"'),
        "expected raw stdout key, got: {generated}"
    );
    assert!(
        !generated.contains('\n') && !generated.contains('\r'),
        "expected a single stdout line, got: {stdout:?}"
    );
    assert_eq!(
        generated.len(),
        44,
        "expected 44 base64 characters for a 32-byte key, got: {generated}"
    );
    assert!(
        generated.bytes().all(
            |byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'=')
        ),
        "expected standard base64 output, got: {generated}"
    );
    assert!(
        generated.ends_with('='),
        "expected padded base64 output, got: {generated}"
    );
    assert_eq!(BASE64_STANDARD.decode(generated)?.len(), 32);
    Ok(())
}

#[test]
fn secrets_generate_prints_one_raw_base64_key_on_stdout() -> Result<()> {
    let bin = cli_bin()?;
    let output = Command::new(bin).args(["secrets", "generate"]).output()?;
    assert_raw_generated_key(&output)
}

#[test]
fn secrets_generate_ignores_global_output_flag_and_still_prints_raw_key() -> Result<()> {
    let bin = cli_bin()?;
    let output = Command::new(bin)
        .args(["--output", "json", "secrets", "generate"])
        .output()?;
    assert_raw_generated_key(&output)
}
