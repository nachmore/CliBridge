use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{COOKIE, HeaderMap, HeaderValue};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use bridge_core::error::BridgeError;
use bridge_core::messaging::MessagingClient;
use bridge_core::types::{Credentials, IncomingMessage};

use crate::rate_limiter::RateLimiter;

const DEFAULT_API_BASE: &str = "https://slack.com/api";
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Slack implementation of the MessagingClient trait.
/// Uses user tokens (xoxc-) with the d cookie for authentication.
pub struct SlackClient {
    http: reqwest::Client,
    credentials: Option<Credentials>,
    rate_limiter: RateLimiter,
    api_base: String,
    /// Our own user ID (to filter out our own messages)
    self_user_id: Option<String>,
}

#[derive(Deserialize)]
struct SlackResponse {
    ok: bool,
    error: Option<String>,
    ts: Option<String>,
}

#[derive(Deserialize)]
struct AuthTestResponse {
    ok: bool,
    user_id: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ConversationsHistoryResponse {
    ok: bool,
    messages: Option<Vec<SlackMessage>>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct SlackMessage {
    text: Option<String>,
    user: Option<String>,
    ts: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

impl SlackClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            credentials: None,
            rate_limiter: RateLimiter::new(Duration::from_millis(1100)),
            api_base: DEFAULT_API_BASE.to_string(),
            self_user_id: None,
        }
    }

    /// Set a custom API base URL (e.g. "https://mycompany.enterprise.slack.com/api")
    pub fn with_api_base(mut self, url: &str) -> Self {
        self.api_base = url.trim_end_matches('/').to_string();
        self
    }

    fn headers(&self) -> Result<HeaderMap, BridgeError> {
        let creds = self
            .credentials
            .as_ref()
            .ok_or_else(|| BridgeError::Messaging("Not connected".to_string()))?;

        let mut headers = HeaderMap::new();

        if let Some(cookie) = &creds.cookie {
            headers.insert(
                COOKIE,
                HeaderValue::from_str(cookie)
                    .map_err(|e| BridgeError::Messaging(format!("Invalid cookie: {e}")))?,
            );
        }

        // Enterprise Slack requires Origin header
        headers.insert(
            reqwest::header::ORIGIN,
            HeaderValue::from_static("https://app.slack.com"),
        );

        Ok(headers)
    }
}

