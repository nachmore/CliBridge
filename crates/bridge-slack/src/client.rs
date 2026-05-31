use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, COOKIE, HeaderMap, HeaderValue};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use bridge_core::error::BridgeError;
use bridge_core::messaging::MessagingClient;
use bridge_core::types::{Credentials, IncomingMessage};

use crate::rate_limiter::RateLimiter;

const SLACK_API_BASE: &str = "https://slack.com/api";
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Slack implementation of the MessagingClient trait.
/// Uses user tokens (xoxc-) with the d cookie for authentication.
pub struct SlackClient {
    http: reqwest::Client,
    credentials: Option<Credentials>,
    rate_limiter: RateLimiter,
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
            self_user_id: None,
        }
    }

    fn headers(&self) -> Result<HeaderMap, BridgeError> {
        let creds = self
            .credentials
            .as_ref()
            .ok_or_else(|| BridgeError::Messaging("Not connected".to_string()))?;

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", creds.token))
                .map_err(|e| BridgeError::Messaging(format!("Invalid token: {e}")))?,
        );

        if let Some(cookie) = &creds.cookie {
            headers.insert(
                COOKIE,
                HeaderValue::from_str(&format!("d={cookie}"))
                    .map_err(|e| BridgeError::Messaging(format!("Invalid cookie: {e}")))?,
            );
        }

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
        self.credentials = Some(credentials);

        // Verify the token works with auth.test
        let headers = self.headers()?;
        let resp: AuthTestResponse = self
            .http
            .post(format!("{SLACK_API_BASE}/auth.test"))
            .headers(headers)
            .send()
            .await
            .map_err(|e| BridgeError::Messaging(format!("HTTP error: {e}")))?
            .json()
            .await
            .map_err(|e| BridgeError::Messaging(format!("JSON parse error: {e}")))?;

        if !resp.ok {
            return Err(BridgeError::Auth(format!(
                "Slack auth failed: {}",
                resp.error.unwrap_or_default()
            )));
        }

        self.self_user_id = resp.user_id;
        debug!("Connected to Slack as user: {:?}", self.self_user_id);
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

        let resp: SlackResponse = self
            .http
            .post(format!("{SLACK_API_BASE}/chat.postMessage"))
            .headers(headers)
            .form(&[("channel", channel), ("text", content)])
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

        let resp: SlackResponse = self
            .http
            .post(format!("{SLACK_API_BASE}/chat.update"))
            .headers(headers)
            .form(&[("channel", channel), ("ts", message_id), ("text", content)])
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

        // Spawn a polling task to check for new messages
        tokio::spawn(async move {
            let mut last_ts: Option<String> = None;

            loop {
                tokio::time::sleep(POLL_INTERVAL).await;

                let mut headers = HeaderMap::new();
                if let Ok(auth) = HeaderValue::from_str(&format!("Bearer {}", credentials.token)) {
                    headers.insert(AUTHORIZATION, auth);
                }
                if let Some(cookie) = &credentials.cookie
                    && let Ok(cookie_val) = HeaderValue::from_str(&format!("d={cookie}"))
                {
                    headers.insert(COOKIE, cookie_val);
                }

                let mut params = vec![("channel", channel.as_str()), ("limit", "10")];

                let oldest_str;
                if let Some(ref ts) = last_ts {
                    oldest_str = ts.clone();
                    params.push(("oldest", &oldest_str));
                }

                let resp = http
                    .get(format!("{SLACK_API_BASE}/conversations.history"))
                    .headers(headers)
                    .query(&params)
                    .send()
                    .await;

                match resp {
                    Ok(response) => {
                        if let Ok(history) = response.json::<ConversationsHistoryResponse>().await {
                            if !history.ok {
                                warn!("conversations.history error: {:?}", history.error);
                                continue;
                            }

                            if let Some(messages) = history.messages {
                                // Messages come newest-first, reverse for chronological order
                                for msg in messages.iter().rev() {
                                    // Skip our own messages and subtypes (joins, etc.)
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

                                        // Update last_ts to avoid re-processing
                                        last_ts = Some(ts.clone());
                                    }
                                }

                                // Even if no messages matched, update last_ts from newest
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
            cookie: Some("xoxd-cookie".to_string()),
            workspace_url: None,
            workspace_name: None,
        });

        let headers = client.headers().unwrap();
        assert!(headers.contains_key(AUTHORIZATION));
        assert!(headers.contains_key(COOKIE));
    }
}
