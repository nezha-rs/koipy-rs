use anyhow::{Result, bail};

use crate::cleaner::{ClashConfig, parse_subscription_url, site_name};
use crate::config::{KoipyConfig, SlaveConfigEntry};
use crate::miaospeed::{
    MiaoSpeedProgress, MiaoSpeedRequest, attach_scripts, connectivity_matrices, send_with_retries,
    send_with_retries_and_progress, speed_matrices, topo_matrices,
};
use crate::result::TestResultTable;
use crate::subscription::SubscriptionCollector;
use crate::task::{PreparedTask, TaskKind, TaskRequest};

#[derive(Debug, Clone)]
pub struct KoipyApp {
    config: KoipyConfig,
}

impl KoipyApp {
    pub fn new(config: KoipyConfig) -> Self {
        Self { config }
    }

    pub async fn prepare_task(&self, request: TaskRequest) -> Result<PreparedTask> {
        let mut subcvt = self.config.subconverter.clone();
        if request.nocvt {
            subcvt.enable = false;
        }
        let url = parse_subscription_url(&request.raw_target, &subcvt).ok_or_else(|| {
            anyhow::anyhow!("could not parse a subscription URL or convertible protocol URI")
        })?;
        let collector = SubscriptionCollector::new(&self.config)?;
        let raw = collector.fetch_config(&url).await?;
        let mut clash = ClashConfig::from_slice(&raw)?;
        if let Some(dns) = &self.config.runtime.dns {
            clash.inject_dns(dns);
        }
        let filter_stats = clash.filter_nodes(&request.include, &request.exclude)?;
        if clash.proxies.is_empty() {
            bail!("no proxy nodes left after filtering");
        }
        enforce_speed_nodes_limit(clash.proxies.len(), self.config.runtime.speed_nodes)?;

        let available_slaves = self
            .config
            .visible_slaves()
            .into_iter()
            .map(|slave| slave_display_name(&self.config, slave))
            .collect();
        let available_scripts = self
            .config
            .script_config
            .scripts
            .iter()
            .map(|script| script.name.clone())
            .filter(|name| !name.is_empty())
            .collect();

        Ok(PreparedTask {
            name: site_name(&url),
            url,
            nodes: clash.proxies.clone(),
            node_count: clash.proxies.len(),
            filter_stats,
            available_slaves,
            available_scripts,
        })
    }

    pub async fn execute_task(&self, request: TaskRequest) -> Result<ExecutedTask> {
        self.execute_task_inner(request, |_| {}).await
    }

    pub async fn execute_task_with_progress<F>(
        &self,
        request: TaskRequest,
        on_progress: F,
    ) -> Result<ExecutedTask>
    where
        F: FnMut(MiaoSpeedProgress),
    {
        self.execute_task_inner(request, on_progress).await
    }

    async fn execute_task_inner<F>(
        &self,
        request: TaskRequest,
        mut on_progress: F,
    ) -> Result<ExecutedTask>
    where
        F: FnMut(MiaoSpeedProgress),
    {
        let prepared = self.prepare_task(request.clone()).await?;
        let slaves = self.select_slaves(&request.requested_slave_ids())?;
        let scripts = self.select_scripts(&request);
        let mut slave_results = Vec::new();
        let mut merged_table = TestResultTable::default();
        let mut has_table = false;
        for slave in slaves {
            let table = self
                .execute_prepared_on_slave(&prepared, slave, &scripts, &request, &mut on_progress)
                .await?;
            if has_table {
                merged_table.merge_from(table);
            } else {
                merged_table = table;
                has_table = true;
            }
            slave_results.push(slave.clone());
        }
        merged_table.sort(request.sort.unwrap_or(self.config.runtime.sort));
        Ok(ExecutedTask {
            prepared,
            table: merged_table,
            slaves: slave_results,
        })
    }

    async fn execute_prepared_on_slave<F>(
        &self,
        prepared: &PreparedTask,
        slave: &SlaveConfigEntry,
        scripts: &[crate::config::Script],
        request: &TaskRequest,
        on_progress: &mut F,
    ) -> Result<TestResultTable>
    where
        F: FnMut(MiaoSpeedProgress),
    {
        let matrices = match request.kind {
            TaskKind::Test => connectivity_matrices(&scripts),
            TaskKind::Speed => speed_matrices(),
            TaskKind::Analyze | TaskKind::Topo => topo_matrices(),
        };
        let mut ms_request = MiaoSpeedRequest::new(slave, &prepared.nodes, matrices);
        self.apply_runtime_overrides(&mut ms_request, &request);
        attach_scripts(&mut ms_request, &scripts);
        let raw = if request.realtime {
            send_with_retries_and_progress(slave, ms_request, on_progress).await?
        } else {
            send_with_retries(slave, ms_request).await?
        };
        Ok(TestResultTable::from_miaospeed(raw))
    }

