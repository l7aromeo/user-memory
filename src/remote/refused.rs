//! What a client says when the memory server refuses this machine's token
//! and says why: one sentence per reason, naming the fix, used by every path
//! that meets the refusal (doctor's probe, the hooks' and the CLI's calls,
//! export and import, and the bridge). It uses only what the client knows:
//! the server's URL.

use rmcp::{
    service::{ClientInitializeError, ServiceError},
    transport::streamable_http_client::AuthRequiredError,
};

/// Why the server refused the token, from its 401's `error_description`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Revoked,
    /// Unused for 90 days.
    Unused,
    /// Past the date its operator set.
    Expired,
}

impl Reason {
    /// The reason a `WWW-Authenticate` value gives; `None` without one, or
    /// with one this client does not know, which then gets today's message.
    pub fn from_challenge(challenge: &str) -> Option<Self> {
        let (_, rest) = challenge.split_once("error_description=\"")?;
        let (description, _) = rest.split_once('"')?;
        match description {
            "revoked" => Some(Self::Revoked),
            "unused" => Some(Self::Unused),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }

    /// The reason in a failure's chain: rmcp reports a 401 as an
    /// [`AuthRequiredError`] with the server's challenge.
    pub fn of(err: &anyhow::Error) -> Option<Self> {
        err.chain().find_map(Self::in_chain)
    }

    /// The reason in `cause` or what it wraps. A failed handshake or call
    /// does not give its transport error as its source: a handshake's
    /// challenge comes from rmcp's own accessor, and a call's (which has
    /// none) from the transport error it wraps.
    fn in_chain(cause: &(dyn std::error::Error + 'static)) -> Option<Self> {
        let challenge = if let Some(handshake) = cause.downcast_ref::<ClientInitializeError>() {
            handshake.auth_challenge()
        } else {
            let start: &(dyn std::error::Error + 'static) = match cause.downcast_ref() {
                // rmcp has no accessor for a call's challenge, as
                // `auth_challenge` is for the handshake's; use one if added.
                Some(ServiceError::TransportSend(error)) => error,
                _ => cause,
            };
            std::iter::successors(Some(start), |cause| cause.source())
                .find_map(|cause| cause.downcast_ref::<AuthRequiredError>())
                .map(|refused| refused.www_authenticate_header.as_str())
        };
        challenge.and_then(Self::from_challenge)
    }

    /// What the user is told for the server at `url`.
    pub fn sentence(self, url: &str) -> String {
        let fix = format!(
            "Ask its operator for a new token file, then run the installer with --url {url} --token-file FILE."
        );
        match self {
            Self::Revoked => {
                format!("The memory server at {url} revoked this machine's token. {fix}")
            }
            Self::Unused => {
                format!("This machine's token for {url} expired after 90 days unused. {fix}")
            }
            Self::Expired => format!(
                "This machine's token for {url} expired on the date its operator set. {fix}"
            ),
        }
    }
}

/// `err`, as the client tells it: the sentence for a token the server
/// refused with a known reason, else `err` as before, with `context` when
/// given.
pub fn told(
    err: impl Into<anyhow::Error>,
    url: &str,
    context: Option<&'static str>,
) -> anyhow::Error {
    let err = err.into();
    match (Reason::of(&err), context) {
        (Some(reason), _) => anyhow::anyhow!(reason.sentence(url)),
        (None, Some(context)) => err.context(context),
        (None, None) => err,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_reason_is_read_from_the_challenge() {
        for (challenge, reason) in [
            (
                r#"Bearer error="invalid_token", error_description="revoked""#,
                Some(Reason::Revoked),
            ),
            (
                r#"Bearer error="invalid_token", error_description="unused""#,
                Some(Reason::Unused),
            ),
            (
                r#"Bearer error="invalid_token", error_description="expired""#,
                Some(Reason::Expired),
            ),
            ("Bearer", None),
            // A reason from a later server: today's message.
            (
                r#"Bearer error="invalid_token", error_description="sign in required""#,
                None,
            ),
        ] {
            assert_eq!(Reason::from_challenge(challenge), reason, "{challenge}");
        }
    }

    #[test]
    fn each_sentence_names_the_server_the_operator_and_the_installer() {
        let url = "https://memory.example.com/mcp";
        for reason in [Reason::Revoked, Reason::Unused, Reason::Expired] {
            let said = reason.sentence(url);
            assert!(said.contains(url), "{said}");
            assert!(said.contains("operator"), "{said}");
            assert!(
                said.contains("--url https://memory.example.com/mcp --token-file FILE"),
                "{said}"
            );
            assert!(!said.contains("login"), "{said}");
        }
    }
}
