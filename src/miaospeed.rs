use anyhow::{Context, Result, bail};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::sync::{Arc, Once};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async, connect_async_tls_with_config};

use crate::cleaner::ProxyNode;
use crate::config::{MiaoSpeedOption, Script, SlaveConfigEntry};

const DEFAULT_BUILD_TOKEN: &str = "MIAOKO4|580JxAo049R|GEnERAl|1X571R930|T0kEN";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MiaoSpeedRequest {
    pub challenge: String,
    pub vendor: String,
    pub basics: RequestBasics,
    pub configs: RequestConfigs,
    pub options: RequestOptions,
    pub nodes: Vec<RequestNode>,
}

impl MiaoSpeedRequest {
    pub fn new(slave: &SlaveConfigEntry, nodes: &[ProxyNode], matrices: Vec<MatrixEntry>) -> Self {
        Self {
            challenge: String::new(),
            vendor: String::new(),
            basics: RequestBasics {
                id: "114514".to_string(),
                slave: slave.id.clone(),
                slave_name: slave.comment.clone(),
                invoker: slave.invoker.clone().unwrap_or_default(),
                version: "1.0-rs".to_string(),
            },
            configs: RequestConfigs::from(&slave.option),
            options: RequestOptions { matrices },
            nodes: nodes
                .iter()
                .enumerate()
                .map(|(index, node)| RequestNode {
                    name: index.to_string(),
                    payload: serde_yaml::to_string(node).unwrap_or_default(),
                })
                .collect(),
        }
    }

