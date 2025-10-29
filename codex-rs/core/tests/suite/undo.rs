#![cfg(not(target_os = "windows"))]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core::CodexConversation;
use codex_core::config::Config;
use codex_core::features::Feature;
use codex_core::model_family::find_family_for_model;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol::UndoCompletedEvent;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_apply_patch_function_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;

#[allow(clippy::expect_used)]
async fn undo_harness() -> Result<TestCodexHarness> {
    TestCodexHarness::with_config(|config: &mut Config| {
        config.include_apply_patch_tool = true;
        config.model = "gpt-5".to_string();
        config.model_family = find_family_for_model("gpt-5").expect("gpt-5 is valid");
        config.features.enable(Feature::GhostCommit);
    })
    .await
}

fn git(path: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(path)
        .status()
        .with_context(|| format!("failed to run git {args:?}"))?;
    if status.success() {
        return Ok(());
    }
    let exit_status = status;
    bail!("git {args:?} exited with {exit_status}");
}

fn git_output(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .with_context(|| format!("failed to run git {args:?}"))?;
    if !output.status.success() {
        let exit_status = output.status;
        bail!("git {args:?} exited with {exit_status}");
    }
    String::from_utf8(output.stdout).context("stdout was not valid utf8")
}

fn init_git_repo(path: &Path) -> Result<()> {
    // Use a consistent initial branch and config across environments to avoid
    // CI variance (default-branch hints, line ending differences, etc.).
    git(path, &["init", "--initial-branch=main"])?;
    git(path, &["config", "core.autocrlf", "false"])?;
    git(path, &["config", "user.name", "Codex Tests"])?;
    git(path, &["config", "user.email", "codex-tests@example.com"])?;

    // Create README.txt
    let readme_path = path.join("README.txt");
    fs::write(&readme_path, "Test repository initialized by Codex.\n")?;

    // Stage and commit
    git(path, &["add", "README.txt"])?;
    git(path, &["commit", "-m", "Add README.txt"])?;

    Ok(())
}

fn apply_patch_responses(call_id: &str, patch: &str, assistant_msg: &str) -> Vec<String> {
    vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_apply_patch_function_call(call_id, patch),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-1", assistant_msg),
            ev_completed("resp-2"),
        ]),
    ]
}

async fn run_apply_patch_turn(
    harness: &TestCodexHarness,
    user_prompt: &str,
    call_id: &str,
    patch: &str,
    assistant_msg: &str,
) -> Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    let responses = apply_patch_responses(call_id, patch, assistant_msg);
    let _response_mock = mount_sse_sequence(&server, responses).await;

    let codex = Arc::clone(&harness.test().codex);
    let session_model = harness.test().session_configured.model.clone();
    codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: user_prompt.into(),
            }],
            final_output_json_schema: None,
            cwd: harness.cwd().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: session_model,
            effort: None,
            summary: ReasoningSummary::Auto,
        })
        .await?;

    let _: () = wait_for_event_match(&codex, |ev| {
        if matches!(ev, EventMsg::TaskComplete(_)) {
            Some(())
        } else {
            None
        }
    })
    .await;
    Ok(())
}

async fn expect_successful_undo(codex: &Arc<CodexConversation>) -> Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    let _response_mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-undo"),
            ev_assistant_message("msg-undo", "Undo completed successfully."),
            ev_completed("resp-undo"),
        ])],
    )
    .await;

    codex.submit(Op::Undo).await?;

    let _: () = wait_for_event_match(codex, |ev| {
        if matches!(
            ev,
            EventMsg::UndoCompleted(UndoCompletedEvent { success: true, .. })
        ) {
            Some(())
        } else {
            None
        }
    })
    .await;
    Ok(())
}

