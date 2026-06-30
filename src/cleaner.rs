use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::BTreeMap;
use url::Url;

use crate::config::SubconverterConfig;
use crate::config::RuntimeDnsConfig;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClashConfig {
    #[serde(default)]
    pub proxies: Vec<ProxyNode>,
    #[serde(flatten)]
    pub extra: serde_yaml::Mapping,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProxyNode {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(flatten)]
    pub payload: serde_yaml::Mapping,
}

impl ClashConfig {
    pub fn from_slice(data: &[u8]) -> Result<Self> {
        let value: Value =
            serde_yaml::from_slice(data).context("subscription is not valid YAML")?;
        let mut cfg: Self =
            serde_yaml::from_value(value).context("subscription is not Clash-compatible YAML")?;
        cfg.proxies
            .retain(|node| !node.name.is_empty() && node.name.len() < 128);
        Ok(cfg)
    }

    pub fn inject_dns(&mut self, dns: &RuntimeDnsConfig) {
        if dns.enable && !dns.nameserver.is_empty() {
            self.extra.insert(
                Value::String("dns".to_string()),
                serde_yaml::to_value(dns).unwrap_or(Value::Null),
            );
        }
    }

    pub fn filter_nodes(&mut self, include: &str, exclude: &str) -> Result<NodeFilterStats> {
        let before = self.proxies.len();
        let include_re = compile_optional(include)?;
        let exclude_re = compile_optional(exclude)?;
        self.proxies.retain(|node| {
            let included = include_re
                .as_ref()
                .map(|re| re.is_match(&node.name))
                .unwrap_or(true);
            let excluded = exclude_re
                .as_ref()
                .map(|re| re.is_match(&node.name))
                .unwrap_or(false);
            included && !excluded
        });
        Ok(NodeFilterStats {
            before,
            after: self.proxies.len(),
            include: include.to_string(),
            exclude: exclude.to_string(),
        })
    }
}

