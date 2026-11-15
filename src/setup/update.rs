//! doctor's notice of a newer release. One request to the public releases
//! page, which redirects `latest` to the newest release's tag; nothing else
//! is sent, and a failed check says nothing.

use super::versions::Version;
use std::{env, time::Duration};

/// The public releases, where the installer downloads from;
/// `USER_MEMORY_RELEASES` points the check elsewhere, as for the installer.
pub(super) const RELEASES: &str = "https://github.com/l7aromeo/user-memory/releases";
/// Enough for one round trip to GitHub; an offline doctor barely waits.
const TIMEOUT: Duration = Duration::from_secs(3);
/// Set to 1, it turns the check off, as `setup --no-update-check` does.
pub(super) const OFF: &str = "USER_MEMORY_NO_UPDATE_CHECK";

/// Whether the environment turns the check off.
pub(super) fn off_by_environment() -> bool {
    env::var(OFF).is_ok_and(|value| value.trim() == "1")
}

/// The line doctor prints when a release newer than `current` exists;
/// `None` when there is none, or the check failed.
pub(super) async fn notice(current: &str) -> Option<String> {
    let latest = latest(&releases_base()).await?;
    newer(&latest, current).then(|| {
        format!(
            "user-memory {latest} is available (this is {current}): run \"user-memory upgrade\""
        )
    })
}

/// Where the releases are: [`RELEASES`], or `USER_MEMORY_RELEASES`.
pub(super) fn releases_base() -> String {
    env::var("USER_MEMORY_RELEASES")
        .unwrap_or_else(|_| RELEASES.into())
        .trim_end_matches('/')
        .to_owned()
}

/// Whether `url` may be fetched: over HTTPS, or plain http to this machine.
pub(super) fn allowed(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        let here = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        url.scheme() == "https" || (url.scheme() == "http" && here)
    })
}

/// The newest release's version, from where `<base>/latest` redirects:
/// `…/releases/tag/vX.Y.Z`. Only over HTTPS, or plain http to this machine.
pub(super) async fn latest(base: &str) -> Option<String> {
    let url = format!("{}/latest", base.trim_end_matches('/'));
    if !allowed(&url) {
        return None;
    }
    let url = reqwest::Url::parse(&url).ok()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?;
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()?;
    let (_, tag) = location.rsplit_once("/tag/")?;
    Some(tag.strip_prefix('v').unwrap_or(tag).to_owned())
}

/// Whether release `latest` is newer than `current`, both `X.Y.Z`. A
/// version in any other form, such as a pre-release, is never newer.
fn newer(latest: &str, current: &str) -> bool {
    matches!(
        (Version::parse(latest), Version::parse(current)),
        (Some(latest), Some(current)) if latest > current
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_higher_release_number_is_newer() {
        for (latest, current, expected) in [
            ("0.6.0", "0.5.0", true),
            ("0.10.0", "0.9.9", true),
            ("1.0.0", "0.99.99", true),
            ("0.5.1", "0.5.0", true),
            ("0.5.0", "0.5.0", false),
            ("0.4.9", "0.5.0", false),
            ("0.6.0-rc.1", "0.5.0", false),
            ("0.6", "0.5.0", false),
            ("0.6.0.1", "0.5.0", false),
            ("latest", "0.5.0", false),
        ] {
            assert_eq!(newer(latest, current), expected, "{latest} over {current}");
        }
    }
}
