use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::task::TaskRequest;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotState {
    #[serde(default)]
    pub subscriptions: BTreeMap<String, SubscriptionRecord>,
    #[serde(default)]
    pub last_tasks: BTreeMap<i64, TaskRequest>,
    #[serde(default)]
    pub temporary_invites: BTreeMap<i64, DateTime<Utc>>,
    #[serde(default)]
    pub pending_invites: BTreeMap<i64, PendingInvite>,
    #[serde(default)]
    pub anti_group: bool,
    #[serde(default)]
    pub night_shift: bool,
    #[serde(default)]
    pub rules: BTreeMap<String, RuleRecord>,
    #[serde(default)]
    pub granted_users: Vec<i64>,
    #[serde(default)]
    pub pending_tasks: BTreeMap<String, TaskRequest>,
    #[serde(default)]
    pub pending_task_owners: BTreeMap<String, i64>,
    #[serde(default)]
    pub pending_script_pages: BTreeMap<String, usize>,
    #[serde(default)]
    pub pending_script_selections: BTreeMap<String, ScriptSelection>,
    #[serde(default)]
    pub pending_slave_selections: BTreeMap<String, SlaveSelection>,
    #[serde(default)]
    pub pending_config_edits: BTreeMap<i64, PendingConfigEdit>,
    #[serde(default)]
    pub last_echo_at: BTreeMap<i64, DateTime<Utc>>,
}

impl BotState {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read state {}", path.display()))?;
        serde_json::from_str(&raw).context("failed to parse bot state")
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
    state: BotState,
}

impl StateStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = BotState::load(&path)?;
        Ok(Self { path, state })
    }

    pub fn state(&self) -> &BotState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut BotState {
        &mut self.state
    }

    pub fn save(&self) -> Result<()> {
        self.state.save(&self.path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInvite {
    pub rule: String,
    pub chat_id: i64,
    pub message_id: i64,
    pub expires_at: DateTime<Utc>,
}

impl PendingInvite {
    pub fn new(rule: String, chat_id: i64, message_id: i64, expires_at: DateTime<Utc>) -> Self {
        Self {
            rule,
            chat_id,
            message_id,
            expires_at,
        }
    }

    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionRecord {
    pub name: String,
    pub url: String,
    pub password: Option<String>,
    pub owner: i64,
    #[serde(default)]
    pub shared_with: Vec<i64>,
    pub created_at: DateTime<Utc>,
}

impl SubscriptionRecord {
    pub fn new(name: String, url: String, password: Option<String>, owner: i64) -> Self {
        Self {
            name,
            url,
            password,
            owner,
            shared_with: Vec::new(),
            created_at: Utc::now(),
        }
    }

    pub fn can_access(&self, user_id: i64) -> bool {
        self.owner == user_id || self.shared_with.contains(&user_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleRecord {
    pub name: String,
    pub url: String,
    pub owner: i64,
    #[serde(default)]
    pub include: String,
    #[serde(default)]
    pub exclude: String,
    #[serde(default)]
    pub slave_ids: Vec<String>,
    #[serde(default)]
    pub slave_id: Option<String>,
    #[serde(default)]
    pub scripts: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScriptSelection {
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default)]
    pub page: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlaveSelection {
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default)]
    pub page: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingConfigEdit {
    pub path: String,
    pub chat_id: i64,
    pub message_id: i64,
    pub expires_at: DateTime<Utc>,
}

impl PendingConfigEdit {
    pub fn new(path: String, chat_id: i64, message_id: i64, expires_at: DateTime<Utc>) -> Self {
        Self {
            path,
            chat_id,
            message_id,
            expires_at,
        }
    }

    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }
}

impl ScriptSelection {
    pub fn toggle(&mut self, name: &str) {
        if let Some(index) = self.selected.iter().position(|item| item == name) {
            self.selected.remove(index);
        } else {
            self.selected.push(name.to_string());
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.selected.iter().any(|item| item == name)
    }
}

impl SlaveSelection {
    pub fn toggle(&mut self, id: &str) {
        if let Some(index) = self.selected.iter().position(|item| item == id) {
            self.selected.remove(index);
        } else {
            self.selected.push(id.to_string());
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.selected.iter().any(|item| item == id)
    }
}

impl RuleRecord {
    pub fn new(name: String, url: String, owner: i64) -> Self {
        Self {
            name,
            url,
            owner,
            include: String::new(),
            exclude: String::new(),
            slave_ids: Vec::new(),
            slave_id: None,
            scripts: Vec::new(),
            created_at: Utc::now(),
        }
    }

    pub fn can_access(&self, user_id: i64) -> bool {
        self.owner == user_id
    }
}