    fn select_slave(&self, requested: Option<&str>) -> Result<&SlaveConfigEntry> {
        if let Some(requested) = requested.filter(|value| !value.is_empty()) {
            if let Some(slave) = self
                .config
                .slave_config
                .slaves
                .iter()
                .find(|slave| slave.id == requested || slave.comment == requested)
            {
                return Ok(slave);
            }
            bail!("requested slave not found: {requested}");
        }
        if let Some(slave) = self.config.slave_config.slaves.iter().find(|slave| {
            !slave.hidden
                && !self.config.slave_config.default.is_empty()
                && (slave.id == self.config.slave_config.default
                    || slave.comment == self.config.slave_config.default)
        }) {
            return Ok(slave);
        }
        self.config
            .slave_config
            .slaves
            .iter()
            .find(|slave| !slave.hidden)
            .ok_or_else(|| anyhow::anyhow!("no visible slave configured"))
    }

    fn select_slaves(&self, requested: &[String]) -> Result<Vec<&SlaveConfigEntry>> {
        if requested.is_empty() {
            return Ok(vec![self.select_slave(None)?]);
        }
        let mut selected = Vec::new();
        for requested_id in requested {
            let slave = self.select_slave(Some(requested_id))?;
            if !selected
                .iter()
                .any(|item: &&SlaveConfigEntry| item.id == slave.id)
            {
                selected.push(slave);
            }
        }
        Ok(selected)
    }

    fn select_scripts(&self, request: &TaskRequest) -> Vec<crate::config::Script> {
        if request.selected_scripts.is_empty() {
            return self.config.script_config.scripts.clone();
        }
        self.config
            .script_config
            .scripts
            .iter()
            .filter(|script| request.selected_scripts.contains(&script.name))
            .cloned()
            .collect()
    }

    fn apply_runtime_overrides(&self, ms_request: &mut MiaoSpeedRequest, request: &TaskRequest) {
        if let Some(duration) = request.duration {
            ms_request.configs.download_duration = duration;
        }
        if let Some(threading) = request.threading {
            ms_request.configs.download_threading = threading;
        }
        if ms_request.configs.download_url == "DYNAMIC:ALL" {
            if let Some(download_url) = self.config.runtime.speed_files.first() {
                ms_request.configs.download_url = download_url.clone();
            }
        }
        ms_request.configs.ping_address = self.config.runtime.ping_url.clone();
    }

    pub async fn serve(&self) -> Result<()> {
        let webapi_enabled = self.config.webapi.enable;
        let bot_enabled = self
            .config
            .bot
            .bot_token
            .as_deref()
            .map(|token| !token.trim().is_empty() && token != "replace-me")
            .unwrap_or(false);
        match (webapi_enabled, bot_enabled) {
            (true, true) => {
                let bot = crate::bot::BotRuntime::new(self.config.clone())?;
                tokio::try_join!(
                    crate::webapi_server::serve_webapi(self.config.clone()),
                    bot.run(),
                )?;
                Ok(())
            }
            (true, false) => crate::webapi_server::serve_webapi(self.config.clone()).await,
            (false, true) => {
                crate::bot::BotRuntime::new(self.config.clone())?
                    .run()
                    .await
            }
            (false, false) => {
                bail!("no serving transport enabled: configure bot-token or webapi.enable")
            }
        }
    }
}

fn slave_display_name(config: &KoipyConfig, slave: &SlaveConfigEntry) -> String {
    match (slave.comment.trim().is_empty(), config.slave_config.show_id) {
        (true, _) => slave.id.clone(),
        (false, true) => format!("{}({})", slave.comment, slave.id),
        (false, false) => slave.comment.clone(),
    }
}

