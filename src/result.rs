use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::SortType;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestResultTable {
    #[serde(default)]
    pub rows: Vec<TestResultRow>,
    #[serde(default)]
    pub inbound: Option<Value>,
    #[serde(default)]
    pub outbound: Option<Value>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestResultRow {
    pub node_name: String,
    pub node_type: String,
    pub http_latency_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub avg_speed_bytes: Option<f64>,
    pub max_speed_bytes: Option<f64>,
    pub udp_type: Option<String>,
    #[serde(default)]
    pub per_second_mb: Vec<f64>,
    #[serde(default)]
    pub script_results: Vec<(String, String)>,
}

impl TestResultTable {
    pub fn from_miaospeed(raw: Value) -> Self {
        let mut rows = Vec::new();
        if let Some(results) = raw
            .get("Result")
            .and_then(|v| v.get("Results"))
            .and_then(Value::as_array)
        {
            for item in results {
                rows.push(parse_row(item));
            }
        }
        let inbound = find_topology_payload(&raw, "GEOIP_INBOUND");
        let outbound = find_topology_payload(&raw, "GEOIP_OUTBOUND");
        Self {
            rows,
            inbound,
            outbound,
            raw,
        }
    }

    pub fn sort(&mut self, sort: SortType) {
        match sort {
            SortType::HttpAsc => self
                .rows
                .sort_by(compare_opt_f64(|row| row.http_latency_ms, false)),
            SortType::HttpDesc => self
                .rows
                .sort_by(compare_opt_f64(|row| row.http_latency_ms, true)),
            SortType::AvgSpeedAsc => self
                .rows
                .sort_by(compare_opt_f64(|row| row.avg_speed_bytes, false)),
            SortType::AvgSpeedDesc => self
                .rows
                .sort_by(compare_opt_f64(|row| row.avg_speed_bytes, true)),
            SortType::MaxSpeedAsc => self
                .rows
                .sort_by(compare_opt_f64(|row| row.max_speed_bytes, false)),
            SortType::MaxSpeedDesc => self
                .rows
                .sort_by(compare_opt_f64(|row| row.max_speed_bytes, true)),
            SortType::Origin => {}
        }
    }

    pub fn merge_from(&mut self, mut other: Self) {
        self.rows.append(&mut other.rows);
        if self.inbound.is_none() {
            self.inbound = other.inbound;
        }
        if self.outbound.is_none() {
            self.outbound = other.outbound;
        }
        self.raw = match std::mem::take(&mut self.raw) {
            Value::Array(mut values) => {
                values.push(other.raw);
                Value::Array(values)
            }
            Value::Null => other.raw,
            previous => Value::Array(vec![previous, other.raw]),
        };
    }

    pub fn summary(&self) -> String {
        if self.rows.is_empty() {
            return "No rows in result".to_string();
        }
        let ok_latency = self
            .rows
            .iter()
            .filter(|row| row.http_latency_ms.unwrap_or_default() > 0.0)
            .count();
        let udp_known = self
            .rows
            .iter()
            .filter(|row| row.udp_type.is_some())
            .count();
        let topo = if self.inbound.is_some() || self.outbound.is_some() {
            ", topology: yes"
        } else {
            ""
        };
        format!(
            "Result rows: {}, HTTP reachable: {}, UDP typed: {}{}",
            self.rows.len(),
            ok_latency,
            udp_known,
            topo,
        )
    }
}

fn parse_row(item: &Value) -> TestResultRow {
    let proxy = item.get("ProxyInfo").unwrap_or(&Value::Null);
    let mut row = TestResultRow {
        node_name: proxy
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        node_type: normalize_type(
            proxy
                .get("Type")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        ..Default::default()
    };
    if let Some(matrices) = item.get("Matrices").and_then(Value::as_array) {
        for matrix in matrices {
            let kind = matrix
                .get("Type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let payload = matrix
                .get("Payload")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or(Value::Null);
            match kind {
                "TEST_PING_CONN" => {
                    row.http_latency_ms = payload.get("Value").and_then(Value::as_f64)
                }
                "TEST_PING_RTT" => row.rtt_ms = payload.get("Value").and_then(Value::as_f64),
                "SPEED_AVERAGE" => {
                    row.avg_speed_bytes = payload.get("Value").and_then(Value::as_f64)
                }
                "SPEED_MAX" => row.max_speed_bytes = payload.get("Value").and_then(Value::as_f64),
                "SPEED_PER_SECOND" => {
                    row.per_second_mb = payload
                        .get("Speeds")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_f64)
                                .map(|v| v / 1024.0 / 1024.0)
                                .collect()
                        })
                        .unwrap_or_default();
                }
                "UDP_TYPE" => {
                    row.udp_type = payload
                        .get("Value")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                }
                "TEST_SCRIPT" => {
                    let name = payload
                        .get("Key")
                        .and_then(Value::as_str)
                        .unwrap_or("script");
                    let text = payload.get("Text").and_then(Value::as_str).unwrap_or("N/A");
                    row.script_results
                        .push((name.to_string(), text.to_string()));
                }
                _ => {}
            }
        }
    }
    row
}

