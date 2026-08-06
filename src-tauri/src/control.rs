//! Lightweight control-client mode used by compositor key bindings.
//!
//! `flov_app --record-start` and `--record-stop` exit before GTK/Tauri is
//! initialized. Hyprland can therefore invoke the installed application on
//! key press/release without launching a second desktop instance.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};

const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:17432";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Start,
    Stop,
    Health,
}

/// Map a CLI argument to a control command. Pure function so the flag
/// parsing stays regression-tested; unknown flags (including
/// `--headless-server`) fall through to the other launch modes.
fn command_for(argument: &str) -> Option<Command> {
    match argument {
        "--record-start" => Some(Command::Start),
        "--record-stop" => Some(Command::Stop),
        "--server-health" => Some(Command::Health),
        _ => None,
    }
}

/// Accept any 2xx status; reject everything else with the HTTP status and
/// request path. The error deliberately omits the response body and any
/// credentials, so a misbehaving server cannot flood stderr through us.
fn ensure_success(status: u16, path: &str) -> Result<()> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(anyhow!("flov returned HTTP {status} for {path}"))
    }
}

pub fn run_if_requested() -> Result<bool> {
    let Some(argument) = std::env::args().nth(1) else {
        return Ok(false);
    };
    let Some(command) = command_for(&argument) else {
        return Ok(false);
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
        // Non-2xx statuses are checked explicitly below so the failure
        // message carries the HTTP status and path.
        .http_status_as_error(false)
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

    ensure_success(response.status().as_u16(), path)?;

    if matches!(command, Command::Health) {
        let body = response
            .body_mut()
            .read_to_string()
            .context("failed to read health response")?;
        println!("{body}");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_flags_are_recognized() {
        assert_eq!(command_for("--record-start"), Some(Command::Start));
        assert_eq!(command_for("--record-stop"), Some(Command::Stop));
        assert_eq!(command_for("--server-health"), Some(Command::Health));
    }

    #[test]
    fn other_modes_are_not_control_commands() {
        assert_eq!(command_for("--headless-server"), None);
        assert_eq!(command_for("--record"), None);
        assert_eq!(command_for(""), None);
    }

    #[test]
    fn success_statuses_are_accepted() {
        for status in [200u16, 201, 204, 299] {
            assert!(
                ensure_success(status, "/v1/health").is_ok(),
                "status {status} must be accepted"
            );
        }
    }

    #[test]
    fn failure_status_is_rejected_with_status_and_path() {
        let error = ensure_success(503, "/v1/recording/start").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("503"), "missing status: {message}");
        assert!(
            message.contains("/v1/recording/start"),
            "missing path: {message}"
        );
    }
}
