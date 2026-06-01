use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::run_return::EventLoopExtRunReturn,
    window::WindowBuilder,
};
use tracing::{debug, info, warn};
use wry::{WebViewBuilder, http::Request};

use bridge_core::types::Credentials;
use bridge_core::url::origin_of;

const SIGNIN_URL: &str = "https://slack.com/signin";

/// JS injected into every page. Hooks fetch() and XHR.send() so that any time
/// the Slack web client calls an `xoxc-`-bearing API (login completes the moment
/// the client starts hydrating), we can lift the token out of the request body
/// and post it back to Rust.
const TOKEN_SNIFFER_JS: &str = r#"
(function() {
  if (window.__cliBridgeAuthInstalled) return;
  window.__cliBridgeAuthInstalled = true;

  function send(payload) {
    try {
      window.ipc.postMessage(JSON.stringify(payload));
    } catch (e) { /* webview not ready yet */ }
  }

  function extractXoxc(s) {
    if (!s || typeof s !== 'string') return null;
    var m = s.match(/xoxc-[A-Za-z0-9-]{20,}/);
    return m ? m[0] : null;
  }

  function bodyToString(body) {
    if (!body) return null;
    if (typeof body === 'string') return body;
    if (body instanceof FormData) {
      var t = body.get && body.get('token');
      if (t) return 'token=' + t;
    }
    if (body instanceof URLSearchParams) return body.toString();
    return null;
  }

  function report(token, url) {
    if (!token) return;
    if (window.__cliBridgeReportedToken === token) return;
    window.__cliBridgeReportedToken = token;
    var apiBase = null;
    try {
      var u = new URL(url, window.location.origin);
      var idx = u.pathname.indexOf('/api/');
      if (idx >= 0) {
        apiBase = u.origin + u.pathname.substring(0, idx + 4);
      } else {
        apiBase = u.origin + '/api';
      }
    } catch (e) {}
    send({ kind: 'token', token: token, apiBase: apiBase, href: window.location.href });
  }

  // fetch hook
  var origFetch = window.fetch;
  if (origFetch) {
    window.fetch = function(input, init) {
      try {
        var url = typeof input === 'string' ? input : (input && input.url) || '';
        var body = init && init.body ? bodyToString(init.body) : null;
        var token = extractXoxc(body);
        if (token) report(token, url);
      } catch (e) {}
      return origFetch.apply(this, arguments);
    };
  }

  // XHR hook
  var origSend = XMLHttpRequest.prototype.send;
  var origOpen = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function(method, url) {
    this.__cliBridgeUrl = url;
    return origOpen.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function(body) {
    try {
      var s = bodyToString(body);
      var token = extractXoxc(s);
      if (token) report(token, this.__cliBridgeUrl || '');
    } catch (e) {}
    return origSend.apply(this, arguments);
  };
})();
"#;

#[derive(Debug, Clone)]
struct CapturedAuth {
    token: String,
    api_base: Option<String>,
    href: Option<String>,
}

/// Open a WebView2 window pointed at slack.com/signin, let the user log in,
/// and capture the resulting xoxc- token + slack cookies once the web client
/// makes its first authenticated API call.
pub fn login() -> Result<Credentials> {
    info!(
        "Opening Slack login window. Sign in to your workspace; this window will close automatically."
    );

    let mut event_loop: tao::event_loop::EventLoop<UserEvent> =
        EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("CliBridge — Slack Login")
        .with_inner_size(tao::dpi::LogicalSize::new(900.0, 760.0))
        .build(&event_loop)
        .context("Failed to create login window")?;

    let captured: Arc<Mutex<Option<CapturedAuth>>> = Arc::new(Mutex::new(None));
    let captured_for_handler = captured.clone();
    let proxy_for_handler = proxy.clone();

    let ipc_handler = move |req: Request<String>| {
        let body = req.body();
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                debug!("ipc message not JSON: {e}: {body}");
                return;
            }
        };
        if parsed.get("kind").and_then(|v| v.as_str()) != Some("token") {
            return;
        }
        let token = match parsed.get("token").and_then(|v| v.as_str()) {
            Some(t) if t.starts_with("xoxc-") && t.len() >= 25 => t.to_string(),
            _ => return,
        };
        let api_base = parsed
            .get("apiBase")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let href = parsed
            .get("href")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        debug!("Captured token via IPC: {}…", &token[..token.len().min(20)]);
        let mut slot = captured_for_handler.lock().unwrap();
        if slot.is_some() {
            return;
        }
        *slot = Some(CapturedAuth {
            token,
            api_base,
            href,
        });
        let _ = proxy_for_handler.send_event(UserEvent::TokenCaptured);
    };

    let webview = WebViewBuilder::new()
        .with_url(SIGNIN_URL)
        .with_initialization_script(TOKEN_SNIFFER_JS)
        .with_ipc_handler(ipc_handler)
        .build(&window)
        .context("Failed to build webview")?;

    let mut result: Option<Credentials> = None;
    let captured_for_loop = captured.clone();

    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::TokenCaptured) => {
                let auth = match captured_for_loop.lock().unwrap().clone() {
                    Some(a) => a,
                    None => return,
                };

                let cookie_header = match collect_slack_cookies(&webview, auth.href.as_deref()) {
                    Ok(c) if !c.is_empty() => c,
                    Ok(_) => {
                        warn!("No cookies found yet — waiting for next API call");
                        captured_for_loop.lock().unwrap().take();
                        return;
                    }
                    Err(e) => {
                        warn!("Failed to read cookies: {e} — waiting for next API call");
                        captured_for_loop.lock().unwrap().take();
                        return;
                    }
                };

                let workspace_url = auth
                    .href
                    .as_deref()
                    .and_then(workspace_url_from_href)
                    .or_else(|| auth.api_base.clone());

                info!("Captured Slack credentials, closing login window");
                result = Some(Credentials {
                    token: auth.token,
                    cookie: Some(cookie_header),
                    workspace_url,
                    workspace_name: None,
                });
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    result.ok_or_else(|| anyhow::anyhow!("Login window closed before credentials were captured"))
}

#[derive(Debug)]
enum UserEvent {
    TokenCaptured,
}

fn collect_slack_cookies(webview: &wry::WebView, href: Option<&str>) -> Result<String> {
    // Try the href the page reported first, then fall back to common Slack origins.
    let mut urls: Vec<String> = Vec::new();
    if let Some(h) = href
        && let Some(origin) = origin_of(h)
    {
        urls.push(origin);
    }
    urls.push("https://app.slack.com".to_string());
    urls.push("https://slack.com".to_string());

    let mut seen = std::collections::HashSet::new();
    let mut parts: Vec<String> = Vec::new();
    let mut have_d = false;

    for url in &urls {
        let cookies = match webview.cookies_for_url(url) {
            Ok(cs) => cs,
            Err(e) => {
                debug!("cookies_for_url({url}) failed: {e}");
                continue;
            }
        };
        for c in cookies {
            let name = c.name().to_string();
            if seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            let value = c.value().to_string();
            if name == "d" && value.starts_with("xoxd-") {
                have_d = true;
            }
            parts.push(format!("{name}={value}"));
        }
    }

    if !have_d {
        bail!("did not see 'd' cookie yet");
    }

    Ok(parts.join("; "))
}

fn workspace_url_from_href(href: &str) -> Option<String> {
    // Examples:
    //   https://app.slack.com/client/T01ABC/C01DEF  -> https://app.slack.com
    //   https://acme.slack.com/messages/...          -> https://acme.slack.com
    //   https://acme.enterprise.slack.com/...        -> https://acme.enterprise.slack.com
    origin_of(href)
}