fn find_topology_payload(raw: &Value, kind: &str) -> Option<Value> {
    raw.get("Result")
        .and_then(|v| v.get("Results"))
        .and_then(Value::as_array)
        .and_then(|results| {
            results.iter().find_map(|item| {
                item.get("Matrices")
                    .and_then(Value::as_array)
                    .and_then(|matrices| {
                        matrices.iter().find_map(|matrix| {
                            if matrix.get("Type").and_then(Value::as_str) == Some(kind) {
                                matrix
                                    .get("Payload")
                                    .and_then(Value::as_str)
                                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                            } else {
                                None
                            }
                        })
                    })
            })
        })
}

fn normalize_type(kind: &str) -> String {
    match kind.to_ascii_lowercase().as_str() {
        "ss" => "Shadowsocks".to_string(),
        "ssr" => "ShadowsocksR".to_string(),
        "tuic" => "TUIC".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn compare_opt_f64<F>(
    get: F,
    reverse: bool,
) -> impl FnMut(&TestResultRow, &TestResultRow) -> std::cmp::Ordering
where
    F: Fn(&TestResultRow) -> Option<f64> + Copy,
{
    move |a, b| {
        let left = get(a).unwrap_or(if reverse { f64::MIN } else { f64::MAX });
        let right = get(b).unwrap_or(if reverse { f64::MIN } else { f64::MAX });
        let ordering = left
            .partial_cmp(&right)
            .unwrap_or(std::cmp::Ordering::Equal);
        if reverse {
            ordering.reverse()
        } else {
            ordering
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_miaospeed_row() {
        let raw = serde_json::json!({
            "Result": {
                "Results": [{
                    "ProxyInfo": {"Name": "node-1", "Type": "ss"},
                    "Matrices": [{
                        "Type": "TEST_PING_CONN",
                        "Payload": "{\"Value\":123}"
                    }, {
                        "Type": "UDP_TYPE",
                        "Payload": "{\"Value\":\"Full Cone\"}"
                    }, {
                        "Type": "SPEED_PER_SECOND",
                        "Payload": "{\"Speeds\":[1048576,2097152]}"
                    }]
                }]
            }
        });
        let table = TestResultTable::from_miaospeed(raw);
        assert_eq!(table.rows[0].node_type, "Shadowsocks");
        assert_eq!(table.rows[0].http_latency_ms, Some(123.0));
        assert_eq!(table.rows[0].udp_type.as_deref(), Some("Full Cone"));
        assert_eq!(table.rows[0].per_second_mb, vec![1.0, 2.0]);
    }

    #[test]
    fn parses_topology_payloads() {
        let raw = serde_json::json!({
            "Result": {
                "Results": [{
                    "ProxyInfo": {"Name": "node-1", "Type": "tuic"},
                    "Matrices": [{
                        "Type": "GEOIP_INBOUND",
                        "Payload": "{\"Country\":\"US\",\"IP\":\"1.1.1.1\"}"
                    }, {
                        "Type": "GEOIP_OUTBOUND",
                        "Payload": "{\"Country\":\"JP\",\"IP\":\"2.2.2.2\"}"
                    }]
                }]
            }
        });
        let table = TestResultTable::from_miaospeed(raw);
        assert_eq!(table.rows[0].node_type, "TUIC");
        assert_eq!(
            table
                .inbound
                .as_ref()
                .and_then(|v| v.get("Country"))
                .and_then(Value::as_str),
            Some("US")
        );
        assert_eq!(
            table
                .outbound
                .as_ref()
                .and_then(|v| v.get("Country"))
                .and_then(Value::as_str),
            Some("JP")
        );
        assert!(table.summary().contains("topology: yes"));
    }

    #[test]
    fn merges_rows_and_keeps_first_topology_payloads() {
        let mut first = TestResultTable {
            rows: vec![TestResultRow {
                node_name: "a".to_string(),
                ..Default::default()
            }],
            inbound: Some(serde_json::json!({"Country":"US"})),
            raw: serde_json::json!({"first":true}),
            ..Default::default()
        };
        let second = TestResultTable {
            rows: vec![TestResultRow {
                node_name: "b".to_string(),
                ..Default::default()
            }],
            inbound: Some(serde_json::json!({"Country":"JP"})),
            outbound: Some(serde_json::json!({"Country":"SG"})),
            raw: serde_json::json!({"second":true}),
        };

        first.merge_from(second);

        assert_eq!(first.rows.len(), 2);
        assert_eq!(first.rows[1].node_name, "b");
        assert_eq!(
            first
                .inbound
                .as_ref()
                .and_then(|v| v.get("Country"))
                .and_then(Value::as_str),
            Some("US")
        );
        assert_eq!(
            first
                .outbound
                .as_ref()
                .and_then(|v| v.get("Country"))
                .and_then(Value::as_str),
            Some("SG")
        );
        assert!(first.raw.is_array());
    }
}
