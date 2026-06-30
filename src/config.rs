use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KoipyConfig {
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    #[serde(default)]
    pub admin: Vec<UserId>,
    #[serde(default)]
    pub user: Vec<UserId>,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub subscription: SubscriptionConfig,
    #[serde(default)]
    pub bot: BotConfig,
    #[serde(default)]
    pub image: ImageConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub slave_config: SlaveConfig,
    #[serde(default)]
    pub subconverter: SubconverterConfig,
    #[serde(default)]
    pub translation: TranslationConfig,
    #[serde(default)]
    pub script_config: ScriptConfig,
    #[serde(default)]
    pub webapi: WebApiConfig,
    #[serde(default)]
    pub callbacks: CallbackConfig,
    #[serde(default)]
    pub license: String,
    #[serde(default, alias = "log-level")]
    pub log_level: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

pub type UserId = serde_yaml::Value;

impl KoipyConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let mut cfg: Self = serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse YAML config {}", path.display()))?;
        cfg.user.extend(cfg.admin.iter().cloned());
        cfg.user.dedup();
        cfg.script_config.scripts.sort_by_key(|script| script.rank);
        resolve_script_content_paths(&mut cfg, path.parent().unwrap_or_else(|| Path::new(".")))?;
        cfg.source_path = Some(path.to_path_buf());
        Ok(cfg)
    }

    pub fn save_to_source(&self) -> Result<()> {
        let path = self
            .source_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("config source path is not known"))?;
        self.save_to_path(path)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let serialized = serde_yaml::to_string(self).context("failed to serialize config")?;
        fs::write(path, serialized)
            .with_context(|| format!("failed to write config {}", path.display()))?;
        Ok(())
    }

    pub fn grant_user(&mut self, user_id: i64) -> bool {
        let value = serde_yaml::Value::Number(user_id.into());
        if self.user.iter().any(|existing| existing == &value) {
            return false;
        }
        self.user.push(value);
        true
    }

    pub fn revoke_user(&mut self, user_id: i64) -> bool {
        let before = self.user.len();
        self.user.retain(|value| !yaml_value_is_id(value, user_id));
        before != self.user.len()
    }

    pub fn summary(&self) -> String {
        format!(
            "koipy-rs config loaded\nadmins: {}\nusers: {}\nslaves: {}\nscripts: {}\nsubconverter: {}\nwatermark: {}\nlicense: {}",
            self.admin.len(),
            self.user.len(),
            self.visible_slaves().len(),
            self.script_config.scripts.len(),
            if self.subconverter.enable {
                "enabled"
            } else {
                "disabled"
            },
            if self.image.watermark.enable {
                "enabled"
            } else {
                "disabled"
            },
            if self.license.trim().is_empty() {
                "not configured"
            } else {
                "configured"
            },
        )
    }

    pub fn visible_slaves(&self) -> Vec<&SlaveConfigEntry> {
        self.slave_config
            .slaves
            .iter()
            .filter(|slave| !slave.hidden)
            .collect()
    }
}

fn yaml_value_is_id(value: &serde_yaml::Value, user_id: i64) -> bool {
    match value {
        serde_yaml::Value::Number(number) => number.as_i64() == Some(user_id),
        serde_yaml::Value::String(text) => text.trim().parse::<i64>().ok() == Some(user_id),
        _ => false,
    }
}