fn compile_optional(pattern: &str) -> Result<Option<Regex>> {
    if pattern.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(
            Regex::new(pattern).context("invalid node filter regex")?,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct NodeFilterStats {
    pub before: usize,
    pub after: usize,
    pub include: String,
    pub exclude: String,
}

pub fn parse_subscription_url(input: &str, subconverter: &SubconverterConfig) -> Option<String> {
    let http_re =
        Regex::new(r#"https?://(?:[A-Za-z0-9]|[$\-_.+!*'(),]|%[0-9A-Fa-f]{2}|[/?#@=&:])+"#).ok()?;
    if let Some(found) = http_re.find(input) {
        let url = found.as_str();
        if should_rewrite_http_subscription(url, subconverter) {
            return render_subconverter_template(url, subconverter);
        }
        return Some(url.to_string());
    }
    protocol_join(input.trim(), subconverter)
}

pub fn protocol_join(link: &str, subconverter: &SubconverterConfig) -> Option<String> {
    let accepted = [
        "vmess",
        "vless",
        "ss",
        "ssr",
        "trojan",
        "hysteria2",
        "hysteria",
        "socks5",
        "snell",
        "tuic",
        "juicity",
    ];
    let scheme = link.split("://").next()?;
    if !accepted.contains(&scheme) || !subconverter.enable {
        return None;
    }

    render_subconverter_template(link, subconverter)
}

fn should_rewrite_http_subscription(url: &str, subconverter: &SubconverterConfig) -> bool {
    if !subconverter.enable || is_builtin_mode(subconverter) {
        return false;
    }
    Url::parse(url)
        .ok()
        .map(|parsed| {
            !parsed
                .query_pairs()
                .any(|(key, _)| key.eq_ignore_ascii_case("target"))
        })
        .unwrap_or(false)
}

fn render_subconverter_template(link: &str, subconverter: &SubconverterConfig) -> Option<String> {
    if !subconverter.enable {
        return None;
    }
    let template = backend_template(subconverter);
    if template.trim().is_empty() {
        return None;
    }
    let values = template_values(link, subconverter, &template);
    let mut rendered = template;
    for (key, value) in values {
        rendered = rendered.replace(&format!("${key}"), &value);
    }
    Some(rendered)
}

fn backend_template(subconverter: &SubconverterConfig) -> String {
    if !subconverter.template.backend.trim().is_empty() {
        return subconverter.template.backend.clone();
    }
    let scheme = if subconverter.tls { "https" } else { "http" };
    format!(
        "{scheme}://{}/sub?target=$Target&new_name=true&url=$EncodedURL&insert=false&config=$EncodedConfig",
        subconverter.address
    )
}

fn template_values(
    link: &str,
    subconverter: &SubconverterConfig,
    template: &str,
) -> BTreeMap<String, String> {
    let mode = normalized_mode(subconverter);
    let substore_style = mode == "substore" || template.contains("/download/sub");
    let host = default_value(subconverter, "host").unwrap_or_else(|| {
        host_from_address(&subconverter.address).unwrap_or("127.0.0.1".to_string())
    });
    let port = default_value(subconverter, "port").unwrap_or_else(|| {
        if substore_style {
            "3000".to_string()
        } else {
            port_from_address(&subconverter.address).unwrap_or_else(|| "25500".to_string())
        }
    });
    let scheme = default_value(subconverter, "scheme").unwrap_or_else(|| {
        Url::parse(template)
            .ok()
            .map(|url| url.scheme().to_string())
            .unwrap_or_else(|| if subconverter.tls { "https" } else { "http" }.to_string())
    });
    let target = default_value(subconverter, "target").unwrap_or_else(|| {
        if substore_style || mode == "builtin" {
            "ClashMeta".to_string()
        } else {
            "clash".to_string()
        }
    });
    let config = subconverter.remote_config.clone().unwrap_or_else(|| {
        "https://raw.githubusercontent.com/ACL4SSR/ACL4SSR/master/Clash/config/ACL4SSR_Online.ini".to_string()
    });

    let mut values = BTreeMap::new();
    insert_placeholder(&mut values, "Mode", &mode);
    insert_placeholder(&mut values, "Scheme", &scheme);
    insert_placeholder(&mut values, "Host", &host);
    insert_placeholder(&mut values, "Port", &port);
    insert_placeholder(&mut values, "Target", &target);
    insert_placeholder(&mut values, "URL", link);
    insert_placeholder(&mut values, "Content", link);
    insert_placeholder(&mut values, "Config", &config);

    for (key, value) in &subconverter.defaults {
        if let Some(value) = yaml_scalar_to_string(value) {
            insert_placeholder(&mut values, &placeholder_name(key), &value);
        }
    }
    values
}

fn insert_placeholder(values: &mut BTreeMap<String, String>, name: &str, value: &str) {
    let encoded = urlencoding(value);
    for variant in placeholder_variants(name) {
        values.insert(variant.clone(), value.to_string());
        values.insert(format!("Encoded{variant}"), encoded.clone());
    }
}

fn placeholder_variants(name: &str) -> Vec<String> {
    let mut variants = vec![name.to_string(), name.to_ascii_uppercase()];
    let lower = name.to_ascii_lowercase();
    if !variants.contains(&lower) {
        variants.push(lower);
    }
    variants.dedup();
    variants
}

fn placeholder_name(key: &str) -> String {
    let mut out = String::new();
    let mut upper_next = true;
    for ch in key.chars() {
        if ch == '-' || ch == '_' || ch == ' ' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn default_value(subconverter: &SubconverterConfig, key: &str) -> Option<String> {
    subconverter
        .defaults
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .and_then(|(_, value)| yaml_scalar_to_string(value))
}

fn yaml_scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn normalized_mode(subconverter: &SubconverterConfig) -> String {
    let mode = subconverter.mode.trim();
    if mode.is_empty() {
        "builtin".to_string()
    } else {
        mode.to_ascii_lowercase()
    }
}

fn is_builtin_mode(subconverter: &SubconverterConfig) -> bool {
    normalized_mode(subconverter) == "builtin"
}

fn host_from_address(address: &str) -> Option<String> {
    address.split(':').next().map(ToString::to_string)
}

fn port_from_address(address: &str) -> Option<String> {
    address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .map(|port| port.to_string())
}

fn urlencoding(input: &str) -> String {
    url::form_urlencoded::byte_serialize(input.as_bytes()).collect()
}

pub fn site_name(suburl: &str) -> String {
    Url::parse(suburl)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_string()))
        .map(|host| {
            let parts: Vec<&str> = host.split('.').collect();
            if parts.len() > 1 {
                format!("{}{}", "*.".repeat(parts.len() - 1), parts[parts.len() - 1])
            } else {
                host
            }
        })
        .unwrap_or_else(|| "subscription".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SubconverterConfig, SubconverterTemplate};

    #[test]
    fn protocol_link_uses_subconverter_template() {
        let mut cfg = SubconverterConfig {
            enable: true,
            mode: "substore".to_string(),
            template: SubconverterTemplate {
                backend:
                    "http://$Host:$Port/download/sub?target=$Target&url=$EncodedURL&ua=$EncodedUA"
                        .to_string(),
            },
            ..Default::default()
        };
        cfg.defaults.insert(
            "ua".to_string(),
            serde_yaml::Value::String("clash verge".to_string()),
        );
        let out = protocol_join("tuic://token@example.com:443", &cfg).expect("converted");
        assert!(out.starts_with("http://127.0.0.1:3000/download/sub?target=ClashMeta"));
        assert!(out.contains("url=tuic%3A%2F%2Ftoken%40example.com%3A443"));
        assert!(out.contains("ua=clash+verge"));
    }

    #[test]
    fn http_subscription_rewrites_only_when_external_and_not_converted() {
        let cfg = SubconverterConfig {
            enable: true,
            mode: "subconverter".to_string(),
            template: SubconverterTemplate {
                backend: "http://$Host:$Port/sub?target=$Target&url=$EncodedURL".to_string(),
            },
            ..Default::default()
        };
        let out =
            parse_subscription_url("https://example.com/sub?token=a&b=c", &cfg).expect("rewritten");
        assert_eq!(
            out,
            "http://127.0.0.1:25500/sub?target=clash&url=https%3A%2F%2Fexample.com%2Fsub%3Ftoken%3Da%26b%3Dc"
        );
        let already_converted =
            parse_subscription_url("https://example.com/sub?target=ClashMeta&url=x", &cfg)
                .expect("url");
        assert_eq!(
            already_converted,
            "https://example.com/sub?target=ClashMeta&url=x"
        );
    }

    #[test]
    fn legacy_subconverter_config_still_builds_url() {
        let cfg = SubconverterConfig {
            enable: true,
            mode: "subconverter".to_string(),
            address: "10.0.0.2:25500".to_string(),
            remote_config: Some("https://config.example/r.ini".to_string()),
            ..Default::default()
        };
        let out = protocol_join("vmess://abc", &cfg).expect("converted");
        assert!(out.starts_with("http://10.0.0.2:25500/sub?target=clash"));
        assert!(out.contains("url=vmess%3A%2F%2Fabc"));
        assert!(out.contains("config=https%3A%2F%2Fconfig.example%2Fr.ini"));
    }

    #[test]
    fn injects_runtime_dns_into_clash_yaml() {
        let mut clash = ClashConfig::from_slice(
            br#"
proxies:
  - name: node
    type: ss
"#,
        )
        .expect("clash");
        let dns: RuntimeDnsConfig = serde_yaml::from_str(
            r#"
enable: true
nameserver:
  - 1.1.1.1
"#,
        )
        .expect("dns");
        clash.inject_dns(&dns);
        let injected = clash
            .extra
            .get(Value::String("dns".to_string()))
            .expect("dns inserted");
        assert_eq!(injected["enable"], Value::Bool(true));
        assert_eq!(
            injected["nameserver"],
            Value::Sequence(vec![Value::String("1.1.1.1".to_string())])
        );
        assert_eq!(clash.proxies.len(), 1);
    }
}