fn enforce_speed_nodes_limit(node_count: usize, speed_nodes: usize) -> Result<()> {
    if speed_nodes > 0 && node_count > speed_nodes {
        bail!("node count {node_count} exceeds runtime.speedNodes limit {speed_nodes}");
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ExecutedTask {
    pub prepared: PreparedTask,
    pub table: TestResultTable,
    pub slaves: Vec<SlaveConfigEntry>,
}

impl ExecutedTask {
    pub fn summary(&self) -> String {
        let slave_summary = self
            .slaves
            .iter()
            .map(|slave| slave.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{}\nexecuted slaves: {}\n{}",
            self.prepared.summary(),
            slave_summary,
            self.table.summary()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleaner::ProxyNode;
    use crate::config::{MiaoSpeedOption, SlaveConfigEntry, SlaveType};
    use crate::miaospeed::speed_matrices;
    use crate::task::OutputMode;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn applies_documented_runtime_overrides() {
        let mut config = KoipyConfig::default();
        config.runtime.ping_url = "https://ping.example/generate_204".to_string();
        config.runtime.speed_files = vec!["https://speed.example/file.bin".to_string()];
        let app = KoipyApp::new(config);
        let mut slave = SlaveConfigEntry {
            id: "local".to_string(),
            comment: String::new(),
            hidden: false,
            token: "token".to_string(),
            r#type: SlaveType::MiaoSpeed,
            address: "127.0.0.1:8765".to_string(),
            path: "/".to_string(),
            proxy: None,
            skip_cert_verify: true,
            tls: false,
            invoker: None,
            buildtoken: None,
            option: MiaoSpeedOption::default(),
        };
        slave.option.download_duration = 8;
        slave.option.download_threading = 4;
        slave.option.download_url = "DYNAMIC:ALL".to_string();
        let nodes = vec![ProxyNode {
            name: "node-1".to_string(),
            kind: "ss".to_string(),
            ..Default::default()
        }];
        let mut request =
            TaskRequest::new_url(TaskKind::Speed, "https://example.com/sub".to_string());
        request.duration = Some(12);
        request.threading = Some(6);
        request.output = OutputMode::Json;
        let mut ms_request = MiaoSpeedRequest::new(&slave, &nodes, speed_matrices());

        app.apply_runtime_overrides(&mut ms_request, &request);

        assert_eq!(ms_request.configs.download_duration, 12);
        assert_eq!(ms_request.configs.download_threading, 6);
        assert_eq!(
            ms_request.configs.download_url,
            "https://speed.example/file.bin"
        );
        assert_eq!(
            ms_request.configs.ping_address,
            "https://ping.example/generate_204"
        );
    }

    #[tokio::test]
    async fn executes_task_against_local_subscription_and_miaospeed_backend() {
        let subscription_url = spawn_subscription_server().await;
        let slave_address = spawn_miaospeed_server().await;
        let mut config = KoipyConfig::default();
        config.slave_config.slaves = vec![SlaveConfigEntry {
            id: "local".to_string(),
            comment: "Local backend".to_string(),
            hidden: false,
            token: "token".to_string(),
            r#type: SlaveType::MiaoSpeed,
            address: slave_address,
            path: "/".to_string(),
            proxy: None,
            skip_cert_verify: true,
            tls: false,
            invoker: None,
            buildtoken: None,
            option: MiaoSpeedOption::default(),
        }];
        let app = KoipyApp::new(config);
        let request =
            TaskRequest::new_url(TaskKind::Test, subscription_url).with_include("Demo".to_string());

        let executed = app.execute_task(request).await.expect("execute task");

        assert_eq!(executed.prepared.node_count, 1);
        assert_eq!(executed.slaves[0].id, "local");
        assert_eq!(executed.table.rows.len(), 1);
        assert_eq!(executed.table.rows[0].node_name, "Demo Node");
        assert_eq!(executed.table.rows[0].http_latency_ms, Some(88.0));
        assert!(executed.summary().contains("executed slaves: local"));
    }

    #[test]
    fn selects_documented_default_slave_before_first_visible() {
        let mut config = KoipyConfig::default();
        config.slave_config.default = "second".to_string();
        config.slave_config.slaves = vec![
            SlaveConfigEntry {
                id: "first".to_string(),
                comment: String::new(),
                hidden: false,
                token: "token".to_string(),
                r#type: SlaveType::MiaoSpeed,
                address: "127.0.0.1:1".to_string(),
                path: "/".to_string(),
                proxy: None,
                skip_cert_verify: true,
                tls: false,
                invoker: None,
                buildtoken: None,
                option: MiaoSpeedOption::default(),
            },
            SlaveConfigEntry {
                id: "second".to_string(),
                comment: String::new(),
                hidden: false,
                token: "token".to_string(),
                r#type: SlaveType::MiaoSpeed,
                address: "127.0.0.1:2".to_string(),
                path: "/".to_string(),
                proxy: None,
                skip_cert_verify: true,
                tls: false,
                invoker: None,
                buildtoken: None,
                option: MiaoSpeedOption::default(),
            },
        ];
        let app = KoipyApp::new(config);

        let selected = app.select_slave(None).expect("default slave");

        assert_eq!(selected.id, "second");
    }

    #[test]
    fn selects_multiple_requested_slaves_in_order() {
        let mut config = KoipyConfig::default();
        config.slave_config.slaves = vec![
            SlaveConfigEntry {
                id: "local".to_string(),
                comment: String::new(),
                hidden: false,
                token: "token".to_string(),
                r#type: SlaveType::MiaoSpeed,
                address: "127.0.0.1:1".to_string(),
                path: "/".to_string(),
                proxy: None,
                skip_cert_verify: true,
                tls: false,
                invoker: None,
                buildtoken: None,
                option: MiaoSpeedOption::default(),
            },
            SlaveConfigEntry {
                id: "backup".to_string(),
                comment: "Backup".to_string(),
                hidden: true,
                token: "token".to_string(),
                r#type: SlaveType::MiaoSpeed,
                address: "127.0.0.1:2".to_string(),
                path: "/".to_string(),
                proxy: None,
                skip_cert_verify: true,
                tls: false,
                invoker: None,
                buildtoken: None,
                option: MiaoSpeedOption::default(),
            },
        ];
        let app = KoipyApp::new(config);

        let selected = app
            .select_slaves(&[
                "backup".to_string(),
                "local".to_string(),
                "backup".to_string(),
            ])
            .expect("requested slaves");

        assert_eq!(
            selected
                .iter()
                .map(|slave| slave.id.as_str())
                .collect::<Vec<_>>(),
            vec!["backup", "local"]
        );
    }

    #[test]
    fn slave_display_name_respects_show_id() {
        let mut config = KoipyConfig::default();
        let slave = SlaveConfigEntry {
            id: "local".to_string(),
            comment: "Local backend".to_string(),
            hidden: false,
            token: "token".to_string(),
            r#type: SlaveType::MiaoSpeed,
            address: "127.0.0.1:1".to_string(),
            path: "/".to_string(),
            proxy: None,
            skip_cert_verify: true,
            tls: false,
            invoker: None,
            buildtoken: None,
            option: MiaoSpeedOption::default(),
        };
        assert_eq!(slave_display_name(&config, &slave), "Local backend(local)");
        config.slave_config.show_id = false;
        assert_eq!(slave_display_name(&config, &slave), "Local backend");
    }

    #[test]
    fn speed_nodes_limit_rejects_oversized_tasks_after_filtering() {
        assert!(enforce_speed_nodes_limit(300, 300).is_ok());
        assert!(enforce_speed_nodes_limit(301, 300).is_err());
        assert!(enforce_speed_nodes_limit(301, 0).is_ok());
    }

    async fn spawn_subscription_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("subscription listener");
        let addr = listener.local_addr().expect("subscription addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("subscription accept");
            let mut buf = [0_u8; 2048];
            let _ = stream.read(&mut buf).await;
            let body = "proxies:\n  - name: Demo Node\n    type: ss\n    server: 127.0.0.1\n    port: 8388\n    cipher: aes-128-gcm\n    password: pass\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/yaml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("subscription response");
        });
        format!("http://{addr}/sub.yaml")
    }

    async fn spawn_miaospeed_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("miaospeed listener");
        let addr = listener.local_addr().expect("miaospeed addr");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("miaospeed accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("miaospeed websocket");
            let request = socket
                .next()
                .await
                .expect("miaospeed request frame")
                .expect("miaospeed request");
            let Message::Text(request_text) = request else {
                panic!("expected text request");
            };
            let request_json: serde_json::Value =
                serde_json::from_str(&request_text).expect("request json");
            assert_eq!(
                request_json
                    .get("Nodes")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len),
                Some(1)
            );
            socket
                .send(Message::Text(
                    serde_json::json!({"Progress":{"Count":1,"Stage":"TEST_PING_CONN"}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("progress send");
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "Result": {
                            "Results": [{
                                "ProxyInfo": {"Name": "Demo Node", "Type": "ss"},
                                "Matrices": [{
                                    "Type": "TEST_PING_CONN",
                                    "Payload": "{\"Value\":88}"
                                }]
                            }]
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("result send");
        });
        addr.to_string()
    }
}