fn resolve_script_content_paths(cfg: &mut KoipyConfig, base_dir: &Path) -> Result<()> {
    for script in &mut cfg.script_config.scripts {
        let content = script.content.trim();
        if content.is_empty() || content.contains('\n') {
            continue;
        }
        let path = Path::new(content);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.join(path)
        };
        if candidate.is_file() {
            script.content = fs::read_to_string(&candidate).with_context(|| {
                format!(
                    "failed to read script content for {} from {}",
                    script.name,
                    candidate.display()
                )
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConfig {
    pub http_proxy: Option<String>,
    pub socks5_proxy: Option<String>,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
}

fn default_user_agent() -> String {
    "ClashMetaForAndroid/2.8.9.Meta Mihomo/0.16".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionConfig {
    #[serde(default)]
    pub age: SubscriptionAgeConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionAgeConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default, alias = "secret-key", alias = "secret_key")]
    pub secret_key: String,
    #[serde(default, alias = "public-key", alias = "public_key")]
    pub public_key: String,
    #[serde(
        default = "default_age_public_key_header",
        alias = "public-key-header",
        alias = "public_key_header"
    )]
    pub public_key_header: String,
}

impl Default for SubscriptionAgeConfig {
    fn default() -> Self {
        Self {
            enable: false,
            secret_key: String::new(),
            public_key: String::new(),
            public_key_header: default_age_public_key_header(),
        }
    }
}

fn default_age_public_key_header() -> String {
    "X-Age-Public-Key".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct BotConfig {
    pub api_id: Option<i64>,
    pub api_hash: Option<String>,
    pub bot_token: Option<String>,
    pub proxy: Option<String>,
    #[serde(default)]
    pub ipv6: bool,
    #[serde(default, alias = "antiGroup")]
    pub anti_group: bool,
    #[serde(default, alias = "strictMode")]
    pub strict_mode: bool,
    #[serde(default, alias = "bypassMode")]
    pub bypass_mode: bool,
    #[serde(default, alias = "cacheTime")]
    pub cache_time: u64,
    #[serde(default, alias = "parseMode")]
    pub parse_mode: String,
    #[serde(default, alias = "disableNotification")]
    pub disable_notification: bool,
    #[serde(default, alias = "autoResetCommands")]
    pub auto_reset_commands: bool,
    #[serde(
        default,
        alias = "inviteGroup",
        deserialize_with = "deserialize_string_list"
    )]
    pub invite_group: Vec<String>,
    #[serde(
        default,
        alias = "inviteBlacklistURL",
        deserialize_with = "deserialize_string_list"
    )]
    pub invite_blacklist_url: Vec<String>,
    #[serde(default, alias = "inviteBlacklistDomain")]
    pub invite_blacklist_domain: Vec<String>,
    #[serde(default, alias = "echoLimit", deserialize_with = "deserialize_f64")]
    pub echo_limit: f64,
    #[serde(default)]
    pub script_text: String,
    #[serde(default)]
    pub analyze_text: String,
    #[serde(default)]
    pub speed_text: String,
    #[serde(default = "default_bar")]
    pub bar: String,
    #[serde(default = "default_left")]
    pub bleft: String,
    #[serde(default = "default_right")]
    pub bright: String,
    #[serde(default = "default_space")]
    pub bspace: String,
    #[serde(
        default,
        alias = "commands",
        deserialize_with = "deserialize_bot_commands"
    )]
    pub command: Vec<BotCommandConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BotCommandConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default)]
    pub rule: String,
    #[serde(default)]
    pub pin: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub title: String,
    #[serde(default = "default_true")]
    pub attach_to_invite: bool,
}

impl BotCommandConfig {
    pub fn legacy(name: String) -> Self {
        Self {
            name,
            enable: true,
            rule: "test".to_string(),
            pin: false,
            text: String::new(),
            title: String::new(),
            attach_to_invite: true,
        }
    }

    pub fn is_test_command(&self) -> bool {
        self.enable && !self.name.trim().is_empty() && !self.rule.trim().is_empty()
    }
}

fn deserialize_bot_commands<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<BotCommandConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<serde_yaml::Value>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| match value {
            serde_yaml::Value::String(name) => Ok(BotCommandConfig::legacy(name)),
            other => serde_yaml::from_value(other).map_err(serde::de::Error::custom),
        })
        .collect()
}

fn deserialize_string_list<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_yaml::Value::Null => Vec::new(),
        serde_yaml::Value::String(value) => split_string_list(&value),
        serde_yaml::Value::Sequence(items) => items
            .into_iter()
            .filter_map(|item| yaml_scalar_to_string(&item))
            .flat_map(|value| split_string_list(&value))
            .collect(),
        other => yaml_scalar_to_string(&other)
            .map(|value| split_string_list(&value))
            .unwrap_or_default(),
    })
}

fn split_string_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn deserialize_f64<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    match value {
        serde_yaml::Value::Null => Ok(0.0),
        serde_yaml::Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("number is not representable as f64")),
        serde_yaml::Value::String(value) => value
            .trim()
            .parse()
            .map_err(|err| serde::de::Error::custom(format!("invalid f64 value: {err}"))),
        other => Err(serde::de::Error::custom(format!(
            "expected f64-compatible value, got {other:?}"
        ))),
    }
}

fn yaml_scalar_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(value) => Some(value.clone()),
        serde_yaml::Value::Number(value) => Some(value.to_string()),
        serde_yaml::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn default_bar() -> String {
    "=".to_string()
}

fn default_left() -> String {
    "[".to_string()
}

fn default_right() -> String {
    "]".to_string()
}

fn default_space() -> String {
    "  ".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    #[serde(default = "default_runtime_entrance")]
    pub entrance: RuntimeEntrance,
    #[serde(default = "default_true")]
    pub ipstack: bool,
    #[serde(default = "default_ping_url")]
    pub ping_url: String,
    #[serde(default = "default_speed_files")]
    pub speed_files: Vec<String>,
    #[serde(default = "default_speed_nodes")]
    pub speed_nodes: usize,
    #[serde(default = "default_speed_threads")]
    pub speed_threads: usize,
    #[serde(default)]
    pub nospeed: bool,
    #[serde(default)]
    pub localip: bool,
    #[serde(default)]
    pub dasn: Option<String>,
    #[serde(default)]
    pub dorg: Option<String>,
    #[serde(default)]
    pub dcity: Option<String>,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default = "default_geoip_api")]
    pub geoip_api: String,
    #[serde(default)]
    pub geoip_key: Option<String>,
    #[serde(default, alias = "disableSubCvt")]
    pub disable_sub_cvt: bool,
    #[serde(default)]
    pub realtime: bool,
    #[serde(default = "default_runtime_output")]
    pub output: String,
    #[serde(default)]
    pub duration: u64,
    #[serde(default, alias = "protectContent")]
    pub protect_content: bool,
    #[serde(default)]
    pub dns: Option<RuntimeDnsConfig>,
    #[serde(default, alias = "enableDNSInject")]
    pub enable_dns_inject: bool,
    #[serde(default)]
    pub include_filter: String,
    #[serde(default)]
    pub exclude_filter: String,
    #[serde(default)]
    pub sort: SortType,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeDnsConfig {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default, alias = "nameserver")]
    pub nameserver: Vec<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            ping_url: default_ping_url(),
            entrance: default_runtime_entrance(),
            ipstack: true,
            speed_files: default_speed_files(),
            speed_nodes: default_speed_nodes(),
            speed_threads: default_speed_threads(),
            nospeed: false,
            localip: false,
            dasn: None,
            dorg: None,
            dcity: None,
            interval: default_interval(),
            geoip_api: default_geoip_api(),
            geoip_key: None,
            disable_sub_cvt: false,
            realtime: false,
            output: default_runtime_output(),
            duration: 0,
            protect_content: false,
            dns: None,
            enable_dns_inject: false,
            include_filter: String::new(),
            exclude_filter: String::new(),
            sort: SortType::Origin,
        }
    }
}

