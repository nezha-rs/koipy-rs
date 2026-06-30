use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::cleaner::{NodeFilterStats, ProxyNode};
use crate::config::SortType;

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    Test,
    Speed,
    Analyze,
    Topo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    pub kind: TaskKind,
    pub raw_target: String,
    pub include: String,
    pub exclude: String,
    pub selected_scripts: Vec<String>,
    pub sort: Option<SortType>,
    #[serde(default)]
    pub slave_ids: Vec<String>,
    #[serde(default)]
    pub slave_id: Option<String>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub threading: Option<u64>,
    #[serde(default)]
    pub output: OutputMode,
    #[serde(default)]
    pub realtime: bool,
    #[serde(default)]
    pub nocvt: bool,
}

impl TaskRequest {
    pub fn new_url(kind: TaskKind, raw_target: String) -> Self {
        Self {
            kind,
            raw_target,
            include: String::new(),
            exclude: String::new(),
            selected_scripts: Vec::new(),
            sort: None,
            slave_ids: Vec::new(),
            slave_id: None,
            duration: None,
            threading: None,
            output: OutputMode::Image,
            realtime: false,
            nocvt: false,
        }
    }

    pub fn with_include(mut self, include: String) -> Self {
        self.include = include;
        self
    }

    pub fn with_exclude(mut self, exclude: String) -> Self {
        self.exclude = exclude;
        self
    }

    pub fn apply_command_options(mut self, command_token: &str) -> Self {
        let options = parse_command_options(command_token);
        if let Some(value) = options.get("s").or_else(|| options.get("slave")) {
            self.set_slave_ids(parse_slave_ids(value));
        }
        if let Some(sort) = options
            .get("sort")
            .and_then(|value| SortType::parse_text(value))
        {
            self.sort = Some(sort);
        }
        if let Some(value) = options.get("include") {
            self.include = value.clone();
        }
        if let Some(value) = options.get("exclude") {
            self.exclude = value.clone();
        }
        if let Some(value) = options.get("output") {
            self.output = OutputMode::parse(value);
        }
        if let Some(value) = options
            .get("duration")
            .or_else(|| options.get("d"))
            .and_then(|value| value.parse().ok())
        {
            self.duration = Some(value);
        }
        if let Some(value) = options
            .get("thread")
            .or_else(|| options.get("t"))
            .and_then(|value| value.parse().ok())
        {
            self.threading = Some(value);
        }
        if let Some(value) = options.get("realtime") {
            self.realtime = value == "true";
        }
        if let Some(value) = options.get("nocvt") {
            self.nocvt = value == "true";
        }
        self
    }

    pub fn set_slave_ids(&mut self, slave_ids: Vec<String>) {
        self.slave_ids = normalize_slave_ids(slave_ids);
        self.slave_id = self.slave_ids.first().cloned();
    }

    pub fn requested_slave_ids(&self) -> Vec<String> {
        if !self.slave_ids.is_empty() {
            return normalize_slave_ids(self.slave_ids.clone());
        }
        self.slave_id
            .as_ref()
            .map(|value| parse_slave_ids(value))
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputMode {
    #[default]
    Image,
    Json,
    Video,
}

impl OutputMode {
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "json" => Self::Json,
            "video" => Self::Video,
            _ => Self::Image,
        }
    }
}

pub fn parse_command_options(command_token: &str) -> BTreeMap<String, String> {
    let mut options = BTreeMap::new();
    let Some((_, query)) = command_token.split_once('?') else {
        return options;
    };
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            options.insert(key.to_string(), value.to_string());
        }
    }
    options
}

pub fn parse_slave_ids(value: &str) -> Vec<String> {
    value
        .split([',', '|', '+'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn normalize_slave_ids(slave_ids: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for slave_id in slave_ids {
        for item in parse_slave_ids(&slave_id) {
            if !normalized.contains(&item) {
                normalized.push(item);
            }
        }
    }
    normalized
}

#[derive(Debug, Clone)]
pub struct PreparedTask {
    pub name: String,
    pub url: String,
    pub nodes: Vec<ProxyNode>,
    pub node_count: usize,
    pub filter_stats: NodeFilterStats,
    pub available_slaves: Vec<String>,
    pub available_scripts: Vec<String>,
}

impl PreparedTask {
    pub fn summary(&self) -> String {
        format!(
            "task prepared: {}\nurl: {}\nnodes: {} -> {}\nslaves: {}\nscripts: {}",
            self.name,
            self.url,
            self.filter_stats.before,
            self.nodes.len(),
            self.available_slaves.join(", "),
            self.available_scripts.join(", "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_test_command_options() {
        let request = TaskRequest::new_url(TaskKind::Test, "https://example.com/sub".to_string())
            .apply_command_options(
                "test?s=local&sort=http&include=HK&exclude=0.1&duration=12&thread=8&output=video&realtime=true",
            );
        assert_eq!(request.slave_id.as_deref(), Some("local"));
        assert_eq!(request.slave_ids, vec!["local"]);
        assert_eq!(request.sort, Some(SortType::HttpAsc));
        assert_eq!(request.include, "HK");
        assert_eq!(request.exclude, "0.1");
        assert_eq!(request.duration, Some(12));
        assert_eq!(request.threading, Some(8));
        assert_eq!(request.output, OutputMode::Video);
        assert!(request.realtime);
    }

    #[test]
    fn parses_documented_short_command_options() {
        let request = TaskRequest::new_url(TaskKind::Speed, "https://example.com/sub".to_string())
            .apply_command_options("speed?d=15&t=6&nocvt=true");
        assert_eq!(request.duration, Some(15));
        assert_eq!(request.threading, Some(6));
        assert!(request.nocvt);
    }

    #[test]
    fn parses_multi_slave_command_options() {
        let request = TaskRequest::new_url(TaskKind::Test, "https://example.com/sub".to_string())
            .apply_command_options("test?s=local,backup|edge+local");
        assert_eq!(request.slave_id.as_deref(), Some("local"));
        assert_eq!(request.slave_ids, vec!["local", "backup", "edge"]);
        assert_eq!(
            request.requested_slave_ids(),
            vec![
                "local".to_string(),
                "backup".to_string(),
                "edge".to_string()
            ]
        );
    }
}