    pub fn sign(&mut self, start_token: &str, build_token: Option<&str>) -> Result<()> {
        let mut unsigned = self.clone();
        unsigned.challenge.clear();
        unsigned.vendor.clear();
        let body = serde_json::to_string(&unsigned)?;
        self.challenge = sign_payload(
            start_token,
            build_token.unwrap_or(DEFAULT_BUILD_TOKEN),
            &body,
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RequestBasics {
    pub id: String,
    pub slave: String,
    pub slave_name: String,
    pub invoker: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RequestConfigs {
    pub download_duration: u64,
    pub download_threading: u64,
    pub download_url: String,
    pub ping_address: String,
    pub ping_average_over: u64,
    pub stun_url: String,
    pub task_retry: u64,
    pub task_timeout: u64,
    #[serde(rename = "DNSServer")]
    pub dns_server: Vec<String>,
    #[serde(rename = "ApiVersion")]
    pub api_version: u64,
    #[serde(rename = "UploadURL")]
    pub upload_url: String,
    pub upload_duration: u64,
    pub upload_threading: u64,
    #[serde(default)]
    pub scripts: Vec<RequestScript>,
}

impl From<&MiaoSpeedOption> for RequestConfigs {
    fn from(value: &MiaoSpeedOption) -> Self {
        Self {
            download_duration: value.download_duration,
            download_threading: value.download_threading,
            download_url: value.download_url.clone(),
            ping_address: value.ping_address.clone(),
            ping_average_over: value.ping_average_over,
            stun_url: value.stun_url.clone(),
            task_retry: value.task_retry,
            task_timeout: value.task_timeout,
            dns_server: value.dns_server.clone(),
            api_version: value.api_version,
            upload_url: value.upload_url.clone(),
            upload_duration: value.upload_duration,
            upload_threading: value.upload_threading,
            scripts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RequestOptions {
    pub matrices: Vec<MatrixEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RequestNode {
    pub name: String,
    pub payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RequestScript {
    pub id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MatrixEntry {
    #[serde(rename = "Type")]
    pub kind: MatrixType,
    pub params: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MatrixType {
    #[serde(rename = "TEST_PING_RTT")]
    TestPingRtt,
    #[serde(rename = "TEST_PING_CONN")]
    TestPingConn,
    #[serde(rename = "TEST_SCRIPT")]
    TestScript,
    #[serde(rename = "SPEED_AVERAGE")]
    SpeedAverage,
    #[serde(rename = "SPEED_MAX")]
    SpeedMax,
    #[serde(rename = "SPEED_PER_SECOND")]
    SpeedPerSecond,
    #[serde(rename = "UDP_TYPE")]
    UdpType,
    #[serde(rename = "GEOIP_INBOUND")]
    GeoipInbound,
    #[serde(rename = "GEOIP_OUTBOUND")]
    GeoipOutbound,
}

pub fn connectivity_matrices(scripts: &[Script]) -> Vec<MatrixEntry> {
    let mut matrices = vec![
        MatrixEntry {
            kind: MatrixType::TestPingRtt,
            params: String::new(),
        },
        MatrixEntry {
            kind: MatrixType::TestPingConn,
            params: String::new(),
        },
        MatrixEntry {
            kind: MatrixType::UdpType,
            params: "0".to_string(),
        },
    ];
    matrices.extend(scripts.iter().map(|script| MatrixEntry {
        kind: MatrixType::TestScript,
        params: script.name.clone(),
    }));
    matrices
}

pub fn speed_matrices() -> Vec<MatrixEntry> {
    vec![
        MatrixEntry {
            kind: MatrixType::TestPingRtt,
            params: String::new(),
        },
        MatrixEntry {
            kind: MatrixType::TestPingConn,
            params: String::new(),
        },
        MatrixEntry {
            kind: MatrixType::SpeedAverage,
            params: "0".to_string(),
        },
        MatrixEntry {
            kind: MatrixType::SpeedMax,
            params: "0".to_string(),
        },
        MatrixEntry {
            kind: MatrixType::SpeedPerSecond,
            params: "0".to_string(),
        },
        MatrixEntry {
            kind: MatrixType::UdpType,
            params: "0".to_string(),
        },
    ]
}

pub fn topo_matrices() -> Vec<MatrixEntry> {
    vec![
        MatrixEntry {
            kind: MatrixType::GeoipInbound,
            params: String::new(),
        },
        MatrixEntry {
            kind: MatrixType::GeoipOutbound,
            params: String::new(),
        },
    ]
}

pub fn attach_scripts(request: &mut MiaoSpeedRequest, scripts: &[Script]) {
    request
        .configs
        .scripts
        .extend(scripts.iter().map(|script| RequestScript {
            id: script.name.clone(),
            content: resolve_script_content(&script.content),
        }));
}

fn resolve_script_content(content: &str) -> String {
    let path = content.trim();
    if path.is_empty() {
        return String::new();
    }
    if looks_like_inline_script(path) {
        return content.to_string();
    }
    std::fs::read_to_string(path).unwrap_or_else(|_| content.to_string())
}

fn looks_like_inline_script(content: &str) -> bool {
    content.contains('\n')
        || content.contains("function ")
        || content.contains("=>")
        || content.contains("const ")
        || content.contains("let ")
        || content.contains("var ")
        || content.contains("return ")
}

pub fn sign_payload(start_token: &str, build_token: &str, request: &str) -> String {
    let mut hasher = Sha512::new();
    hasher.update(request.as_bytes());
    for token in std::iter::once(start_token).chain(build_token.split('|')) {
        let token = if token.is_empty() {
            "SOME_TOKEN"
        } else {
            token
        };
        let previous_digest = hasher.clone().finalize();
        let mut copy = hasher.clone();
        copy.update(token.as_bytes());
        copy.update(previous_digest);
        hasher = copy;
    }
    base64::engine::general_purpose::URL_SAFE.encode(hasher.finalize())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MiaoSpeedProgress {
    pub count: usize,
    pub total: usize,
    pub queuing: usize,
    pub stage: Option<String>,
}

impl MiaoSpeedProgress {
    pub fn from_value(value: &serde_json::Value, count: usize, total: usize) -> Self {
        let progress = value.get("Progress").unwrap_or(value);
        Self {
            count,
            total,
            queuing: progress
                .get("Queuing")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default() as usize,
            stage: progress
                .get("Stage")
                .or_else(|| progress.get("Type"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
        }
    }

    pub fn percent(&self) -> u8 {
        if self.total == 0 {
            0
        } else {
            ((self.count.min(self.total) as f64 / self.total as f64) * 100.0).round() as u8
        }
    }

    pub fn should_emit(&self, last_count: usize) -> bool {
        self.count == 1 || self.count >= self.total || self.count.saturating_sub(last_count) >= 4
    }

    pub fn render_text(
        &self,
        slave_name: &str,
        label_slave: &str,
        label_queue: &str,
        label_progress: &str,
    ) -> String {
        let slave_label = if slave_name.trim().is_empty() {
            "Local"
        } else {
            slave_name.trim()
        };
        let percent = self.percent();
        let filled = ((percent as usize) / 5).min(20);
        let progress_bar = format!("[{}{}]", "=".repeat(filled), " ".repeat(20 - filled));
        let progress_value = if self.total == 0 {
            "0.00".to_string()
        } else {
            format!(
                "{:.2}",
                (self.count.min(self.total) as f64 / self.total as f64) * 100.0
            )
        };
        let mut out = String::new();
        out.push_str(&format!("{label_slave}{slave_label}\n"));
        if let Some(stage) = self.stage.as_deref() {
            out.push_str(&format!("{stage}\n"));
        } else {
            out.push('\n');
        }
        if self.queuing > 0 {
            out.push_str(&format!("{label_queue} `{}`\n\n", self.queuing));
        } else {
            out.push('\n');
        }
        out.push_str(&format!("{progress_bar}\n\n"));
        out.push_str(label_progress);
        out.push('\n');
        out.push_str(&format!(
            "{progress_value}%     [{}/{}]",
            self.count, self.total
        ));
        out
    }
}

pub async fn send_once(
    slave: &SlaveConfigEntry,
    request: MiaoSpeedRequest,
) -> Result<serde_json::Value> {
    send_once_with_progress(slave, request, |_| {}).await
}

pub async fn send_once_with_progress<F>(
    slave: &SlaveConfigEntry,
    mut request: MiaoSpeedRequest,
    mut on_progress: F,
) -> Result<serde_json::Value>
where
    F: FnMut(MiaoSpeedProgress),
{
    request.sign(&slave.token, slave.buildtoken.as_deref())?;
    let total = request.nodes.len();
    let url = slave_ws_url(slave);
    let (mut socket, _) = connect_miaospeed(slave, &url)
        .await
        .with_context(|| format!("failed to connect miaospeed slave {}", slave.address))?;
    socket
        .send(Message::Text(serde_json::to_string(&request)?.into()))
        .await?;
    while let Some(message) = socket.next().await {
        let message = message?;
        if let Message::Text(text) = message {
            let value: serde_json::Value = serde_json::from_str(&text)?;
            if value.get("Progress").is_some() {
                let count = progress_count(&value).unwrap_or_else(|| total.min(1));
                on_progress(MiaoSpeedProgress::from_value(&value, count, total));
                continue;
            }
            if value.get("Result").is_some() || value.get("Error").is_some() {
                if let Some(error) = value.get("Error").filter(|error| !error.is_null()) {
                    bail!("miaospeed slave returned error: {error}");
                }
                return Ok(value);
            }
        }
    }
    Ok(serde_json::json!({}))
}

pub async fn send_with_retries(
    slave: &SlaveConfigEntry,
    request: MiaoSpeedRequest,
) -> Result<serde_json::Value> {
    send_with_retries_and_progress(slave, request, |_| {}).await
}

pub async fn send_with_retries_and_progress<F>(
    slave: &SlaveConfigEntry,
    request: MiaoSpeedRequest,
    mut on_progress: F,
) -> Result<serde_json::Value>
where
    F: FnMut(MiaoSpeedProgress),
{
    let attempts = retry_attempts(slave.option.task_retry);
    let mut last_error = None;
    for attempt in 1..=attempts {
        match send_once_with_progress(slave, request.clone(), &mut on_progress).await {
            Ok(value) => return Ok(value),
            Err(err) => {
                tracing::warn!(
                    slave = %slave.id,
                    attempt,
                    attempts,
                    "MiaoSpeed request failed: {err:#}"
                );
                last_error = Some(err);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("miaospeed request did not run")))
}

fn progress_count(value: &serde_json::Value) -> Option<usize> {
    let progress = value.get("Progress")?;
    for key in ["Count", "Current", "Index", "Done", "Finished"] {
        if let Some(count) = progress.get(key).and_then(serde_json::Value::as_u64) {
            return Some(count as usize);
        }
    }
    progress
        .get("Results")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.len())
}

fn retry_attempts(task_retry: u64) -> u64 {
    task_retry.max(1)
}

pub async fn ping_slave(slave: &SlaveConfigEntry) -> bool {
    matches!(
        timeout(
            Duration::from_secs(5),
            connect_miaospeed(slave, &slave_ws_url(slave))
        )
        .await,
        Ok(Ok(_))
    )
}

async fn connect_miaospeed(
    slave: &SlaveConfigEntry,
    url: &str,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    if let Some(proxy) = slave
        .proxy
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return connect_miaospeed_via_http_proxy(slave, url, proxy).await;
    }
    if slave.tls && slave.skip_cert_verify {
        connect_async_tls_with_config(url, None, false, miaospeed_tls_connector(slave)).await
    } else {
        connect_async(url).await
    }
}

async fn connect_miaospeed_via_http_proxy(
    slave: &SlaveConfigEntry,
    url: &str,
    proxy: &str,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    let proxy = parse_http_proxy(proxy)?;
    let mut stream = TcpStream::connect(proxy.address())
        .await
        .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
    let target = slave.address.trim();
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = proxy.basic_auth_header() {
        request.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
    let response = read_proxy_connect_response(&mut stream)
        .await
        .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
    if !proxy_connect_succeeded(&response) {
        return Err(tokio_tungstenite::tungstenite::Error::Http(
            http::Response::builder()
                .status(proxy_status_code(&response).unwrap_or(502))
                .body(None)
                .expect("proxy CONNECT response"),
        ));
    }
    tokio_tungstenite::client_async_tls_with_config(
        url,
        stream,
        None,
        miaospeed_tls_connector(slave),
    )
    .await
}

fn miaospeed_tls_connector(slave: &SlaveConfigEntry) -> Option<Connector> {
    if slave.tls && slave.skip_cert_verify {
        Some(Connector::Rustls(no_verify_tls_config()))
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpProxyConfig {
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

impl HttpProxyConfig {
    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn basic_auth_header(&self) -> Option<String> {
        let username = self.username.as_ref()?;
        let password = self.password.as_deref().unwrap_or_default();
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        Some(format!("Basic {encoded}"))
    }
}

fn parse_http_proxy(value: &str) -> Result<HttpProxyConfig, tokio_tungstenite::tungstenite::Error> {
    let url = url::Url::parse(value).map_err(|err| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid slave proxy URL: {err}"),
        ))
    })?;
    if url.scheme() != "http" {
        return Err(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unsupported slave proxy scheme: {}", url.scheme()),
            ),
        ));
    }
    let host = url.host_str().ok_or_else(|| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "slave proxy URL is missing host",
        ))
    })?;
    Ok(HttpProxyConfig {
        host: host.to_string(),
        port: url.port_or_known_default().unwrap_or(80),
        username: if url.username().is_empty() {
            None
        } else {
            Some(url.username().to_string())
        },
        password: url.password().map(ToString::to_string),
    })
}

async fn read_proxy_connect_response(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0_u8; 1];
    while buf.len() < 8192 {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn proxy_connect_succeeded(response: &str) -> bool {
    matches!(proxy_status_code(response), Some(200..=299))
}

fn proxy_status_code(response: &str) -> Option<u16> {
    response
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn no_verify_tls_config() -> Arc<ClientConfig> {
    install_rustls_crypto_provider();
    Arc::new(
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth(),
    )
}

fn install_rustls_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

fn slave_ws_url(slave: &SlaveConfigEntry) -> String {
    let scheme = if slave.tls { "wss" } else { "ws" };
    let address = slave.address.trim();
    let path = normalize_slave_path(&slave.path);
    format!("{scheme}://{address}{path}")
}

fn normalize_slave_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable() {
        let one = sign_payload("token", DEFAULT_BUILD_TOKEN, "{\"a\":1}");
        let two = sign_payload("token", DEFAULT_BUILD_TOKEN, "{\"a\":1}");
        assert_eq!(one, two);
    }

    #[test]
    fn task_retry_is_at_least_one_attempt() {
        assert_eq!(retry_attempts(0), 1);
        assert_eq!(retry_attempts(3), 3);
    }

    #[test]
    fn request_configs_include_closed_binary_backend_options() {
        let mut option = crate::config::MiaoSpeedOption::default();
        option.task_timeout = 5000;
        option.dns_server = vec!["119.29.29.29:53".to_string()];
        option.api_version = 3;
        option.upload_url = "https://speed.cloudflare.com/__up".to_string();
        option.upload_duration = 9;
        option.upload_threading = 5;

        let configs = RequestConfigs::from(&option);
        let json = serde_json::to_value(configs).expect("configs json");

        assert_eq!(json["TaskTimeout"], 5000);
        assert_eq!(json["DNSServer"][0], "119.29.29.29:53");
        assert_eq!(json["ApiVersion"], 3);
        assert_eq!(json["UploadURL"], "https://speed.cloudflare.com/__up");
        assert_eq!(json["UploadDuration"], 9);
        assert_eq!(json["UploadThreading"], 5);
    }

    #[test]
    fn attach_scripts_loads_file_content_when_content_is_path() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-script-{}-netflix.js", std::process::id()));
        std::fs::write(&path, "const ok = true;").expect("write script");
        let slave = crate::config::SlaveConfigEntry {
            id: "local".to_string(),
            comment: String::new(),
            hidden: false,
            token: "token".to_string(),
            r#type: crate::config::SlaveType::MiaoSpeed,
            address: "127.0.0.1:8765".to_string(),
            path: "/".to_string(),
            proxy: None,
            skip_cert_verify: false,
            tls: false,
            invoker: None,
            buildtoken: None,
            option: crate::config::MiaoSpeedOption::default(),
        };
        let mut request = MiaoSpeedRequest::new(&slave, &[], Vec::new());
        let scripts = vec![
            crate::config::Script {
                name: "Netflix".to_string(),
                content: path.to_string_lossy().to_string(),
                ..Default::default()
            },
            crate::config::Script {
                name: "Inline".to_string(),
                content: "const inline = true;".to_string(),
                ..Default::default()
            },
        ];

        attach_scripts(&mut request, &scripts);

        assert_eq!(request.configs.scripts[0].id, "Netflix");
        assert_eq!(request.configs.scripts[0].content, "const ok = true;");
        assert_eq!(request.configs.scripts[1].content, "const inline = true;");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slave_websocket_url_uses_documented_path() {
        let mut slave = crate::config::SlaveConfigEntry {
            id: "local".to_string(),
            comment: String::new(),
            hidden: false,
            token: "token".to_string(),
            r#type: crate::config::SlaveType::MiaoSpeed,
            address: "127.0.0.1:8765".to_string(),
            path: "miaospeed".to_string(),
            proxy: None,
            skip_cert_verify: false,
            tls: false,
            invoker: None,
            buildtoken: None,
            option: crate::config::MiaoSpeedOption::default(),
        };
        assert_eq!(slave_ws_url(&slave), "ws://127.0.0.1:8765/miaospeed");
        slave.tls = true;
        slave.path = "/koipy/ws".to_string();
        assert_eq!(slave_ws_url(&slave), "wss://127.0.0.1:8765/koipy/ws");
        slave.path = String::new();
        assert_eq!(slave_ws_url(&slave), "wss://127.0.0.1:8765/");
    }

    #[test]
    fn skip_cert_verify_selects_rustls_no_verify_connector() {
        let mut slave = crate::config::SlaveConfigEntry {
            id: "local".to_string(),
            comment: String::new(),
            hidden: false,
            token: "token".to_string(),
            r#type: crate::config::SlaveType::MiaoSpeed,
            address: "127.0.0.1:8765".to_string(),
            path: "/".to_string(),
            proxy: None,
            skip_cert_verify: true,
            tls: true,
            invoker: None,
            buildtoken: None,
            option: crate::config::MiaoSpeedOption::default(),
        };
        assert!(matches!(
            miaospeed_tls_connector(&slave),
            Some(Connector::Rustls(_))
        ));
        slave.skip_cert_verify = false;
        assert!(miaospeed_tls_connector(&slave).is_none());
        slave.skip_cert_verify = true;
        slave.tls = false;
        assert!(miaospeed_tls_connector(&slave).is_none());
    }

    #[test]
    fn parses_documented_slave_http_proxy() {
        let proxy =
            parse_http_proxy("http://user:pass@proxy.example.com:7890").expect("proxy config");
        assert_eq!(proxy.address(), "proxy.example.com:7890");
        assert_eq!(
            proxy.basic_auth_header().as_deref(),
            Some("Basic dXNlcjpwYXNz")
        );
        let default_port = parse_http_proxy("http://proxy.example.com").expect("default port");
        assert_eq!(default_port.address(), "proxy.example.com:80");
        assert!(parse_http_proxy("socks5://proxy.example.com:1080").is_err());
    }

    #[test]
    fn parses_proxy_connect_status() {
        assert!(proxy_connect_succeeded(
            "HTTP/1.1 200 Connection Established\r\n\r\n"
        ));
        assert_eq!(
            proxy_status_code("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n"),
            Some(407)
        );
        assert!(!proxy_connect_succeeded("HTTP/1.1 502 Bad Gateway\r\n\r\n"));
    }

    #[test]
    fn parses_progress_and_throttles_emits() {
        let value = serde_json::json!({
            "Progress": {
                "Count": 4,
                "Queuing": 2,
                "Stage": "speed"
            }
        });
        let progress = MiaoSpeedProgress::from_value(&value, progress_count(&value).unwrap(), 10);
        assert_eq!(progress.count, 4);
        assert_eq!(progress.queuing, 2);
        assert_eq!(progress.percent(), 40);
        assert!(progress.should_emit(0));
        assert!(!progress.should_emit(2));
        let rendered = progress.render_text("local", "Slave: ", "Queue size: ", "Progress: ");
        assert!(rendered.contains("Slave: local"));
        assert!(rendered.contains("[========            ]"));
        assert!(rendered.contains("Progress:"));
    }
}
