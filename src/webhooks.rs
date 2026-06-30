use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::Value;

use crate::config::KoipyConfig;

#[derive(Debug, Clone, Copy)]
pub enum WebhookEvent {
    OnMessage,
    OnPreSend,
    OnResult,
}

#[derive(Debug, Clone)]
pub struct WebhookClient {
    config: KoipyConfig,
    client: Client,
}

impl WebhookClient {
    pub fn new(config: KoipyConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    pub async fn emit(&self, event: WebhookEvent, payload: Value) -> Result<WebhookOutcome> {
        if !self.config.webapi.enable {
            return Ok(WebhookOutcome::default());
        }
        let Some(url) = self.url_for(event) else {
            return Ok(WebhookOutcome::default());
        };
        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("webhook request failed")?;
        let status = response.status();
        if status.as_u16() > 400 {
            bail!("webhook rejected event {:?} with HTTP {}", event, status);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if content_type.contains("application/json") {
            let json = response
                .json::<Value>()
                .await
                .context("webhook JSON decode failed")?;
            Ok(WebhookOutcome {
                append_text: None,
                merge_json: Some(json),
            })
        } else if content_type.starts_with("text/") {
            let text = response
                .text()
                .await
                .context("webhook text decode failed")?;
            Ok(WebhookOutcome {
                append_text: Some(text),
                merge_json: None,
            })
        } else {
            Ok(WebhookOutcome::default())
        }
    }

    fn url_for(&self, event: WebhookEvent) -> Option<&str> {
        match event {
            WebhookEvent::OnMessage => self.config.webapi.on_message.as_deref().or(self
                .config
                .callbacks
                .on_message
                .as_deref()),
            WebhookEvent::OnPreSend => self.config.webapi.on_pre_send.as_deref().or(self
                .config
                .callbacks
                .on_pre_send
                .as_deref()),
            WebhookEvent::OnResult => self.config.webapi.on_result.as_deref().or(self
                .config
                .callbacks
                .on_result
                .as_deref()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WebhookOutcome {
    pub append_text: Option<String>,
    pub merge_json: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_top_level_is_used_as_webhook_fallback() {
        let mut config = KoipyConfig::default();
        config.callbacks.on_message = Some("https://hooks.example/message".to_string());
        let client = WebhookClient::new(config);

        assert_eq!(
            client.url_for(WebhookEvent::OnMessage),
            Some("https://hooks.example/message")
        );
    }

    #[test]
    fn webapi_webhook_url_takes_precedence() {
        let mut config = KoipyConfig::default();
        config.webapi.on_result = Some("https://hooks.example/webapi".to_string());
        config.callbacks.on_result = Some("https://hooks.example/callbacks".to_string());
        let client = WebhookClient::new(config);

        assert_eq!(
            client.url_for(WebhookEvent::OnResult),
            Some("https://hooks.example/webapi")
        );
    }
}
