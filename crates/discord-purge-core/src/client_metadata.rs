//! Runtime device settings and the native Discord desktop client profile.
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

// The PTB desktop host on 2026-10-24: Electron and its Chromium are shared by
// every platform. Client versions are from Discord's public update endpoints;
// the native build number is only published in the macOS host.
const ELECTRON_VERSION: &str = "44.9.3";
const CHROME_VERSION: &str = "152.0.7977.167";
const MACOS_CLIENT_VERSION: &str = "0.0.268";
const MACOS_NATIVE_BUILD: u64 = 96819;
const WINDOWS_CLIENT_VERSION: &str = "1.0.1229";
const LINUX_CLIENT_VERSION: &str = "1.0.224";

/// The order in which the desktop client writes X-Super-Properties fields.
const PROPERTY_ORDER: [&str; 19] = [
    "os",
    "browser",
    "release_channel",
    "client_version",
    "os_version",
    "os_arch",
    "app_arch",
    "system_locale",
    "has_client_mods",
    "client_launch_id",
    "browser_user_agent",
    "browser_version",
    "os_sdk_version",
    "client_build_number",
    "native_build_number",
    "client_event_source",
    "launch_signature",
    "client_heartbeat_session_id",
    "client_app_state",
];

pub(crate) struct ClientMetadata {
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) properties: Map<String, Value>,
}

impl ClientMetadata {
    pub(crate) fn current() -> anyhow::Result<Arc<Self>> {
        static METADATA: OnceLock<Arc<ClientMetadata>> = OnceLock::new();
        if let Some(metadata) = METADATA.get() {
            return Ok(metadata.clone());
        }
        let metadata = Arc::new(Self::detect()?);
        Ok(METADATA.get_or_init(|| metadata).clone())
    }

    pub(crate) fn detect() -> anyhow::Result<Self> {
        let (os, platform, ch_platform, client_version, native_build) = match std::env::consts::OS {
            "macos" => (
                "Mac OS X",
                "Macintosh; Intel Mac OS X 10_15_7",
                "macOS",
                MACOS_CLIENT_VERSION,
                Some(MACOS_NATIVE_BUILD),
            ),
            "windows" => (
                "Windows",
                "Windows NT 10.0; Win64; x64",
                "Windows",
                WINDOWS_CLIENT_VERSION,
                None,
            ),
            _ => (
                "Linux",
                "X11; Linux x86_64",
                "Linux",
                LINUX_CLIENT_VERSION,
                None,
            ),
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => other,
        };
        let os_version = os_version()?;
        let user_agent = format!(
            "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) discord/{client_version} Chrome/{CHROME_VERSION} Electron/{ELECTRON_VERSION} Safari/537.36"
        );
        let locale = std::env::var("LC_ALL")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var("LC_MESSAGES").ok().filter(|v| !v.is_empty()))
            .or_else(|| std::env::var("LANG").ok())
            .unwrap_or_else(|| "en-US".into());
        let locale = locale
            .split(['.', '@'])
            .next()
            .unwrap_or("en-US")
            .replace('_', "-");
        let locale = if locale == "C" || locale == "POSIX" || locale.is_empty() {
            "en-US".to_string()
        } else {
            locale
        };
        let timezone = iana_time_zone::get_timezone().context("Cannot detect your system timezone. Set a valid timezone in your operating system settings, then restart the app.")?;
        let headers = [
            ("accept", "*/*".to_string()),
            ("accept-encoding", "gzip, deflate, br, zstd".into()),
            ("accept-language", locale.clone()),
            ("priority", "u=1, i".into()),
            ("referer", "https://ptb.discord.com/channels/@me".into()),
            (
                "sec-ch-ua",
                format!(
                    "\"Not?A_Brand\";v=\"24\", \"Chromium\";v=\"{}\"",
                    CHROME_VERSION.split('.').next().unwrap_or_default()
                ),
            ),
            ("sec-ch-ua-mobile", "?0".into()),
            ("sec-ch-ua-platform", format!("\"{ch_platform}\"")),
            ("sec-fetch-dest", "empty".into()),
            ("sec-fetch-mode", "cors".into()),
            ("sec-fetch-site", "same-origin".into()),
            ("user-agent", user_agent.clone()),
            ("x-debug-options", "bugReporterEnabled".into()),
            ("x-discord-locale", locale.clone()),
            ("x-discord-timezone", timezone),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut properties: Map<String, Value> = serde_json::from_value(json!({
            "os": os, "browser": "Discord Client", "release_channel": "ptb",
            "client_version": client_version, "os_version": os_version,
            "os_arch": arch, "app_arch": arch, "system_locale": locale,
            "has_client_mods": false, "browser_user_agent": user_agent,
            "browser_version": ELECTRON_VERSION, "native_build_number": native_build,
            "client_event_source": null, "client_app_state": "unfocused",
        }))?;
        if os == "Mac OS X" {
            // macOS reports the Darwin major version as its SDK version.
            let sdk = os_version.split('.').next().unwrap_or_default().to_string();
            properties.insert("os_sdk_version".into(), Value::String(sdk));
        }
        let metadata = Self {
            headers,
            properties,
        };
        metadata.header_map()?;
        Ok(metadata)
    }

    pub(crate) fn header_map(&self) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes())?;
            let mut value = HeaderValue::from_str(value).context("A detected device setting is not a valid HTTP header. Check LANG, LC_ALL, LC_MESSAGES, and the system timezone, then restart the app.")?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

pub(crate) fn encode_properties(properties: &Map<String, Value>) -> String {
    let known = PROPERTY_ORDER
        .iter()
        .copied()
        .filter(|k| properties.contains_key(*k));
    let other = properties
        .keys()
        .map(String::as_str)
        .filter(|k| !PROPERTY_ORDER.contains(k));
    let fields: Vec<String> = known
        .chain(other)
        .map(|key| format!("{}:{}", Value::from(key), properties[key]))
        .collect();
    STANDARD.encode(format!("{{{}}}", fields.join(",")))
}

fn os_version() -> anyhow::Result<String> {
    let failed = "Cannot detect the operating system version. Make sure the `uname` command (or `cmd` on Windows) is available, then restart the app.";
    let output = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "ver"])
            .output()
    } else {
        std::process::Command::new("uname").arg("-r").output()
    }
    .context(failed)?;
    anyhow::ensure!(output.status.success(), failed);
    let text = String::from_utf8_lossy(&output.stdout);
    let version = if cfg!(windows) {
        // "Microsoft Windows [Version 10.0.26100.1234]" -> "10.0.26100"
        let numbers = text
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find(|part| part.matches('.').count() >= 2)
            .unwrap_or_default();
        numbers.split('.').take(3).collect::<Vec<_>>().join(".")
    } else {
        text.trim().to_string()
    };
    anyhow::ensure!(!version.is_empty(), failed);
    Ok(version)
}
