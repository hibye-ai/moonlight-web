use moonlight_common::{
    MoonlightError,
    high::MoonlightClientError,
    http::{ParseError, client::tokio_hyper::HyperError},
};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HostLaunchFailure {
    #[error("host app launch rejected")]
    Rejected,
    #[error("a stream is already active")]
    AlreadyActive,
    #[error("host start request failed")]
    Other,
}

impl HostLaunchFailure {
    pub(crate) fn outcome(&self) -> &'static str {
        match self {
            Self::Rejected => "app_launch_rejected",
            Self::AlreadyActive => "already_active",
            Self::Other => "failed",
        }
    }
}

impl From<&MoonlightClientError> for HostLaunchFailure {
    fn from(error: &MoonlightClientError) -> Self {
        match error {
            MoonlightClientError::Moonlight(MoonlightError::ConnectionAlreadyExists) => {
                Self::AlreadyActive
            }
            MoonlightClientError::Backend(error) => match error.downcast_ref::<HyperError>() {
                // The pinned client drops the numeric XML status code. Restrict the
                // classification to Sunshine's exact launch rejection message.
                Some(HyperError::Parse(ParseError::InvalidXmlStatusCode {
                    message: Some(message),
                })) if message == "Failed to start the specified application" => Self::Rejected,
                _ => Self::Other,
            },
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonlight_common::http::launch::LaunchResponse;

    fn classify(xml: &str) -> HostLaunchFailure {
        let parse_error = xml
            .parse::<LaunchResponse>()
            .expect_err("rejection fixture");
        let error = MoonlightClientError::Backend(Box::new(HyperError::Parse(parse_error)));
        HostLaunchFailure::from(&error)
    }

    #[test]
    fn sunshine_launch_rejection_has_a_safe_terminal_classification() {
        let failure = classify(
            r#"<root status_code="503" status_message="Failed to start the specified application"><gamesession>0</gamesession></root>"#,
        );
        assert_eq!(failure, HostLaunchFailure::Rejected);
        assert_eq!(failure.to_string(), "host app launch rejected");
    }

    #[test]
    fn unknown_xml_failures_are_not_classified_as_app_rejections() {
        for xml in [
            r#"<root status_code="503" status_message="Temporarily unavailable at rtsp://test.invalid/secret"/>"#,
            r#"<root status_code="503"/>"#,
            "<invalid",
        ] {
            let failure = classify(xml);
            assert_eq!(failure, HostLaunchFailure::Other);
            assert_eq!(failure.to_string(), "host start request failed");
        }
        let conflict = MoonlightClientError::Moonlight(MoonlightError::ConnectionAlreadyExists);
        assert_eq!(
            HostLaunchFailure::from(&conflict),
            HostLaunchFailure::AlreadyActive
        );
    }
}