fn default_ping_url() -> String {
    "https://www.gstatic.com/generate_204".to_string()
}

fn default_speed_files() -> Vec<String> {
    vec!["https://dl.google.com/dl/android/studio/install/3.4.1.0/android-studio-ide-183.5522156-windows.exe".to_string()]
}

fn default_speed_nodes() -> usize {
    300
}

fn default_speed_threads() -> usize {
    4
}

fn default_runtime_output() -> String {
    "image".to_string()
}

fn default_interval() -> u64 {
    10
}

fn default_geoip_api() -> String {
    "ip-api.com".to_string()
}

fn default_runtime_entrance() -> RuntimeEntrance {
    RuntimeEntrance::Enabled(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEntrance {
    Enabled(bool),
    Mode(String),
}

impl RuntimeEntrance {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Enabled(value) => *value,
            Self::Mode(value) => !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "false" | "none" | "off" | "disable" | "disabled"
            ),
        }
    }

    pub fn mode(&self) -> Option<&str> {
        match self {
            Self::Mode(value) if !value.trim().is_empty() => Some(value.as_str()),
            _ => None,
        }
    }
}

impl Default for RuntimeEntrance {
    fn default() -> Self {
        default_runtime_entrance()
    }
}

impl<'de> Deserialize<'de> for RuntimeEntrance {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::Bool(value) => Ok(Self::Enabled(value)),
            serde_yaml::Value::String(value) => {
                let normalized = value.trim().to_ascii_lowercase();
                match normalized.as_str() {
                    "true" => Ok(Self::Enabled(true)),
                    "false" => Ok(Self::Enabled(false)),
                    _ => Ok(Self::Mode(value)),
                }
            }
            serde_yaml::Value::Null => Ok(Self::default()),
            other => Err(serde::de::Error::custom(format!(
                "runtime.entrance must be a bool or string, got {other:?}"
            ))),
        }
    }
}

impl Serialize for RuntimeEntrance {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Enabled(value) => serializer.serialize_bool(*value),
            Self::Mode(value) => serializer.serialize_str(value),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfig {
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_speed_format")]
    pub speed_format: String,
    #[serde(default)]
    pub font: String,
    #[serde(default)]
    pub compress: bool,
    #[serde(default)]
    pub end_colors_switch: bool,
    #[serde(default)]
    pub speed_end_color_switch: bool,
    #[serde(default)]
    pub invert: bool,
    #[serde(default = "default_true")]
    pub save: bool,
    #[serde(default = "default_pixel_threshold")]
    pub pixel_threshold: String,
    #[serde(default = "default_true")]
    pub logo: bool,
    #[serde(default = "default_true")]
    pub show_unsafe_tips: bool,
    #[serde(default)]
    pub emoji: EmojiConfig,
    #[serde(default)]
    pub watermark: WatermarkConfig,
    #[serde(default)]
    pub non_commercial_watermark: WatermarkConfig,
    #[serde(default)]
    pub color: ImageColorConfig,
}

fn default_title() -> String {
    "Koipy".to_string()
}

fn default_speed_format() -> String {
    "byte/decimal".to_string()
}

fn default_pixel_threshold() -> String {
    "2500x3500".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmojiConfig {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default = "default_emoji_source")]
    pub source: String,
}

impl Default for EmojiConfig {
    fn default() -> Self {
        Self {
            enable: true,
            source: default_emoji_source(),
        }
    }
}