async fn expect_failed_undo(codex: &Arc<CodexConversation>) -> Result<()> {
    let server = core_test_support::responses::start_mock_server().await;
    let _response_mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-undo"),
            ev_assistant_message("msg-undo", "Nothing to undo."),
            ev_completed("resp-undo"),
        ])],
    )
    .await;

    codex.submit(Op::Undo).await?;

    let _: () = wait_for_event_match(codex, |ev| {
        if matches!(
            ev,
            EventMsg::UndoCompleted(UndoCompletedEvent { success: false, .. })
        ) {
            Some(())
        } else {
            None
        }
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_restores_initial_state_after_multiple_turns() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = undo_harness().await?;
    init_git_repo(harness.cwd())?;

    let story = harness.path("story.txt");
    fs::write(&story, "initial\n")?;
    git(harness.cwd(), &["add", "story.txt"])?;
    git(harness.cwd(), &["commit", "-m", "Initial story"])?;

    let codex = Arc::clone(&harness.test().codex);

    run_apply_patch_turn(
        &harness,
        "extend story",
        "turn-one",
        "*** Begin Patch\n*** Update File: story.txt\n@@\n-initial\n+turn one\n*** End Patch",
        "turn one done",
    )
    .await?;
    assert_eq!(fs::read_to_string(&story)?, "turn one\n");

    run_apply_patch_turn(
        &harness,
        "extend story again",
        "turn-two",
        "*** Begin Patch\n*** Update File: story.txt\n@@\n-turn one\n+turn two\n*** End Patch",
        "turn two done",
    )
    .await?;
    assert_eq!(fs::read_to_string(&story)?, "turn two\n");

    run_apply_patch_turn(
        &harness,
        "continue story",
        "turn-three",
        "*** Begin Patch\n*** Update File: story.txt\n@@\n-turn two\n+turn three\n*** End Patch",
        "turn three done",
    )
    .await?;
    assert_eq!(fs::read_to_string(&story)?, "turn three\n");

    expect_successful_undo(&codex).await?;
    assert_eq!(fs::read_to_string(&story)?, "turn two\n");

    expect_successful_undo(&codex).await?;
    assert_eq!(fs::read_to_string(&story)?, "turn one\n");

    expect_successful_undo(&codex).await?;
    assert_eq!(fs::read_to_string(&story)?, "initial\n");

    expect_failed_undo(&codex).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_without_snapshot_reports_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = undo_harness().await?;
    let codex = Arc::clone(&harness.test().codex);

    expect_failed_undo(&codex).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_restores_moves_and_renames() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = undo_harness().await?;
    init_git_repo(harness.cwd())?;

    let source = harness.path("rename_me.txt");
    fs::write(&source, "original\n")?;
    git(harness.cwd(), &["add", "rename_me.txt"])?;
    git(harness.cwd(), &["commit", "-m", "add rename target"])?;

    let patch = "*** Begin Patch\n*** Update File: rename_me.txt\n*** Move to: relocated/renamed.txt\n@@\n-original\n+renamed content\n*** End Patch";
    run_apply_patch_turn(&harness, "rename file", "undo-rename", patch, "done").await?;

    let destination = harness.path("relocated/renamed.txt");
    assert!(!source.exists());
    assert_eq!(fs::read_to_string(&destination)?, "renamed content\n");

    let codex = Arc::clone(&harness.test().codex);
    expect_successful_undo(&codex).await?;

    assert_eq!(fs::read_to_string(&source)?, "original\n");
    assert!(!destination.exists());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_does_not_touch_ignored_directory_contents() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = undo_harness().await?;
    init_git_repo(harness.cwd())?;

    let gitignore = harness.path(".gitignore");
    fs::write(&gitignore, "logs/\n")?;
    git(harness.cwd(), &["add", ".gitignore"])?;
    git(harness.cwd(), &["commit", "-m", "ignore logs directory"])?;

    let logs_dir = harness.path("logs");
    fs::create_dir_all(&logs_dir)?;
    let preserved = logs_dir.join("persistent.log");
    fs::write(&preserved, "keep me\n")?;

    run_apply_patch_turn(
        &harness,
        "write log",
        "undo-log",
        "*** Begin Patch\n*** Add File: logs/session.log\n+ephemeral log\n*** End Patch",
        "ok",
    )
    .await?;

    let new_log = logs_dir.join("session.log");
    assert_eq!(fs::read_to_string(&new_log)?, "ephemeral log\n");

    let codex = Arc::clone(&harness.test().codex);
    expect_successful_undo(&codex).await?;

    assert!(new_log.exists());
    assert_eq!(fs::read_to_string(&preserved)?, "keep me\n");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_overwrites_manual_edits_after_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = undo_harness().await?;
    init_git_repo(harness.cwd())?;

    let tracked = harness.path("tracked.txt");
    fs::write(&tracked, "baseline\n")?;
    git(harness.cwd(), &["add", "tracked.txt"])?;
    git(harness.cwd(), &["commit", "-m", "baseline tracked"])?;

    run_apply_patch_turn(
        &harness,
        "modify tracked",
        "undo-manual-overwrite",
        "*** Begin Patch\n*** Update File: tracked.txt\n@@\n-baseline\n+turn change\n*** End Patch",
        "ok",
    )
    .await?;
    assert_eq!(fs::read_to_string(&tracked)?, "turn change\n");

    fs::write(&tracked, "manual edit\n")?;
    assert_eq!(fs::read_to_string(&tracked)?, "manual edit\n");

    let codex = Arc::clone(&harness.test().codex);
    expect_successful_undo(&codex).await?;

    assert_eq!(fs::read_to_string(&tracked)?, "baseline\n");

    Ok(())
}
