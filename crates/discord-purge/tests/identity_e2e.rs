//! The identity command makes fresh private IDs and leaves other state alone.
use base64::Engine as _;
use std::process::Command;

#[test]
fn identity_creates_fresh_private_ids_and_keeps_token_and_progress() {
    let state = tempfile::tempdir().unwrap();
    let cache_path = state.path().join(".client-identity.json");
    std::fs::write(state.path().join("token"), b"separate-test-token").unwrap();
    std::fs::write(state.path().join("user-7.deleted.log"), b"10:20\n").unwrap();
    let mut previous_launch = None;
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_discord-purge"))
            .arg("identity")
            .env("DISCORD_PURGE_STATE_DIR", state.path())
            .env("DISCORD_PURGE_IDENTITY_DIR", state.path())
            .env_remove("DISCORD_TOKEN")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(&cache_path).unwrap();
        let cache: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(cache["version"], 2);
        assert!(cache["installation_id"].is_null());
        let properties: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(cache["super_properties"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        let launch = properties["client_launch_id"].as_str().unwrap().to_string();
        assert_eq!(launch.len(), 36);
        assert_eq!(&launch[14..15], "4");
        assert_ne!(previous_launch.as_ref(), Some(&launch));
        previous_launch = Some(launch);
        assert!(properties.get("launch_signature").is_none());
        assert!(matches!(
            properties["os_arch"].as_str(),
            Some("arm64" | "x64")
        ));
        assert!(
            !String::from_utf8_lossy(&output.stdout)
                .contains(cache["super_properties"].as_str().unwrap())
        );
        assert_eq!(
            std::fs::read(state.path().join("token")).unwrap(),
            b"separate-test-token"
        );
        assert_eq!(
            std::fs::read(state.path().join("user-7.deleted.log")).unwrap(),
            b"10:20\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&cache_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn corrupt_runtime_cache_is_preserved_and_reports_recovery() {
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join(".client-identity.json");
    std::fs::write(&path, b"invalid JSON").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_discord-purge"))
        .arg("identity")
        .env("DISCORD_PURGE_IDENTITY_DIR", state.path())
        .env("DISCORD_PURGE_STATE_DIR", state.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Remove this cache file and retry"));
    assert_eq!(std::fs::read(path).unwrap(), b"invalid JSON");
}
