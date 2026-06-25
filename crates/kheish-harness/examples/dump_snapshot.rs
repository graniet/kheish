use anyhow::Result;
use kheish_harness::{load_fixture, run_fixture};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let fixture_path = std::env::args()
        .nth(1)
        .expect("usage: cargo run -p kheish-harness --example dump_snapshot -- <fixture.json>");

    let fixture = load_fixture(fixture_path)?;
    let report = run_fixture(fixture).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report.outcome.snapshot)?
    );

    Ok(())
}