fn default_emoji_source() -> String {
    "TwemojiLocalSource".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatermarkConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_watermark_alpha")]
    pub alpha: u8,
    #[serde(default = "default_watermark_angle")]
    pub angle: f32,
    #[serde(default = "default_watermark_color")]
    pub color: ColorStop,
    #[serde(default = "default_watermark")]
    pub text: String,
    #[serde(default = "default_watermark_size")]
    pub size: u32,
    #[serde(default, alias = "row-spacing", alias = "row_spacing")]
    pub row_spacing: u32,
    #[serde(default)]
    pub shadow: bool,
    #[serde(default, alias = "start-y", alias = "start_y")]
    pub start_y: u32,
    #[serde(default)]
    pub trace: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageColorConfig {
    #[serde(default)]
    pub background: ImageBackgroundColorConfig,
    #[serde(default)]
    pub delay: Vec<ColorStop>,
    #[serde(default)]
    pub speed: Vec<ColorStop>,
    #[serde(default = "default_yes_color")]
    pub yes: ColorStop,
    #[serde(default = "default_no_color")]
    pub no: ColorStop,
    #[serde(default = "default_na_color")]
    pub na: ColorStop,
    #[serde(default = "default_warn_color")]
    pub warn: ColorStop,
    #[serde(default = "default_wait_color")]
    pub wait: ColorStop,
    #[serde(default = "default_xline_color")]
    pub xline: ColorStop,
    #[serde(default = "default_yline_color")]
    pub yline: ColorStop,
    #[serde(default = "default_font_color")]
    pub font: ColorStop,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageBackgroundColorConfig {
    #[serde(default)]
    pub inbound: ColorStop,
    #[serde(default)]
    pub outbound: ColorStop,
    #[serde(default = "default_background_script")]
    pub script: ColorStop,
    #[serde(default = "default_background_title")]
    pub script_title: ColorStop,
    #[serde(default)]
    pub speed: ColorStop,
    #[serde(default = "default_background_title")]
    pub speed_title: ColorStop,
    #[serde(default = "default_background_title")]
    pub topo_title: ColorStop,
    #[serde(default)]
    pub speed_max: ColorStop,
    #[serde(default)]
    pub speed_avg: ColorStop,
    #[serde(default)]
    pub delay: ColorStop,
    #[serde(default)]
    pub udp: ColorStop,
    #[serde(default)]
    pub status: ColorStop,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorStop {
    #[serde(default)]
    pub label: f64,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_color_value")]
    pub value: String,
    #[serde(
        default = "default_color_value",
        alias = "end-color",
        alias = "end_color"
    )]
    pub end_color: String,
    #[serde(default = "default_alpha")]
    pub alpha: u8,
}

impl Default for ColorStop {
    fn default() -> Self {
        Self {
            label: 0.0,
            name: String::new(),
            value: default_color_value(),
            end_color: default_color_value(),
            alpha: default_alpha(),
        }
    }
}

fn default_color_value() -> String {
    "#ffffff".to_string()
}

fn default_alpha() -> u8 {
    255
}

fn default_yes_color() -> ColorStop {
    ColorStop {
        value: "#bee47e".to_string(),
        ..Default::default()
    }
}

fn default_no_color() -> ColorStop {
    ColorStop {
        value: "#ee6b73".to_string(),
        ..Default::default()
    }
}

fn default_na_color() -> ColorStop {
    ColorStop {
        value: "#8d8b8e".to_string(),
        ..Default::default()
    }
}

fn default_warn_color() -> ColorStop {
    ColorStop {
        value: "#fcc43c".to_string(),
        ..Default::default()
    }
}

fn default_wait_color() -> ColorStop {
    ColorStop {
        value: "#dcc7e1".to_string(),
        ..Default::default()
    }
}

fn default_xline_color() -> ColorStop {
    ColorStop {
        value: "#E1E1E1".to_string(),
        ..Default::default()
    }
}

fn default_yline_color() -> ColorStop {
    ColorStop {
        value: "#EAEAEA".to_string(),
        ..Default::default()
    }
}

fn default_font_color() -> ColorStop {
    ColorStop {
        value: "#000000".to_string(),
        ..Default::default()
    }
}

fn default_background_script() -> ColorStop {
    ColorStop {
        value: "#ffffff".to_string(),
        ..Default::default()
    }
}

fn default_background_title() -> ColorStop {
    ColorStop {
        value: "#EAEAEA".to_string(),
        ..Default::default()
    }
}

impl Default for WatermarkConfig {
    fn default() -> Self {
        Self {
            enable: false,
            alpha: default_watermark_alpha(),
            angle: default_watermark_angle(),
            color: default_watermark_color(),
            text: default_watermark(),
            size: default_watermark_size(),
            row_spacing: 0,
            shadow: false,
            start_y: 0,
            trace: false,
        }
    }
}

fn default_watermark_alpha() -> u8 {
    32
}

fn default_watermark_angle() -> f32 {
    -16.0
}

fn default_watermark_color() -> ColorStop {
    ColorStop {
        value: "#000000".to_string(),
        alpha: 16,
        ..Default::default()
    }
}

fn default_watermark() -> String {
    "FullTclash dev".to_string()
}

fn default_watermark_size() -> u32 {
    64
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubconverterConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_subconverter_mode")]
    pub mode: String,
    #[serde(default)]
    pub template: SubconverterTemplate,
    #[serde(default)]
    pub defaults: BTreeMap<String, serde_yaml::Value>,
    #[serde(default = "default_subconverter_addr")]
    pub address: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub include: String,
    #[serde(default)]
    pub exclude: String,
    pub remote_config: Option<String>,
}

impl Default for SubconverterConfig {
    fn default() -> Self {
        Self {
            enable: false,
            mode: default_subconverter_mode(),
            template: SubconverterTemplate::default(),
            defaults: BTreeMap::new(),
            address: default_subconverter_addr(),
            tls: false,
            include: String::new(),
            exclude: String::new(),
            remote_config: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubconverterTemplate {
    #[serde(default)]
    pub backend: String,
}

fn default_subconverter_mode() -> String {
    "builtin".to_string()
}

fn default_subconverter_addr() -> String {
    "127.0.0.1:25500".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlaveConfig {
    #[serde(default)]
    pub default: String,
    #[serde(default = "default_true", alias = "showID")]
    pub show_id: bool,
    #[serde(default)]
    pub health_check: HealthCheckConfig,
    #[serde(default)]
    pub speed_scheduling: SpeedScheduling,
    #[serde(default = "default_true")]
    pub geo_clustering: bool,
    #[serde(default)]
    pub slaves: Vec<SlaveConfigEntry>,
}

impl Default for SlaveConfig {
    fn default() -> Self {
        Self {
            default: String::new(),
            show_id: true,
            health_check: HealthCheckConfig::default(),
            speed_scheduling: SpeedScheduling::default(),
            geo_clustering: true,
            slaves: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheckConfig {
    #[serde(default = "default_health_samples")]
    pub num_samples: usize,
    #[serde(default = "default_status_style")]
    pub show_status_style: String,
    #[serde(default)]
    pub auto_hide_on_failure: bool,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            num_samples: default_health_samples(),
            show_status_style: default_status_style(),
            auto_hide_on_failure: false,
        }
    }
}

fn default_health_samples() -> usize {
    10
}

fn default_status_style() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpeedScheduling {
    Concurrent,
    #[default]
    Pipeline,
    Sequential,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlaveConfigEntry {
    pub id: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub r#type: SlaveType,
    #[serde(default)]
    pub address: String,
    #[serde(default = "default_slave_path")]
    pub path: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub skip_cert_verify: bool,
    #[serde(default = "default_true")]
    pub tls: bool,
    pub invoker: Option<String>,
    pub buildtoken: Option<String>,
    #[serde(default)]
    pub option: MiaoSpeedOption,
}

fn default_true() -> bool {
    true
}

fn default_slave_path() -> String {
    "/".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SlaveType {
    #[default]
    MiaoSpeed,
    FullTclash,
    Websocket,
    Bot,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MiaoSpeedOption {
    #[serde(default = "default_download_duration")]
    pub download_duration: u64,
    #[serde(default = "default_download_threading")]
    pub download_threading: u64,
    #[serde(default = "default_ping_average")]
    pub ping_average_over: u64,
    #[serde(default = "default_task_retry")]
    pub task_retry: u64,
    #[serde(default = "default_download_url")]
    pub download_url: String,
    #[serde(default = "default_ping_address")]
    pub ping_address: String,
    #[serde(default = "default_stun")]
    pub stun_url: String,
    #[serde(default = "default_task_timeout")]
    pub task_timeout: u64,
    #[serde(default, alias = "dnsServer", alias = "dnsServers")]
    pub dns_server: Vec<String>,
    #[serde(default = "default_api_version")]
    pub api_version: u64,
    #[serde(default = "default_upload_url")]
    pub upload_url: String,
    #[serde(default = "default_download_duration")]
    pub upload_duration: u64,
    #[serde(default = "default_download_threading")]
    pub upload_threading: u64,
}

impl Default for MiaoSpeedOption {
    fn default() -> Self {
        Self {
            download_duration: default_download_duration(),
            download_threading: default_download_threading(),
            ping_average_over: default_ping_average(),
            task_retry: default_task_retry(),
            download_url: default_download_url(),
            ping_address: default_ping_address(),
            stun_url: default_stun(),
            task_timeout: default_task_timeout(),
            dns_server: Vec::new(),
            api_version: default_api_version(),
            upload_url: default_upload_url(),
            upload_duration: default_download_duration(),
            upload_threading: default_download_threading(),
        }
    }
}

fn default_download_duration() -> u64 {
    8
}

fn default_download_threading() -> u64 {
    4
}

fn default_ping_average() -> u64 {
    3
}

fn default_task_retry() -> u64 {
    3
}

fn default_download_url() -> String {
    default_speed_files().remove(0)
}

fn default_ping_address() -> String {
    "https://cp.cloudflare.com/generate_204".to_string()
}

fn default_stun() -> String {
    "udp://stun.ideasip.com:3478".to_string()
}

fn default_task_timeout() -> u64 {
    2500
}

fn default_api_version() -> u64 {
    1
}

fn default_upload_url() -> String {
    "https://speed.cloudflare.com/__up".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ScriptConfig {
    #[serde(default)]
    pub scripts: Vec<Script>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Script {
    #[serde(default = "default_script_type")]
    pub r#type: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rank: i64,
    #[serde(default)]
    pub content: String,
}

fn default_script_type() -> String {
    "gojajs".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TranslationConfig {
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default)]
    pub resources: BTreeMap<String, String>,
}

fn default_lang() -> String {
    "zh-CN".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RuleConfig {
    pub name: String,
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default, alias = "script")]
    pub scripts: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub slaveid: Vec<String>,
    #[serde(default)]
    pub sort: SortType,
    #[serde(default)]
    pub owner: Option<i64>,
    #[serde(default)]
    pub runtime: Option<RuntimeConfig>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum SortType {
    #[default]
    #[serde(alias = "订阅原序")]
    Origin,
    #[serde(alias = "HTTP升序")]
    HttpAsc,
    #[serde(alias = "HTTP降序")]
    HttpDesc,
    #[serde(alias = "平均速度升序")]
    AvgSpeedAsc,
    #[serde(alias = "平均速度降序")]
    AvgSpeedDesc,
    #[serde(alias = "最大速度升序")]
    MaxSpeedAsc,
    #[serde(alias = "最大速度降序")]
    MaxSpeedDesc,
}

impl SortType {
    pub fn parse_text(value: &str) -> Option<Self> {
        match value {
            "订阅原序" | "origin" => Some(Self::Origin),
            "HTTP升序" | "http" => Some(Self::HttpAsc),
            "HTTP降序" | "rhttp" => Some(Self::HttpDesc),
            "平均速度升序" | "aspeed" => Some(Self::AvgSpeedAsc),
            "平均速度降序" | "arspeed" => Some(Self::AvgSpeedDesc),
            "最大速度升序" | "mspeed" => Some(Self::MaxSpeedAsc),
            "最大速度降序" | "mrspeed" => Some(Self::MaxSpeedDesc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebApiConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_webapi_address")]
    pub address: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default, alias = "cert", alias = "tlsCertFile")]
    pub cert_path: String,
    #[serde(default, alias = "key", alias = "tlsKeyFile")]
    pub key_path: String,
    #[serde(default, alias = "allowOrigins")]
    pub allow_origins: Vec<String>,
    #[serde(default)]
    pub on_message: Option<String>,
    #[serde(default)]
    pub on_pre_send: Option<String>,
    #[serde(default)]
    pub on_result: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallbackConfig {
    #[serde(default)]
    pub on_message: Option<String>,
    #[serde(default)]
    pub on_pre_send: Option<String>,
    #[serde(default)]
    pub on_result: Option<String>,
}

fn default_webapi_address() -> String {
    "127.0.0.1:8080".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_bot_commands() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
bot:
  commands:
    - name: ping
      enable: true
      rule: ping
      pin: true
      text: PING test
      attachToInvite: false
    - name: disabled
      enable: false
      rule: disabled
"#,
        )
        .expect("config");
        assert_eq!(cfg.bot.command.len(), 2);
        assert_eq!(cfg.bot.command[0].name, "ping");
        assert_eq!(cfg.bot.command[0].rule, "ping");
        assert!(cfg.bot.command[0].pin);
        assert!(!cfg.bot.command[0].attach_to_invite);
        assert!(!cfg.bot.command[1].enable);
    }

    #[test]
    fn parses_legacy_bot_command_strings() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
bot:
  command:
    - mytest
"#,
        )
        .expect("config");
        assert_eq!(
            cfg.bot.command[0],
            BotCommandConfig::legacy("mytest".to_string())
        );
        assert!(cfg.bot.command[0].is_test_command());
    }

    #[test]
    fn parses_documented_bot_runtime_options() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
bot:
  bypassMode: true
  parseMode: MarkdownV2
  disableNotification: true
  autoResetCommands: true
  cacheTime: 60
  inviteGroup: "-100123"
  inviteBlacklistURL: https://example.com/blacklist.txt
  inviteBlacklistDomain:
    - blocked.example
  echoLimit: 10
runtime:
  entrance: false
  ipstack: false
  protectContent: true
  enableDNSInject: true
  disableSubCvt: true
  realtime: true
  output: json
  duration: 12
  dns:
    enable: true
    nameserver:
      - 1.1.1.1
"#,
        )
        .expect("config");
        assert!(cfg.bot.bypass_mode);
        assert_eq!(cfg.bot.parse_mode, "MarkdownV2");
        assert!(cfg.bot.disable_notification);
        assert!(cfg.bot.auto_reset_commands);
        assert_eq!(cfg.bot.cache_time, 60);
        assert_eq!(cfg.bot.invite_group, vec!["-100123"]);
        assert_eq!(
            cfg.bot.invite_blacklist_url,
            vec!["https://example.com/blacklist.txt"]
        );
        assert_eq!(cfg.bot.invite_blacklist_domain, vec!["blocked.example"]);
        assert_eq!(cfg.bot.echo_limit, 10.0);
        assert!(!cfg.runtime.entrance.enabled());
        assert!(!cfg.runtime.ipstack);
        assert!(cfg.runtime.protect_content);
        assert!(cfg.runtime.enable_dns_inject);
        assert!(cfg.runtime.disable_sub_cvt);
        assert!(cfg.runtime.realtime);
        assert_eq!(cfg.runtime.output, "json");
        assert_eq!(cfg.runtime.duration, 12);
        assert_eq!(
            cfg.runtime.dns.as_ref().map(|dns| dns.enable),
            Some(true)
        );
        assert_eq!(
            cfg.runtime.dns.as_ref().map(|dns| dns.nameserver.clone()),
            Some(vec!["1.1.1.1".to_string()])
        );

        let backend: KoipyConfig = serde_yaml::from_str(
            r#"
slaveConfig:
  slaves:
    - type: miaospeed
      id: local
      address: 127.0.0.1:8765
      option:
        dnsServer:
          - 119.29.29.29:53
"#,
        )
        .expect("config");
        assert_eq!(
            backend.slave_config.slaves[0].option.dns_server,
            vec!["119.29.29.29:53".to_string()]
        );
    }

    #[test]
    fn parses_legacy_runtime_entrance_mode() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
runtime:
  entrance: ip
"#,
        )
        .expect("config");
        assert!(cfg.runtime.entrance.enabled());
        assert_eq!(cfg.runtime.entrance.mode(), Some("ip"));
    }

    #[test]
    fn parses_closed_binary_config_surface() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
license: YOUR_LICENSE_CODE
log-level: INFO
bot:
  inviteGroup:
    - "-100123"
  inviteBlacklistURL:
    - https://example.com/url.txt
    - https://example.com/domain.txt
  echoLimit: 0.8
callbacks:
  onMessage: http://127.0.0.1:8080/onMessage
webapi:
  tls: true
  tlsCertFile: cert.pem
  tlsKeyFile: key.pem
runtime:
  localip: false
slaveConfig:
  healthCheck:
    numSamples: 10
    showStatusStyle: emoji
    autoHideOnFailure: true
  speedScheduling: pipeline
  geoClustering: true
  slaves:
    - type: miaospeed
      id: local
      address: 127.0.0.1:8765
      option:
        taskTimeout: 2500
        dnsServer:
          - 119.29.29.29:53
          - https://dns.google/dns-query
        apiVersion: 3
        uploadURL: https://speed.cloudflare.com/__up
        uploadDuration: 8
        uploadThreading: 4
rules:
  - name: multi
    slaveid: [local, backup]
"#,
        )
        .expect("config");

        assert_eq!(cfg.license, "YOUR_LICENSE_CODE");
        assert_eq!(cfg.log_level, "INFO");
        assert_eq!(cfg.bot.invite_group, vec!["-100123"]);
        assert_eq!(
            cfg.bot.invite_blacklist_url,
            vec![
                "https://example.com/url.txt",
                "https://example.com/domain.txt"
            ]
        );
        assert_eq!(cfg.bot.echo_limit, 0.8);
        assert_eq!(
            cfg.callbacks.on_message.as_deref(),
            Some("http://127.0.0.1:8080/onMessage")
        );
        assert_eq!(cfg.webapi.cert_path, "cert.pem");
        assert_eq!(cfg.webapi.key_path, "key.pem");
        assert_eq!(cfg.slave_config.health_check.num_samples, 10);
        assert!(cfg.slave_config.health_check.auto_hide_on_failure);
        assert_eq!(cfg.slave_config.speed_scheduling, SpeedScheduling::Pipeline);
        assert!(cfg.slave_config.geo_clustering);
        let option = &cfg.slave_config.slaves[0].option;
        assert_eq!(option.task_timeout, 2500);
        assert_eq!(
            option.dns_server,
            vec!["119.29.29.29:53", "https://dns.google/dns-query"]
        );
        assert_eq!(option.api_version, 3);
        assert_eq!(option.upload_url, "https://speed.cloudflare.com/__up");
        assert_eq!(cfg.rules[0].slaveid, vec!["local", "backup"]);
    }

    #[test]
    fn parses_closed_binary_resource_config_when_available() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let zip = manifest.join("koipy-linux-amd64.zip");
        let extracted = std::env::temp_dir()
            .join("koipy-linux-amd64-unpack")
            .join("resources")
            .join("config.example.yaml");
        let source = if extracted.is_file() {
            Some(extracted)
        } else if zip.is_file() {
            None
        } else {
            return;
        };

        let Some(path) = source else {
            return;
        };
        let cfg = KoipyConfig::from_path(&path).expect("closed binary config");
        assert!(!cfg.license.trim().is_empty());
        assert!(cfg.bot.echo_limit >= 0.0);
        assert!(!cfg.script_config.scripts.is_empty());
        assert!(cfg.slave_config.health_check.num_samples > 0);
        assert!(!cfg.slave_config.slaves.is_empty());
        assert!(
            !cfg.slave_config.slaves[0]
                .option
                .upload_url
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn parses_documented_subscription_age_options() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
subscription:
  age:
    enable: true
    secretKey: AGE-SECRET-KEY-EXAMPLE
    publicKey: age1example
    publicKeyHeader: X-Custom-Age-Key
"#,
        )
        .expect("config");
        assert!(cfg.subscription.age.enable);
        assert_eq!(cfg.subscription.age.secret_key, "AGE-SECRET-KEY-EXAMPLE");
        assert_eq!(cfg.subscription.age.public_key, "age1example");
        assert_eq!(cfg.subscription.age.public_key_header, "X-Custom-Age-Key");
    }

    #[test]
    fn parses_documented_image_color_options() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r##"
image:
  speedFormat: bit/decimal
  endColorsSwitch: true
  speedEndColorSwitch: true
  invert: true
  save: false
  pixelThreshold: 1200x1600
  color:
    background:
      script:
        value: "#010203"
      scriptTitle:
        value: "#040506"
      speedMax:
        value: "#121212"
      delay:
        value: "#131313"
    yes:
      value: "#111111"
      end-color: "#222222"
    speed:
      - label: 1
        value: "#333333"
        end_color: "#444444"
"##,
        )
        .expect("config");
        assert_eq!(cfg.image.speed_format, "bit/decimal");
        assert!(cfg.image.end_colors_switch);
        assert!(cfg.image.speed_end_color_switch);
        assert!(cfg.image.invert);
        assert!(!cfg.image.save);
        assert_eq!(cfg.image.pixel_threshold, "1200x1600");
        assert_eq!(cfg.image.color.yes.end_color, "#222222");
        assert_eq!(cfg.image.color.speed[0].end_color, "#444444");
        assert_eq!(cfg.image.color.background.speed_max.value, "#121212");
        assert_eq!(cfg.image.color.background.delay.value, "#131313");
    }

    #[test]
    fn parses_documented_watermark_options() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r##"
