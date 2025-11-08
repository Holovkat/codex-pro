#![allow(clippy::unwrap_used)]
use anyhow::Context;
use codex_agentic_core::AgentProfile;
use codex_agentic_core::AgentStore;
use core_test_support::test_codex_exec::test_codex_exec;
use serde_json::Value;
use std::fs;
use std::path::Path;

#[test]
fn exec_records_agent_metadata_and_flags() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let agents_root = test.home_path().join(".codex").join("agents");
    let store = AgentStore::with_root(&agents_root)?;
    let profile = AgentProfile {
        name: "Runner".to_string(),
        slug: "runner".to_string(),
        priming_prompt: Some("Pretend to be helpful.".to_string()),
        enabled_tools: vec!["web_search_request".to_string()],
        ..AgentProfile::default()
    };
    store.upsert_profile(profile)?;

    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli_responses_fixture.sse");

    test.cmd()
        .env("CODEX_RS_SSE_FIXTURE", &fixture)
        .env("OPENAI_BASE_URL", "http://unused.local")
        .arg("--skip-git-repo-check")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("--enable-tool")
        .arg("memory_fetch")
        .arg("--agent")
        .arg("Runner")
        .arg("echo agent metadata")
        .assert()
        .success();

    let instances_dir = agents_root.join("runner").join("instances");
    let mut run_dirs: Vec<_> = fs::read_dir(&instances_dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    assert!(
        !run_dirs.is_empty(),
        "expected at least one agent run directory"
    );
    run_dirs.sort();
    let run_id = run_dirs
        .pop()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .expect("missing run id directory");

    let run_dir = instances_dir.join(&run_id);
    assert!(run_dir.is_dir(), "expected {} to exist", run_dir.display());

    let record = store.load_run("runner", &run_id)?;
    let context = record.context.expect("run context missing");
    assert_eq!(context.agent_name.as_deref(), Some("Runner"));
    assert!(
        context
            .enabled_tools
            .iter()
            .any(|tool| tool == "web_search_request"),
        "expected default tool in run context"
    );
    assert!(
        context
            .enabled_tools
            .iter()
            .any(|tool| tool == "memory_fetch"),
        "expected CLI tool override to be recorded"
    );
    assert!(
        context
            .dangerous_flags
            .iter()
            .any(|flag| flag == "dangerously_bypass_approvals_and_sandbox"),
        "expected dangerous flag usage to be recorded"
    );

    let profile_after = store.load_profile("runner")?;
    assert!(
        profile_after.last_run_at.is_some(),
        "expected last_run_at to be updated"
    );

    let run_json_path = run_dir.join("run.json");
    let run_json_raw = fs::read_to_string(&run_json_path)
        .with_context(|| format!("run.json not found at {}", run_json_path.display()))?;
    let run_json: Value = serde_json::from_str(&run_json_raw)?;
    assert_eq!(run_json["agent_slug"], "runner");
    assert_eq!(
        run_json["context"]["enabled_tools"]
            .as_array()
            .expect("enabled_tools missing")
            .len(),
        2,
        "expected enabled tools to include defaults + CLI override"
    );
    assert_eq!(
        run_json["context"]["dangerous_flags"][0],
        "dangerously_bypass_approvals_and_sandbox"
    );

    let log_path = run_dir.join("events.jsonl");
    let log_contents = fs::read_to_string(&log_path)
        .with_context(|| format!("events log not found at {}", log_path.display()))?;
    assert!(
        log_contents.contains("agent: Runner (runner)"),
        "log file missing agent metadata"
    );
    assert!(
        log_contents.contains("dangerous_flags: dangerously_bypass_approvals_and_sandbox"),
        "log file missing dangerous flag entry"
    );

    Ok(())
}
