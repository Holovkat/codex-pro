#![allow(clippy::expect_used)]
use codex_core::auth::CODEX_API_KEY_ENV_VAR;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;
use wiremock::MockServer;

pub struct TestCodexExecBuilder {
    home: TempDir,
    cwd: TempDir,
}

impl TestCodexExecBuilder {
    fn settings_path(&self) -> PathBuf {
        self.home.path().join("settings.json")
    }

    fn ensure_settings(&self) {
        let path = self.settings_path();
        if path.exists() {
            return;
        }

        let contents = r#"{
  "model": {
    "provider": "openai",
    "default": "gpt-5-codex"
  },
  "providers": { "custom": {} }
}
"#;
        if let Err(err) = fs::write(&path, contents) {
            panic!(
                "failed to write codex settings fixture to {}: {err}",
                path.display()
            );
        }
    }

    pub fn cmd(&self) -> assert_cmd::Command {
        self.ensure_settings();
        let mut cmd = assert_cmd::Command::cargo_bin("codex-exec")
            .expect("should find binary for codex-exec");
        cmd.current_dir(self.cwd.path())
            .env("CODEX_HOME", self.home.path())
            .env(CODEX_API_KEY_ENV_VAR, "dummy")
            .env("CODEX_SETTINGS_PATH", self.settings_path());
        cmd
    }
    pub fn cmd_with_server(&self, server: &MockServer) -> assert_cmd::Command {
        let mut cmd = self.cmd();
        let base = format!("{}/v1", server.uri());
        cmd.env("OPENAI_BASE_URL", base);
        cmd
    }

    pub fn cwd_path(&self) -> &Path {
        self.cwd.path()
    }
    pub fn home_path(&self) -> &Path {
        self.home.path()
    }
}

pub fn test_codex_exec() -> TestCodexExecBuilder {
    TestCodexExecBuilder {
        home: TempDir::new().expect("create temp home"),
        cwd: TempDir::new().expect("create temp cwd"),
    }
}
