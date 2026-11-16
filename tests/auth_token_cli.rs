//! Saved-session credential export through the real CLI, using isolated file storage.
#![cfg(not(target_arch = "wasm32"))]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const SITE: &str = "datadoghq.com";

struct SessionFixture(PathBuf);

impl SessionFixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pup-auth-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn session(org: Option<&str>, id: &str) -> Self {
        let fixture = Self::new();
        fixture.register(SITE, org);
        fixture.token(org, id);
        fixture
    }

    fn read(&self, org: Option<&str>) -> Value {
        response(self.export(org).output().unwrap())
    }

    fn write(&self, name: &str, value: Value) {
        std::fs::write(self.0.join(name), serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn register(&self, site: &str, org: Option<&str>) {
        self.write("sessions.json", json!([{"site": site, "org": org}]));
    }

    fn token(&self, org: Option<&str>, session_id: &str) {
        self.write(
            "tokens_datadoghq_com.json",
            json!({org.unwrap_or("__default__"): {
                "access_token": "fixture-access", "refresh_token": "fixture-refresh",
                "issued_at": chrono::Utc::now().timestamp(), "expires_in": 3600,
                "session_id": session_id, "scope": "metrics_read"
            }}),
        );
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pup"));
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("PUP_CONFIG_DIR", &self.0)
            .env("DD_TOKEN_STORAGE", "file")
            .env("HOME", &self.0)
            .env("XDG_CACHE_HOME", &self.0)
            .env("LOCALAPPDATA", &self.0);
        for name in ["SystemRoot", "LLVM_PROFILE_FILE"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn export(&self, org: Option<&str>) -> Command {
        let mut command = self.command();
        if let Some(org) = org {
            command.args(["--org", org]);
        }
        command.args([
            "auth",
            "token",
            "--stored-session",
            "--json",
            "--site",
            SITE,
        ]);
        command
    }
}

impl Drop for SessionFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn response(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn rejected(output: Output) -> String {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!error.contains("fixture-access"));
    error
}

#[test]
fn exports_only_selected_session_and_access_credentials() {
    for org in [None, Some("production")] {
        let fixture = SessionFixture::session(org, "selected-login");
        fixture.write(
            "config.yaml",
            json!({"access_token": "config-token", "site": "datadoghq.eu"}),
        );
        let result = response(
            fixture
                .export(org)
                .env("DD_ACCESS_TOKEN", "ambient-token")
                .env("DD_API_KEY", "ambient-api-key")
                .env("DD_APP_KEY", "ambient-app-key")
                .env("DD_SITE", "datadoghq.eu")
                .env("DD_ORG", "other")
                .env("PUP_ORG", "other")
                .output()
                .unwrap(),
        );
        assert_eq!(result["access_token"], "fixture-access");
        assert_eq!(result["site"], SITE);
        assert_eq!(result["session_id"], "selected-login");
        assert_eq!(result["scope"], "metrics_read");
        assert!(result.get("refresh_token").is_none());
    }
}

#[test]
fn displaced_and_logged_out_sessions_cannot_be_exported() {
    for org in [None, Some("production")] {
        let fixture = SessionFixture::session(org, "original-login");
        assert_eq!(fixture.read(org)["session_id"], "original-login");
        for registry in [json!([{"site": "datadoghq.eu", "org": org}]), json!([])] {
            fixture.write("sessions.json", registry);
            rejected(fixture.export(org).output().unwrap());
        }
    }
}

#[test]
fn concurrent_exports_assign_one_identity_to_registered_legacy_login() {
    let fixture = SessionFixture::session(None, "");
    let children: Vec<_> = (0..8)
        .map(|index| {
            fixture
                .export(None)
                .env("XDG_CACHE_HOME", fixture.0.join(format!("cache-{index}")))
                .spawn()
                .unwrap()
        })
        .collect();
    let results: Vec<_> = children
        .into_iter()
        .map(|child| response(child.wait_with_output().unwrap()))
        .collect();
    let id = results[0]["session_id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    assert!(results.iter().all(|result| result["session_id"] == id));
    fixture.token(None, "replacement-login");
    assert_eq!(fixture.read(None)["session_id"], "replacement-login");
}

#[cfg(unix)] // HOME isolates dirs::home_dir on Unix; Windows uses the OS profile.
#[test]
fn export_waits_for_saved_session_transaction_then_reads_latest() {
    let fixture = SessionFixture::session(None, "original-login");
    let directory = fixture.0.join(".config/pup");
    std::fs::create_dir_all(&directory).unwrap();
    let lock = std::fs::File::create(directory.join("auth.lock")).unwrap();
    lock.lock().unwrap();
    let mut child = fixture.export(None).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert!(
        child.try_wait().unwrap().is_none(),
        "export ignored the session lock"
    );
    fixture.token(None, "latest-login");
    drop(lock);
    assert_eq!(
        response(child.wait_with_output().unwrap())["session_id"],
        "latest-login"
    );
}

#[test]
fn stored_export_requires_explicit_selection_and_never_falls_back() {
    let fixture = SessionFixture::new();
    for args in [
        vec!["--stored-session"],
        vec!["--stored-session", "--json"],
        vec!["--json"],
        vec!["--stored-session", "--json", "--site", "example.com"],
        vec!["--stored-session", "--json", "--site", SITE],
    ] {
        let result = fixture
            .command()
            .args(["auth", "token"])
            .args(args)
            .env("DD_ACCESS_TOKEN", "ambient-token")
            .output()
            .unwrap();
        rejected(result);
    }
    let result = fixture
        .command()
        .args(["auth", "token"])
        .env("DD_ACCESS_TOKEN", "ambient-token")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().trim(),
        "ambient-token"
    );
}

#[test]
fn unusable_saved_tokens_fail_without_exporting_credentials() {
    for expired in [false, true] {
        let fixture = SessionFixture::new();
        fixture.register(SITE, None);
        fixture.write(
            "tokens_datadoghq_com.json",
            json!({"__default__": {
                "access_token": if expired { "fixture-access" } else { "" },
                "refresh_token": "", "issued_at": chrono::Utc::now().timestamp(),
                "expires_in": if expired { 0 } else { 3600 }, "session_id": "login"
            }}),
        );
        let error = rejected(fixture.export(None).output().unwrap());
        assert!(error.contains("pup auth login"));
    }
}

#[test]
fn logout_removes_the_registered_session_from_export() {
    let fixture = SessionFixture::session(Some("production"), "login");
    let output = fixture
        .command()
        .args(["--org", "production", "auth", "logout"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    rejected(fixture.export(Some("production")).output().unwrap());
}
