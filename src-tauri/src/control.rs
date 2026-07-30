//! Lightweight control-client mode used by compositor key bindings.
//!
//! `flov_app --record-start` and `--record-stop` exit before GTK/Tauri is
//! initialized. Hyprland can therefore invoke the installed application on
//! key press/release without launching a second desktop instance.

use std::time::Duration;

use anyhow::{Context, Result};

const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:17432";

#[derive(Clone, Copy)]
enum Command {
    Start,
    Stop,
    Health,
}

pub fn run_if_requested() -> Result<bool> {
    let Some(argument) = std::env::args().nth(1) else {
        return Ok(false);
    };
    let command = match argument.as_str() {
        "--record-start" => Command::Start,
        "--record-stop" => Command::Stop,
        "--server-health" => Command::Health,
        _ => return Ok(false),
    };

    let base_url = std::env::var("FLOV_SERVER_URL")
        .unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let (method, path) = match command {
        Command::Start => ("POST", "/v1/recording/start"),
        Command::Stop => ("POST", "/v1/recording/stop"),
        Command::Health => ("GET", "/v1/health"),
    };
    let url = format!("{base_url}{path}");
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build();
    let agent: ureq::Agent = config.into();
    let api_key = std::env::var("FLOV_API_KEY").unwrap_or_default();

    let mut response = if method == "POST" {
        let mut request = agent.post(&url);
        if !api_key.is_empty() {
            request = request.header("Authorization", &format!("Bearer {api_key}"));
        }
        request.send_empty()
    } else {
        let mut request = agent.get(&url);
        if !api_key.is_empty() {
            request = request.header("Authorization", &format!("Bearer {api_key}"));
        }
        request.call()
    }
    .with_context(|| format!("cannot reach flov at {base_url}"))?;

    if matches!(command, Command::Health) {
        let body = response
            .body_mut()
            .read_to_string()
            .context("failed to read health response")?;
        println!("{body}");
    }
    Ok(true)
}