impl Default for SlackClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessagingClient for SlackClient {
    async fn connect(&mut self, credentials: Credentials) -> Result<(), BridgeError> {
        debug!(
            "Connecting with token: {}... cookie: {}",
            &credentials.token[..credentials.token.len().min(15)],
            credentials
                .cookie
                .as_deref()
                .map(|c| &c[..c.len().min(15)])
                .unwrap_or("none")
        );

        // If the workspace URL is an enterprise domain, prefer that for the API base
        // unless the caller already overrode it.
        if self.api_base == DEFAULT_API_BASE
            && let Some(url) = credentials.workspace_url.as_deref()
            && url.contains(".enterprise.slack.com")
            && let Some(origin) = origin_of(url)
        {
            self.api_base = format!("{origin}/api");
            debug!("Using enterprise API base: {}", self.api_base);
        }

        self.credentials = Some(credentials);

        let headers = self.headers()?;
        let creds = self.credentials.as_ref().unwrap();
        let resp_text = self
            .http
            .post(format!("{}/auth.test", self.api_base))
            .headers(headers.clone())
            .form(&[("token", &creds.token)])
            .send()
            .await
            .map_err(|e| BridgeError::Messaging(format!("HTTP error: {e}")))?
            .text()
            .await
            .map_err(|e| BridgeError::Messaging(format!("Response read error: {e}")))?;

        debug!("auth.test response: {resp_text}");

        let resp: AuthTestResponse = serde_json::from_str(&resp_text)
            .map_err(|e| BridgeError::Messaging(format!("JSON parse error: {e}")))?;

        if !resp.ok {
            warn!(
                "auth.test failed: {} (continuing anyway — will fail on first API call if credentials are invalid)",
                resp.error.as_deref().unwrap_or("unknown")
            );
        } else {
            self.self_user_id = resp.user_id;
            debug!("Connected to Slack as user: {:?}", self.self_user_id);
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), BridgeError> {
        self.credentials = None;
        self.self_user_id = None;
        Ok(())
    }

    async fn send_message(&self, channel: &str, content: &str) -> Result<String, BridgeError> {
        self.rate_limiter.acquire().await;
        let headers = self.headers()?;
        let creds = self.credentials.as_ref().unwrap();

        let resp: SlackResponse = self
            .http
            .post(format!("{}/chat.postMessage", self.api_base))
            .headers(headers)
            .form(&[
                ("token", creds.token.as_str()),
                ("channel", channel),
                ("text", content),
            ])
            .send()
            .await
            .map_err(|e| BridgeError::Messaging(format!("HTTP error: {e}")))?
            .json()
            .await
            .map_err(|e| BridgeError::Messaging(format!("JSON parse error: {e}")))?;

        if !resp.ok {
            return Err(BridgeError::Messaging(format!(
                "Failed to post message: {}",
                resp.error.unwrap_or_default()
            )));
        }

        resp.ts
            .ok_or_else(|| BridgeError::Messaging("No timestamp in response".to_string()))
    }

    async fn edit_message(
        &self,
        channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), BridgeError> {
        self.rate_limiter.acquire().await;
        let headers = self.headers()?;
        let creds = self.credentials.as_ref().unwrap();

        let resp: SlackResponse = self
            .http
            .post(format!("{}/chat.update", self.api_base))
            .headers(headers)
            .form(&[
                ("token", creds.token.as_str()),
                ("channel", channel),
                ("ts", message_id),
                ("text", content),
            ])
            .send()
            .await
            .map_err(|e| BridgeError::Messaging(format!("HTTP error: {e}")))?
            .json()
            .await
            .map_err(|e| BridgeError::Messaging(format!("JSON parse error: {e}")))?;

        if !resp.ok {
            return Err(BridgeError::Messaging(format!(
                "Failed to edit message: {}",
                resp.error.unwrap_or_default()
            )));
        }

        Ok(())
    }

    async fn subscribe(
        &mut self,
        channel: &str,
    ) -> Result<mpsc::Receiver<IncomingMessage>, BridgeError> {
        let (tx, rx) = mpsc::channel(64);
        let channel = channel.to_string();
        let self_user_id = self.self_user_id.clone();
        let credentials = self
            .credentials
            .clone()
            .ok_or_else(|| BridgeError::Messaging("Not connected".to_string()))?;

        let http = self.http.clone();
        let api_base = self.api_base.clone();

        // Spawn a polling task to check for new messages
        tokio::spawn(async move {
            let mut last_ts: Option<String> = None;

            loop {
                tokio::time::sleep(POLL_INTERVAL).await;

                let mut headers = HeaderMap::new();
                if let Some(cookie) = &credentials.cookie
                    && let Ok(cookie_val) = HeaderValue::from_str(cookie)
                {
                    headers.insert(COOKIE, cookie_val);
                }
                headers.insert(
                    reqwest::header::ORIGIN,
                    HeaderValue::from_static("https://app.slack.com"),
                );

                let mut form: Vec<(&str, String)> = vec![
                    ("token", credentials.token.clone()),
                    ("channel", channel.clone()),
                    ("limit", "10".to_string()),
                ];
                if let Some(ref ts) = last_ts {
                    form.push(("oldest", ts.clone()));
                }

                let resp = http
                    .post(format!("{api_base}/conversations.history"))
                    .headers(headers)
                    .form(&form)
                    .send()
                    .await;

                match resp {
                    Ok(response) => {
                        if let Ok(history) =
                            response.json::<ConversationsHistoryResponse>().await
                        {
                            if !history.ok {
                                warn!("conversations.history error: {:?}", history.error);
                                continue;
                            }

                            if let Some(messages) = history.messages {
                                for msg in messages.iter().rev() {
                                    if msg.subtype.is_some() {
                                        continue;
                                    }
                                    if let Some(ref user) = msg.user
                                        && Some(user) == self_user_id.as_ref()
                                    {
                                        continue;
                                    }

                                    if let (Some(text), Some(user), Some(ts)) =
                                        (&msg.text, &msg.user, &msg.ts)
                                    {
                                        let incoming = IncomingMessage {
                                            channel: channel.clone(),
                                            text: text.clone(),
                                            user: user.clone(),
                                            timestamp: ts.clone(),
                                        };

                                        if tx.send(incoming).await.is_err() {
                                            debug!("Subscriber channel closed");
                                            return;
                                        }

                                        last_ts = Some(ts.clone());
                                    }
                                }

                                if last_ts.is_none()
                                    && let Some(newest) = messages.first()
                                {
                                    last_ts = newest.ts.clone();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to poll messages: {e}");
                    }
                }
            }
        });

        Ok(rx)
    }
}

fn origin_of(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let after = &url[scheme_end + 3..];
    let host_end = after.find('/').unwrap_or(after.len());
    Some(format!("{}://{}", &url[..scheme_end], &after[..host_end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slack_client_new() {
        let client = SlackClient::new();
        assert!(client.credentials.is_none());
        assert!(client.self_user_id.is_none());
    }

    #[test]
    fn test_headers_without_credentials() {
        let client = SlackClient::new();
        assert!(client.headers().is_err());
    }

    #[test]
    fn test_headers_with_credentials() {
        let mut client = SlackClient::new();
        client.credentials = Some(Credentials {
            token: "xoxc-test".to_string(),
            cookie: Some("d=xoxd-cookie".to_string()),
            workspace_url: None,
            workspace_name: None,
        });

        let headers = client.headers().unwrap();
        assert!(headers.contains_key(COOKIE));
    }

    #[test]
    fn test_origin_of() {
        assert_eq!(
            origin_of("https://acme.enterprise.slack.com/messages/foo"),
            Some("https://acme.enterprise.slack.com".to_string())
        );
    }
}
