use common::{api_bindings::RtcIceServer, config::WebRtcConfig};
use log::error;
use std::time::Duration;
use tokio::{process::Command, time::timeout};
use tracing::debug;

pub async fn load_dynamic_ice_servers(config: &WebRtcConfig) -> Vec<RtcIceServer> {
    let Some(script_command) = config.ice_server_script.as_ref() else {
        debug!("No WebRTC ice server script found");
        return vec![];
    };

    debug!(script = script_command, "Running WebRTC ice server script");

    let mut script = Command::new(script_command);

    let output = match timeout(Duration::from_secs(5), script.output()).await {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => {
            error!("Failed to run WebRTC ice server script: {err}");
            return vec![];
        }
        Err(_) => {
            error!(
                "WebRTC ice server script timed out after 5 seconds; using configured ICE servers only"
            );
            return vec![];
        }
    };

    if !matches!(output.status.code(), None | Some(0)) {
        error!(
            "WebRTC ice server script has a non zero exit code: {}",
            output.status
        );

        if let Ok(error) = String::from_utf8(output.stdout) {
            error!("WebRTC ice server script error:\n{error}");
        }
        return vec![];
    }

    let json: Vec<RtcIceServer> = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(err) => {
            error!("Failed to deserialize WebRTC ice server script output: {err}");
            return vec![];
        }
    };

    json
}
