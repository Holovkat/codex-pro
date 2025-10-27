use std::path::Path;

use anyhow::Result;
use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<Command> {
    let mut cmd = Command::cargo_bin("codex")?;
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

fn seed_minicpm_stub(home: &Path) -> Result<()> {
    use chrono::Utc;
    use serde_json::Value;
    use std::fs;

    let model_dir = home.join("memory").join("models").join("minicpm");
    fs::create_dir_all(&model_dir)?;
    let mut artifacts = serde_json::Map::new();
    for (name, contents) in [
        ("model.gguf", b"stub-model".as_ref()),
        ("tokenizer.json", b"stub-tokenizer".as_ref()),
        ("config.json", b"stub-config".as_ref()),
    ] {
        let path = model_dir.join(name);
        fs::write(&path, contents)?;
        let mut hasher = Sha256::new();
        hasher.update(contents);
        let checksum = format!("{:x}", hasher.finalize());
        artifacts.insert(name.to_string(), Value::String(checksum));
    }
    let manifest = json!({
        "version": "MiniCPM-Llama3-V2.5-Q4_K_M",
        "last_updated": Utc::now(),
        "artifacts": artifacts,
    });
    let manifest_path = model_dir.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

fn extract_record_id(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line[prefix.len()..].split_whitespace().next())
        .map(std::string::ToString::to_string)
}

#[tokio::test]
async fn memory_cli_crud_flow() -> Result<()> {
    let codex_home = TempDir::new()?;

    codex_command(codex_home.path())?
        .args(["memory", "init"])
        .assert()
        .success();

    seed_minicpm_stub(codex_home.path())?;

    let mut create_cmd = codex_command(codex_home.path())?;
    let create_out = create_cmd
        .args([
            "memory",
            "create",
            "--summary",
            "Weekly planning notes",
            "--tag",
            "planning",
            "--tag",
            "notes",
            "--confidence",
            "0.9",
            "--source",
            "user",
        ])
        .output()?;
    assert!(create_out.status.success());
    let create_stdout = String::from_utf8(create_out.stdout)?;
    let record_id =
        extract_record_id(&create_stdout, "Created memory ").expect("record id from create output");

    codex_command(codex_home.path())?
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(contains("Weekly planning notes"));

    codex_command(codex_home.path())?
        .args([
            "memory",
            "edit",
            &record_id,
            "--summary",
            "Updated planning notes",
            "--tag",
            "updated",
        ])
        .assert()
        .success()
        .stdout(contains(format!("Updated memory {record_id}")));

    codex_command(codex_home.path())?
        .args(["memory", "search", "planning"])
        .assert()
        .success()
        .stdout(contains(&record_id).and(contains("Updated planning notes")));

    codex_command(codex_home.path())?
        .args(["memory", "delete", &record_id, "--yes"])
        .assert()
        .success()
        .stdout(contains(format!("Deleted memory {record_id}")));

    codex_command(codex_home.path())?
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(contains("Showing 0 record(s)"));

    Ok(())
}