image:
  watermark:
    enable: true
    alpha: 32
    angle: -16.0
    row-spacing: 12
    shadow: true
    start-y: 24
    trace: true
    color:
      alpha: 16
      value: "#000000"
"##,
        )
        .expect("config");
        assert!(cfg.image.watermark.enable);
        assert_eq!(cfg.image.watermark.alpha, 32);
        assert_eq!(cfg.image.watermark.angle, -16.0);
        assert_eq!(cfg.image.watermark.row_spacing, 12);
        assert!(cfg.image.watermark.shadow);
        assert_eq!(cfg.image.watermark.start_y, 24);
        assert_eq!(cfg.image.watermark.color.alpha, 16);
        assert!(cfg.image.watermark.trace);
    }

    #[test]
    fn parses_documented_webapi_options() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
webapi:
  enable: true
  address: 0.0.0.0:9090
  password: secret
  tls: true
  certPath: cert.pem
  keyPath: key.pem
  allowOrigins:
    - https://dash.example
  onMessage: https://hooks.example/message
"#,
        )
        .expect("config");
        assert!(cfg.webapi.enable);
        assert_eq!(cfg.webapi.address, "0.0.0.0:9090");
        assert_eq!(cfg.webapi.password, "secret");
        assert!(cfg.webapi.tls);
        assert_eq!(cfg.webapi.cert_path, "cert.pem");
        assert_eq!(cfg.webapi.key_path, "key.pem");
        assert_eq!(cfg.webapi.allow_origins, vec!["https://dash.example"]);
        assert_eq!(
            cfg.webapi.on_message.as_deref(),
            Some("https://hooks.example/message")
        );
    }

    #[test]
    fn parses_documented_slave_path_option() {
        let cfg: KoipyConfig = serde_yaml::from_str(
            r#"
slaveConfig:
  showID: false
  slaves:
    - type: miaospeed
      id: localmiaospeed
      token: secret
      address: 127.0.0.1:8765
      path: /miaospeed
      proxy: http://user:pass@proxy.example.com:7890
      tls: true
"#,
        )
        .expect("config");
        assert!(!cfg.slave_config.show_id);
        assert_eq!(cfg.slave_config.slaves[0].path, "/miaospeed");
        assert_eq!(
            cfg.slave_config.slaves[0].proxy.as_deref(),
            Some("http://user:pass@proxy.example.com:7890")
        );
        let defaulted: KoipyConfig = serde_yaml::from_str(
            r#"
slaveConfig:
  slaves:
    - id: no-path
      address: 127.0.0.1:8765
"#,
        )
        .expect("config");
        assert_eq!(defaulted.slave_config.slaves[0].path, "/");
    }

    #[test]
    fn grants_user_and_persists_config() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("koipy-rs-config-save-{}.yaml", std::process::id()));
        fs::write(&path, "user: []\n").expect("seed config");
        let mut cfg = KoipyConfig::from_path(&path).expect("load");
        assert!(cfg.grant_user(12345));
        assert!(!cfg.grant_user(12345));
        cfg.save_to_source().expect("save");
        let reloaded = KoipyConfig::from_path(&path).expect("reload");
        assert!(
            reloaded
                .user
                .iter()
                .any(|value| matches!(value, serde_yaml::Value::Number(number) if number.as_i64() == Some(12345)))
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn revokes_user_from_config() {
        let mut cfg = KoipyConfig::default();
        cfg.user.push(serde_yaml::Value::Number(12345.into()));
        cfg.user
            .push(serde_yaml::Value::String("67890".to_string()));

        assert!(cfg.revoke_user(12345));
        assert!(cfg.revoke_user(67890));
        assert!(!cfg.revoke_user(67890));
        assert!(cfg.user.is_empty());
    }
}
