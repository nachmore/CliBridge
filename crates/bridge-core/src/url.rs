//! Tiny URL helpers shared between bridge-auth (capturing the workspace URL
//! during login) and bridge-slack (deriving an enterprise API base from it).
//!
//! We deliberately don't pull in a full URL parsing crate — we only need
//! origin extraction and the inputs we see (Slack workspace URLs) are
//! well-formed.

/// Extract the origin (scheme + host) from a URL, e.g.
/// `https://acme.slack.com/messages/...` → `https://acme.slack.com`.
/// Returns `None` if the input has no scheme.
pub fn origin_of(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let after = &url[scheme_end + 3..];
    let host_end = after.find('/').unwrap_or(after.len());
    Some(format!("{}://{}", &url[..scheme_end], &after[..host_end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enterprise() {
        assert_eq!(
            origin_of("https://acme.enterprise.slack.com/messages/foo"),
            Some("https://acme.enterprise.slack.com".to_string())
        );
    }

    #[test]
    fn standard() {
        assert_eq!(
            origin_of("https://acme.slack.com/"),
            Some("https://acme.slack.com".to_string())
        );
    }

    #[test]
    fn no_path() {
        assert_eq!(
            origin_of("https://app.slack.com"),
            Some("https://app.slack.com".to_string())
        );
    }

    #[test]
    fn rejects_non_url() {
        assert_eq!(origin_of("not-a-url"), None);
        assert_eq!(origin_of(""), None);
    }
}
