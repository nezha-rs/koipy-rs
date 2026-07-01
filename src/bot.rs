use anyhow::{Context, Result, bail};
use chrono::{Duration, Local, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;
use sysinfo::System;
use tokio::sync::{Mutex, RwLock};

use crate::app::KoipyApp;
use crate::cleaner::{parse_subscription_url, site_name};
use crate::config::{BotCommandConfig, KoipyConfig, RuleConfig, RuntimeConfig, SlaveType};
use crate::image::{RenderContext, RenderedMedia, ResultRenderer};
use crate::miaospeed::{MiaoSpeedProgress, ping_slave};
use crate::result::{TestResultRow, TestResultTable};
use crate::state::{PendingInvite, RuleRecord, StateStore, SubscriptionRecord};
use crate::subscription::{SubscriptionCollector, SubscriptionTraffic};
use crate::task::{OutputMode, TaskKind, TaskRequest};
use crate::webhooks::{WebhookClient, WebhookEvent};

#[derive(Debug, Clone)]
pub struct BotRuntime {
    config: Arc<RwLock<KoipyConfig>>,
    api: TelegramApi,
    checkslaves_lock: Arc<Mutex<()>>,
}

impl BotRuntime {
    pub fn new(config: KoipyConfig) -> Result<Self> {
        let token = config
            .bot
            .bot_token
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("bot.bot-token is required before Telegram serving can start")
            })?;
        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            api: TelegramApi::new(token)?,
            checkslaves_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn run(&self) -> Result<()> {
        let mut offset = 0_i64;
        let mut store = StateStore::open("koipy-state.json")?;
        let startup_config = self.config.read().await.clone();
        self.sync_bot_commands(&startup_config).await?;
        tracing::info!("Telegram long polling started");
        loop {
            let updates = self.api.get_updates(offset).await?;
            for update in updates {
                offset = update.update_id + 1;
                if let Some(callback) = update.callback_query {
                    if let Err(err) = self.handle_callback(&mut store, callback).await {
                        tracing::warn!("callback failed: {err:#}");
                    }
                } else if let Some(message) = update.message {
                    let config = self.config.read().await.clone();
                    if self
                        .handle_anti_group_join(&config, &store, &message)
                        .await?
                    {
                        continue;
                    }
                    if let Some(text) = message.text.as_deref() {
                        let reply = self
                            .handle_message(&config, &mut store, &message, text)
                            .await;
                        match reply {
                            Ok(Some(reply_text)) => {
                                self.api
                                    .send_message(
                                        message.chat.id,
                                        &reply_text,
                                        &SendOptions::from_config(&config),
                                    )
                                    .await?;
                            }
                            Ok(None) => {}
                            Err(err) => {
                                self.api
                                    .send_message(
                                        message.chat.id,
                                        &format!("Error: {err:#}"),
                                        &SendOptions::from_config(&config),
                                    )
                                    .await?;
                            }
                        }
                    } else if let Some(reply_text) = non_text_message_reply(&message) {
                        self.api
                            .send_message(
                                message.chat.id,
                                reply_text,
                                &SendOptions::from_config(&config),
                            )
                            .await?;
                    }
                }
            }
        }
    }

    async fn sync_bot_commands(&self, config: &KoipyConfig) -> Result<()> {
        if !config.bot.auto_reset_commands {
            return Ok(());
        }
        self.api.delete_my_commands().await?;
        let commands = pinned_bot_commands(config);
        if !commands.is_empty() {
            self.api.set_my_commands(&commands).await?;
        }
        Ok(())
    }

    async fn handle_anti_group_join(
        &self,
        config: &KoipyConfig,
        store: &StateStore,
        message: &TelegramMessage,
    ) -> Result<bool> {
        if !anti_group_should_leave(config, store, message) {
            return Ok(false);
        }
        self.api
            .send_message(
                message.chat.id,
                "Anti-pull active, contact admin",
                &SendOptions::from_config(config),
            )
            .await?;
        self.api.leave_chat(message.chat.id).await?;
        Ok(true)
    }

    async fn handle_message(
        &self,
        config: &KoipyConfig,
        store: &mut StateStore,
        message: &TelegramMessage,
        text: &str,
    ) -> Result<Option<String>> {
        let user_id = message
            .from
            .as_ref()
            .map(|user| user.id)
            .unwrap_or(message.chat.id);
        let invite_state_changed = prune_expired_temporary_invites(store)
            | prune_expired_pending_invites(store)
            | prune_expired_config_edits(store);
        if let Some(pending) = store.state().pending_config_edits.get(&user_id).cloned() {
            if pending.is_active(Utc::now()) {
                if text.trim() == "/cancel" {
                    store.state_mut().pending_config_edits.remove(&user_id);
                    store.save()?;
                    return Ok(Some(TASK_CANCELLED.to_string()));
                }
                let (updated_config, reply) =
                    apply_pending_config_edit(config, store, user_id, text, &pending)?;
                *self.config.write().await = updated_config;
                return Ok(Some(reply));
            }
            store.state_mut().pending_config_edits.remove(&user_id);
            store.save()?;
        }
        if !allow_echo(store, user_id, config.bot.echo_limit)? {
            return Ok(None);
        }
        let is_admin = is_admin(config, user_id);
        let is_user = is_admin
            || is_user(config, user_id)
            || store.state().granted_users.contains(&user_id)
            || temporary_invite_active(store, user_id);
        let invite_allowed = is_user || invite_group_allowed(config, message.chat.id);
        let invite_action = take_pending_invite_action(config, store, user_id, text).await?;
        if invite_state_changed || invite_action.changed_state() {
            store.save()?;
        }
        let command = match invite_action {
            PendingInviteAction::None => BotCommandRouter::parse_for_config(text, config),
            PendingInviteAction::Command(command) => command,
            PendingInviteAction::Rejected(message) => return Ok(Some(message)),
        };
        let app = KoipyApp::new(config.clone());
        let webhooks = WebhookClient::new(config.clone());
        let _ = webhooks
            .emit(
                WebhookEvent::OnMessage,
                serde_json::json!({
                    "chat_id": message.chat.id,
                    "user_id": user_id,
                    "text": text,
                }),
            )
            .await?;

        match command {
            BotCommand::Cancel => {
                let changed = cancel_user_pending(store, user_id);
                if changed {
                    store.save()?;
                    Ok(Some(TASK_CANCELLED.to_string()))
                } else {
                    Ok(Some(OPERATION_TIMEOUT.to_string()))
                }
            }
            BotCommand::Help => Ok(Some(BotCommandRouter::help_text(is_admin, is_user))),
            BotCommand::Version => Ok(Some(
                "koipy-rs 0.1.0, compatible target: koipy 1.0".to_string(),
            )),
            BotCommand::System if is_admin => Ok(Some(BotCommandRouter::system_info())),
            BotCommand::System => Ok(Some("Permission denied: admin only".to_string())),
            BotCommand::Restart if is_admin => Ok(Some(
                "Restart requested. External process supervisor should restart koipy-rs."
                    .to_string(),
            )),
            BotCommand::Kill if is_admin => {
                self.api
                    .send_message(
                        message.chat.id,
                        "Shutting down koipy-rs",
                        &SendOptions::from_config(config),
                    )
                    .await?;
                std::process::exit(0);
            }
            BotCommand::Test {
                kind,
                command_token,
                rule_name,
                payload,
            } if is_user => {
                let request = self.build_task_request(
                    config,
                    store,
                    user_id,
                    kind,
                    &command_token,
                    rule_name.as_deref(),
                    &payload,
                )?;
                if request.requested_slave_ids().is_empty() && config.visible_slaves().len() > 1 {
                    let key = format!("{}:{}", message.chat.id, message.message_id);
                    store.state_mut().pending_tasks.insert(key.clone(), request);
                    store
                        .state_mut()
                        .pending_task_owners
                        .insert(key.clone(), user_id);
                    store.save()?;
                    self.api
                        .send_message_markup(
                            message.chat.id,
                            "Select a slave backend",
                            slave_keyboard(config, &key),
                            &SendOptions::from_config(config),
                        )
                        .await?;
                    return Ok(None);
                }
                if request.sort.is_none() {
                    let key = format!("{}:{}", message.chat.id, message.message_id);
                    store.state_mut().pending_tasks.insert(key.clone(), request);
                    store
                        .state_mut()
                        .pending_task_owners
                        .insert(key.clone(), user_id);
                    store.save()?;
                    self.api
                        .send_message_markup(
                            message.chat.id,
                            "Select result sorting",
                            sort_keyboard(config, &key),
                            &SendOptions::from_config(config),
                        )
                        .await?;
                    return Ok(None);
                }
                store
                    .state_mut()
                    .last_tasks
                    .insert(user_id, request.clone());
                store.save()?;
                let realtime_message = if request.realtime {
                    let realtime_start = progress_message_text(config, "realtime2");
                    Some(
                        self.api
                            .send_message(
                                message.chat.id,
                                &realtime_start,
                                &SendOptions::from_config(config),
                            )
                            .await?,
                    )
                } else {
                    None
                };
                let executed = if let Some(progress_message) = realtime_message {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MiaoSpeedProgress>();
                    let api = self.api.clone();
                    let send_options = SendOptions::from_config(config);
                    let chat_id = message.chat.id;
                    let slave_name = realtime_slave_name(config, &request);
                    let progress_slave = progress_message_text(config, "progress-4");
                    let progress_queue = progress_message_text(config, "progress-5");
                    let progress_progress = progress_message_text(config, "progress-6");
                    let updater = tokio::spawn(async move {
                        let mut last_count = 0;
                        while let Some(progress) = rx.recv().await {
                            if progress.should_emit(last_count) {
                                last_count = progress.count;
                                let _ = api
                                    .edit_message_text(
                                        chat_id,
                                        progress_message.message_id,
                                        &progress.render_text(
                                            &slave_name,
                                            &progress_slave,
                                            &progress_queue,
                                            &progress_progress,
                                        ),
                                        None,
                                        &send_options,
                                    )
                                    .await;
                            }
                        }
                    });
                    let executed = app
                        .execute_task_with_progress(request.clone(), move |progress| {
                            let _ = tx.send(progress);
                        })
                        .await?;
                    updater.abort();
                    let realtime_done = progress_message_text(config, "realtime");
                    let _ = self
                        .api
                        .edit_message_text(
                            message.chat.id,
                            progress_message.message_id,
                            &realtime_done,
                            None,
                            &SendOptions::from_config(config),
                        )
                        .await;
                    executed
                } else {
                    app.execute_task(request.clone()).await?
                };
                let result_hook = webhooks
                    .emit(
                        WebhookEvent::OnResult,
                        serde_json::json!({
                            "chat_id": message.chat.id,
                            "user_id": user_id,
                            "summary": executed.summary(),
                        }),
                    )
                    .await?;
                let mut caption = executed.summary();
                if let Some(text) = result_hook.append_text {
                    if !text.trim().is_empty() {
                        caption.push('\n');
                        caption.push_str(text.trim());
                    }
                }
                let output = store
                    .state()
                    .last_tasks
                    .get(&user_id)
                    .map(|request| request.output)
                    .unwrap_or_default();
                match output {
                    OutputMode::Json => {
                        let mut json = serde_json::to_value(&executed.table)?;
                        if let Some(extra) = result_hook.merge_json {
                            merge_json(&mut json, extra);
                        }
                        let rendered = ResultRenderer::new(config.clone())
                            .render_json_snapshot(&json, "results")?;
                        self.api
                            .send_document(
                                message.chat.id,
                                &rendered.path,
                                &caption,
                                "application/json",
                                &SendOptions::from_config(config),
                            )
                            .await?;
                        cleanup_rendered(config, &rendered.path)?;
                    }
                    OutputMode::Image => {
                        let rendered = ResultRenderer::new(config.clone())
                            .render_table_with_context(
                                &executed.table,
                                "results",
                                RenderContext {
                                    uid: Some(user_id),
                                    slave: executed.slaves.first().cloned(),
                                },
                            )?;
                        let _ = webhooks
                            .emit(
                                WebhookEvent::OnPreSend,
                                serde_json::json!({
                                    "chat_id": message.chat.id,
                                    "path": rendered.path,
                                    "width": rendered.width,
                                    "height": rendered.height,
                                }),
                            )
                            .await?;
                        self.send_rendered_image(config, message.chat.id, &rendered, &caption)
                            .await?;
                    }
                    OutputMode::Video => {
                        let rendered = ResultRenderer::new(config.clone())
                            .render_video_or_fallback_with_context(
                                &executed.table,
                                "results",
                                RenderContext {
                                    uid: Some(user_id),
                                    slave: executed.slaves.first().cloned(),
                                },
                            )?;
                        let media_path = rendered.path().to_path_buf();
                        let _ = webhooks
                            .emit(
                                WebhookEvent::OnPreSend,
                                serde_json::json!({
                                    "chat_id": message.chat.id,
                                    "path": media_path,
                                    "video": rendered.is_video(),
                                }),
                            )
                            .await?;
                        self.send_rendered_video_or_fallback(
                            config,
                            message.chat.id,
                            &rendered,
                            &caption,
                        )
                        .await?;
                    }
                }
                Ok(None)
            }
            BotCommand::Test { .. } => Ok(Some("Permission denied: user only".to_string())),
            BotCommand::Re { payload } if is_user => {
                let mut request = store
                    .state()
                    .last_tasks
                    .get(&user_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("no previous task for /re"))?;
                if !payload.trim().is_empty() {
                    request.raw_target = self.resolve_target(store, user_id, &payload)?;
                }
                let executed = app.execute_task(request).await?;
                let rendered = ResultRenderer::new(config.clone()).render_table_with_context(
                    &executed.table,
                    "results",
                    RenderContext {
                        uid: Some(user_id),
                        slave: executed.slaves.first().cloned(),
                    },
                )?;
                self.send_rendered_image(
                    config,
                    message.chat.id,
                    &rendered,
                    &format!("Retest complete\n{}", executed.summary()),
                )
                .await?;
                Ok(None)
            }
            BotCommand::NewSubscription {
                url,
                name,
                password,
            } if is_user => {
                ensure_invite_target_allowed(config, Some(&url)).await?;
                let parsed = parse_subscription_url(&url, &config.subconverter)
                    .ok_or_else(|| anyhow::anyhow!("invalid subscription URL"))?;
                store.state_mut().subscriptions.insert(
                    name.clone(),
                    SubscriptionRecord::new(name.clone(), parsed, password, user_id),
                );
                store.save()?;
                Ok(Some(format!("Subscription saved: {name}")))
            }
            BotCommand::NewRule { url, name } if is_user => {
                let name = normalize_rule_name(&name, message.message_id)?;
                ensure_invite_target_allowed(config, Some(&url)).await?;
                let parsed = parse_subscription_url(&url, &config.subconverter)
                    .ok_or_else(|| anyhow::anyhow!("invalid rule URL"))?;
                store
                    .state_mut()
                    .rules
                    .insert(name.clone(), RuleRecord::new(name.clone(), parsed, user_id));
                store.save()?;
                Ok(Some(format!("Rule saved: {name}")))
            }
            BotCommand::Rule { action, name } if is_user => {
                let reply = match action.as_str() {
                    "list" | "" => {
                        let saved: Vec<_> = store
                            .state()
                            .rules
                            .values()
                            .filter(|rule| rule.can_access(user_id) || is_admin)
                            .map(|rule| rule.name.clone())
                            .collect();
                        let preset: Vec<_> = config
                            .rules
                            .iter()
                            .filter(|rule| rule.enable)
                            .map(|rule| rule.name.clone())
                            .collect();
                        format!(
                            "Rules: {}\n\n🎨Available preset rules (may be overridden):\n{}",
                            display_rule_names(&saved),
                            display_rule_names(&preset)
                        )
                    }
                    "show" => rule_detail_text(config, store, &name, user_id, is_admin)?,
                    "delete" | "remove" if is_admin => {
                        let removed = store.state_mut().rules.remove(&name).is_some();
                        store.save()?;
                        format!("Rule removed: {removed}")
                    }
                    _ => "Usage: /rule list | /rule show <name> | /rule delete <name>".to_string(),
                };
                Ok(Some(reply))
            }
            BotCommand::ShowSubscription { name } if is_user => {
                if name.is_empty() {
                    let visible: Vec<_> = store
                        .state()
                        .subscriptions
                        .values()
                        .filter(|record| record.can_access(user_id))
                        .map(|record| record.name.clone())
                        .collect();
                    return Ok(Some(format!("Subscriptions: {}", visible.join(", "))));
                }
                let record = store
                    .state()
                    .subscriptions
                    .get(&name)
                    .filter(|record| record.can_access(user_id))
                    .ok_or_else(|| anyhow::anyhow!("subscription not found or inaccessible"))?;
                let traffic = SubscriptionCollector::new(config)?
                    .fetch_traffic(&record.url)
                    .await?;
                let traffic_ref = traffic.as_ref();
                let traffic_text = traffic_ref
                    .map(|traffic| traffic.summary())
                    .unwrap_or_else(|| "Traffic header not found".to_string());
                Ok(Some(subscription_info_text(
                    &record.name,
                    &record.url,
                    Some(record.created_at),
                    traffic_ref,
                    &traffic_text,
                )))
            }
            BotCommand::ShowSubscription { name } => {
                let parsed = tourist_subinfo_target(config, &name)?;
                let traffic = SubscriptionCollector::new(config)?
                    .fetch_traffic(&parsed)
                    .await?;
                let traffic_ref = traffic.as_ref();
                let traffic_text = traffic_ref
                    .map(|traffic| traffic.summary())
                    .unwrap_or_else(|| "Traffic header not found".to_string());
                Ok(Some(subscription_info_text(
                    &site_name(&parsed),
                    &parsed,
                    None,
                    traffic_ref,
                    &traffic_text,
                )))
            }
            BotCommand::RemoveSubscription { names } if is_admin => {
                let mut removed = 0;
                for name in names {
                    if store.state_mut().subscriptions.remove(&name).is_some() {
                        removed += 1;
                    }
                }
                store.save()?;
                Ok(Some(format!("Removed subscriptions: {removed}")))
            }
            BotCommand::CheckSlaves if is_user => {
                check_slaves_report(config, &self.checkslaves_lock).await
            }
            BotCommand::Invite if invite_allowed => {
                ensure_invite_target_allowed(config, None).await?;
                let expires = Utc::now() + Duration::minutes(30);
                store.state_mut().temporary_invites.insert(user_id, expires);
                store.save()?;
                self.api
                    .send_message_markup(
                        message.chat.id,
                        &format!(
                            "Temporary invite granted until {expires}\nCreated a test task, choose type:"
                        ),
                        invite_keyboard(config),
                        &SendOptions::from_config(config),
                    )
                    .await?;
                Ok(None)
            }
            BotCommand::Share { name, target } if is_user => {
                let record = store
                    .state_mut()
                    .subscriptions
                    .get_mut(&name)
                    .ok_or_else(|| anyhow::anyhow!("subscription not found"))?;
                if record.owner != user_id && !is_admin {
                    bail!("only owner or admin can share subscription");
                }
                if !record.shared_with.contains(&target) {
                    record.shared_with.push(target);
                }
                store.save()?;
                Ok(Some(format!("Subscription {name} shared with {target}")))
            }
            BotCommand::Reload if is_admin => {
                let reloaded = reload_config_from_source(config)?;
                *self.config.write().await = reloaded;
                Ok(Some("Configuration reloaded".to_string()))
            }
            BotCommand::GetConfig { path } if is_admin => {
                let reloaded = reload_config_from_source(config)?;
                let value = serde_yaml::to_value(&reloaded)?;
                let path = ConfigPath::parse(&path)?;
                let selected = config_path_get(&value, &path)
                    .ok_or_else(|| anyhow::anyhow!("config path not found"))?;
                Ok(Some(render_config_value(selected)?))
            }
            BotCommand::SetConfig { path, value } if is_admin => {
                if value.trim().is_empty() {
                    store.state_mut().pending_config_edits.insert(
                        user_id,
                        crate::state::PendingConfigEdit::new(
                            path.clone(),
                            message.chat.id,
                            message.message_id,
                            Utc::now() + Duration::seconds(60),
                        ),
                    );
                    store.save()?;
                    Ok(Some(
                        "Command to set config:\n⏳ Input new value (60s), /cancel to stop"
                            .to_string(),
                    ))
                } else {
                    let updated = apply_config_update(config, &path, &value)?;
                    let reloaded = reload_config_from_source(&updated)?;
                    *self.config.write().await = reloaded;
                    Ok(Some(format!("Config updated: {path}")))
                }
            }
            BotCommand::DeleteConfig { path } if is_admin => {
                let mut reloaded = reload_config_from_source(config)?;
                let mut yaml = serde_yaml::to_value(&reloaded)?;
                let path = ConfigPath::parse(&path)?;
                config_path_delete(&mut yaml, &path)?;
                reloaded = serde_yaml::from_value(yaml)?;
                reloaded.source_path = config.source_path.clone();
                reloaded.save_to_source()?;
                let updated = reload_config_from_source(&reloaded)?;
                *self.config.write().await = updated;
                Ok(Some(format!("Config deleted: {}", path.render())))
            }
            BotCommand::SetAntiGroup if is_admin => {
                store.state_mut().anti_group = !store.state().anti_group;
                let status = store.state().anti_group;
                store.save()?;
                Ok(Some(format!("Anti-group mode: {status}")))
            }
            BotCommand::Panel if is_admin => {
                self.api
                    .send_message_markup(
                        message.chat.id,
                        &panel_text(config, store),
                        panel_keyboard(),
                        &SendOptions::from_config(config),
                    )
                    .await?;
                Ok(None)
            }
            BotCommand::Demo if is_user => {
                self.api
                    .send_message_markup(
                        message.chat.id,
                        &demo_text(),
                        demo_keyboard(config),
                        &SendOptions::from_config(config),
                    )
                    .await?;
                Ok(None)
            }
            BotCommand::License { target } if is_admin => {
                Ok(Some(license_info_text(config, user_id, target.as_deref())))
            }
            BotCommand::Logs { tail } if is_admin => Ok(Some(read_log_tail(tail.unwrap_or(100))?)),
            BotCommand::User if is_admin => Ok(Some(user_text(config, store))),
            BotCommand::SetCommands if is_admin => {
                let commands = pinned_bot_commands(config);
                if commands.is_empty() {
                    return Ok(Some(config.translation_value("setcmd3").unwrap_or_else(
                        || "Command not enabled, please check config or bypass mode.".to_string(),
                    )));
                }
                self.api.set_my_commands(&commands).await?;
                Ok(Some(format!(
                    "Custom commands set to TG:\n{}",
                    bot_commands_text(&commands)
                )))
            }
            BotCommand::Language { lang } if is_admin => {
                if let Some(lang) = lang.filter(|value| !value.trim().is_empty()) {
                    let mut reloaded = reload_config_from_source(config)?;
                    switch_translation_language(&mut reloaded, &lang)?;
                    reloaded.save_to_source()?;
                    *self.config.write().await = reloaded;
                    Ok(Some(format!("Language has been switched to {lang}")))
                } else {
                    Ok(Some(format!(
                        "Current language: {}",
                        config.translation.lang
                    )))
                }
            }
            BotCommand::Grant { user_id: target } if is_admin => {
                let target = authorization_target_user_id(target, message)?;
                if !store.state().granted_users.contains(&target) {
                    store.state_mut().granted_users.push(target);
                    store.save()?;
                }
                Ok(Some(format!("Granted user: {target}")))
            }
            BotCommand::UnGrant { user_id: target } if is_admin => {
                let target = authorization_target_user_id(target, message)?;
                let mut reloaded = reload_config_from_source(config)?;
                let changed_config = reloaded.revoke_user(target);
                if changed_config {
                    reloaded.save_to_source()?;
                    *self.config.write().await = reloaded;
                }
                let before = store.state().granted_users.len();
                store
                    .state_mut()
                    .granted_users
                    .retain(|user_id| *user_id != target);
                let changed_state = before != store.state().granted_users.len();
                if changed_state {
                    store.save()?;
                }
                Ok(Some(format!(
                    "Revoked user: {target} (config: {changed_config}, runtime: {changed_state})"
                )))
            }
            BotCommand::Leave if is_admin => {
                self.api.leave_chat(message.chat.id).await?;
                Ok(None)
            }
            BotCommand::NightShift if is_admin => {
                store.state_mut().night_shift = !store.state().night_shift;
                let status = store.state().night_shift;
                store.save()?;
                Ok(Some(format!("Night shift: {status}")))
            }
            BotCommand::Disabled(name) => Ok(Some(disabled_command_message(config, &name))),
            BotCommand::Unknown(name) if config.bot.bypass_mode => {
                Ok(Some(bypass_mode_message(config, &name)))
            }
            BotCommand::Unknown(name) => Ok(Some(format!("Unknown command: {name}"))),
            _ => Ok(Some("Permission denied".to_string())),
        }
    }

    async fn handle_callback(
        &self,
        store: &mut StateStore,
        callback: TelegramCallbackQuery,
    ) -> Result<()> {
        let config = self.config.read().await.clone();
        let user_id = callback.from.id;
        let is_admin = is_admin(&config, user_id);
        let data = callback.data.unwrap_or_default();
        let Some(message) = callback.message else {
            self.api
                .answer_callback_query(&callback.id, "Missing message")
                .await?;
            return Ok(());
        };
        match data.as_str() {
            "panel:anti" if is_admin => {
                store.state_mut().anti_group = !store.state().anti_group;
                store.save()?;
                self.api
                    .edit_message_text(
                        message.chat.id,
                        message.message_id,
                        &panel_text(&config, store),
                        Some(panel_keyboard()),
                        &SendOptions::from_config(&config),
                    )
                    .await?;
                self.api
                    .answer_callback_query(&callback.id, "anti-group toggled")
                    .await?;
            }
            "panel:night" if is_admin => {
                store.state_mut().night_shift = !store.state().night_shift;
                store.save()?;
                self.api
                    .edit_message_text(
                        message.chat.id,
                        message.message_id,
                        &panel_text(&config, store),
                        Some(panel_keyboard()),
                        &SendOptions::from_config(&config),
                    )
                    .await?;
                self.api
                    .answer_callback_query(&callback.id, "night-shift toggled")
                    .await?;
            }
            "panel:slaves" if is_admin => {
                match check_slaves_report(&config, &self.checkslaves_lock).await? {
                    Some(text) if text == "❌Other user checking slaves, please wait..." => {
                        self.api
                            .answer_callback_query(
                                &callback.id,
                                "❌Other user checking slaves, please wait...",
                            )
                            .await?;
                    }
                    Some(text) => {
                        self.api
                            .edit_message_text(
                                message.chat.id,
                                message.message_id,
                                &text,
                                Some(panel_keyboard()),
                                &SendOptions::from_config(&config),
                            )
                            .await?;
                        self.api
                            .answer_callback_query(&callback.id, "checked")
                            .await?;
                    }
                    None => {}
                }
            }
            "panel:close" if is_admin => {
                self.api
                    .delete_message(message.chat.id, message.message_id)
                    .await?;
                self.api
                    .answer_callback_query(&callback.id, "closed")
                    .await?;
            }
            "demo:image" => {
                let generating = localized_text(&config, "demo3", "Generating...");
                self.api
                    .answer_callback_query(&callback.id, &generating)
                    .await?;
                let rendered = ResultRenderer::new(config.clone()).render_table_with_context(
                    &demo_result_table(),
                    "results",
                    RenderContext {
                        uid: Some(user_id),
                        slave: config.slave_config.slaves.first().cloned(),
                    },
                )?;
                self.send_rendered_image(
                    &config,
                    message.chat.id,
                    &rendered,
                    "Drawing demo uses current image configuration for preview.",
                )
                .await?;
                cleanup_rendered(&config, &rendered.path)?;
            }
            data if data.starts_with("invite:rule:") => {
                let rule = data.strip_prefix("invite:rule:").unwrap_or_default();
                store.state_mut().pending_invites.insert(
                    user_id,
                    PendingInvite::new(
                        rule.to_string(),
                        message.chat.id,
                        message.message_id,
                        Utc::now() + Duration::seconds(60),
                    ),
                );
                store.save()?;
                let waiting = localized_text(&config, "invite-10", "Waiting for subscription link");
                self.api
                    .answer_callback_query(&callback.id, &waiting)
                    .await?;
                self.api
                    .edit_message_text(
                        message.chat.id,
                        message.message_id,
                        &format!("Invite rule selected: {rule}\nPlease send sub link in 60s."),
                        None,
                        &SendOptions::from_config(&config),
                    )
                    .await?;
            }
            data if data.starts_with("task:cancel:") => {
                let key = data.strip_prefix("task:cancel:").unwrap_or_default();
                if !strict_callback_allowed(&config, store, key, user_id) {
                    let denied = localized_text(&config, "realtime3", "Permission denied");
                    self.api
                        .answer_callback_query(&callback.id, &denied)
                        .await?;
                    return Ok(());
                }
                if cancel_pending_task(store, key) {
                    store.save()?;
                    self.api
                        .edit_message_text(
                            message.chat.id,
                            message.message_id,
                            TASK_CANCELLED,
                            None,
                            &SendOptions::from_config(&config),
                        )
                        .await?;
                    self.api
                        .answer_callback_query(&callback.id, TASK_CANCELLED)
                        .await?;
                } else {
                    self.api
                        .answer_callback_query(&callback.id, OPERATION_TIMEOUT)
                        .await?;
                }
            }
            data if data.starts_with("task:slave:") => {
                let Some((key, slave_id)) = data
                    .strip_prefix("task:slave:")
                    .and_then(|rest| rest.rsplit_once(':'))
                else {
                    let bad_callback = localized_text(&config, "error-8", "Bad callback");
                    self.api
                        .answer_callback_query(&callback.id, &bad_callback)
                        .await?;
                    return Ok(());
                };
                if !strict_callback_allowed(&config, store, key, user_id) {
                    let denied = localized_text(&config, "realtime3", "Permission denied");
                    self.api
                        .answer_callback_query(&callback.id, &denied)
                        .await?;
                    return Ok(());
                }
                let Some(mut request) = store.state_mut().pending_tasks.remove(key) else {
                    reclaim_pending_task(store, key);
                    self.api
                        .answer_callback_query(&callback.id, SLAVE_SELECTOR_TIMEOUT)
                        .await?;
                    return Ok(());
                };
                request.set_slave_ids(vec![slave_id.to_string()]);
                store
                    .state_mut()
                    .pending_tasks
                    .insert(key.to_string(), request);
                store.save()?;
                self.api
                    .edit_message_text(
                        message.chat.id,
                        message.message_id,
                        &localized_text(&config, "sort-select", "Select result sorting"),
                        Some(sort_keyboard(&config, key)),
                        &SendOptions::from_config(&config),
                    )
                    .await?;
                self.api
                    .answer_callback_query(&callback.id, "slave selected")
                    .await?;
            }
            data if data.starts_with("task:sort:") => {
                let Some((key, sort_name)) = data
                    .strip_prefix("task:sort:")
                    .and_then(|rest| rest.rsplit_once(':'))
                else {
                    let bad_callback = localized_text(&config, "error-8", "Bad callback");
                    self.api
                        .answer_callback_query(&callback.id, &bad_callback)
                        .await?;
                    return Ok(());
                };
                if !strict_callback_allowed(&config, store, key, user_id) {
                    let denied = localized_text(&config, "realtime3", "Permission denied");
                    self.api
                        .answer_callback_query(&callback.id, &denied)
                        .await?;
                    return Ok(());
                }
                let Some(mut request) = store.state_mut().pending_tasks.remove(key) else {
                    reclaim_pending_task(store, key);
                    self.api
                        .answer_callback_query(&callback.id, SORT_SELECTOR_TIMEOUT)
                        .await?;
                    return Ok(());
                };
                request.sort = crate::config::SortType::parse_text(sort_name);
                store
                    .state_mut()
                    .pending_tasks
                    .insert(key.to_string(), request);
                let all_scripts = script_names(&config);
                store.state_mut().pending_script_selections.insert(
                    key.to_string(),
                    crate::state::ScriptSelection {
                        selected: all_scripts,
                        page: 0,
                    },
                );
                store.save()?;
                self.api
                    .edit_message_text(
                        message.chat.id,
                        message.message_id,
                        &localized_text(&config, "script-select", "Select script set"),
                        Some(script_keyboard(key, &config, store, 0)),
                        &SendOptions::from_config(&config),
                    )
                    .await?;
                self.api
                    .answer_callback_query(&callback.id, "sort selected")
                    .await?;
            }
            data if data.starts_with("task:scripts:") => {
                let Some((key, action)) = data
                    .strip_prefix("task:scripts:")
                    .and_then(|rest| rest.rsplit_once(':'))
                else {
                    let bad_callback = localized_text(&config, "error-8", "Bad callback");
                    self.api
                        .answer_callback_query(&callback.id, &bad_callback)
                        .await?;
                    return Ok(());
                };
                if !strict_callback_allowed(&config, store, key, user_id) {
                    let denied = localized_text(&config, "realtime3", "Permission denied");
                    self.api
                        .answer_callback_query(&callback.id, &denied)
                        .await?;
                    return Ok(());
                }
                if action == "ok" {
                    let Some(mut request) = store.state_mut().pending_tasks.remove(key) else {
                        reclaim_pending_task(store, key);
                        self.api
                            .answer_callback_query(&callback.id, QUERY_NOT_FOUND)
                            .await?;
                        return Ok(());
                    };
                    let selection = store
                        .state_mut()
                        .pending_script_selections
                        .remove(key)
                        .unwrap_or_default();
                    store.state_mut().pending_task_owners.remove(key);
                    request.selected_scripts = selection.selected;
                    store
                        .state_mut()
                        .last_tasks
                        .insert(user_id, request.clone());
                    store.save()?;
                    self.api
                        .edit_message_text(
                            message.chat.id,
                            message.message_id,
                            &localized_text(&config, "script-ok", "Running task..."),
                            None,
                            &SendOptions::from_config(&config),
                        )
                        .await?;
                    let executed = KoipyApp::new(config.clone()).execute_task(request).await?;
                    let rendered = ResultRenderer::new(config.clone()).render_table_with_context(
                        &executed.table,
                        "results",
                        RenderContext {
                            uid: Some(user_id),
                            slave: executed.slaves.first().cloned(),
                        },
                    )?;
                    self.send_rendered_image(
                        &config,
                        message.chat.id,
                        &rendered,
                        &executed.summary(),
                    )
                    .await?;
                    self.api.answer_callback_query(&callback.id, "done").await?;
                    return Ok(());
                }
                if !store.state().pending_tasks.contains_key(key) {
                    reclaim_pending_task(store, key);
                    self.api
                        .answer_callback_query(&callback.id, QUERY_NOT_FOUND)
                        .await?;
                    return Ok(());
                }
                let script_names = script_names(&config);
                {
                    let selection = store
                        .state_mut()
                        .pending_script_selections
                        .entry(key.to_string())
                        .or_default();
                    match action {
                        "all" => selection.selected = script_names.clone(),
                        "none" => selection.selected.clear(),
                        "reverse" => {
                            selection.selected = script_names
                                .iter()
                                .filter(|name| !selection.contains(name))
                                .cloned()
                                .collect();
                        }
                        "next" => {
                            let max_page = script_names.len().saturating_sub(1) / SCRIPT_PAGE_SIZE;
                            selection.page = (selection.page + 1).min(max_page);
                        }
                        "prev" => {
                            selection.page = selection.page.saturating_sub(1);
                        }
                        "noop" => {}
                        script_name => selection.toggle(script_name),
                    }
                }
                store.save()?;
                let page = store
                    .state()
                    .pending_script_selections
                    .get(key)
                    .map(|selection| selection.page)
                    .unwrap_or_default();
                self.api
                    .edit_message_text(
                        message.chat.id,
                        message.message_id,
                        &localized_text(&config, "script-select", "Select scripts"),
                        Some(script_keyboard(key, &config, store, page)),
                        &SendOptions::from_config(&config),
                    )
                    .await?;
                self.api
                    .answer_callback_query(&callback.id, "updated")
                    .await?;
            }
            _ => {
                let answer = if known_callback_namespace(&data) {
                    localized_text(&config, "realtime3", "Permission denied")
                } else {
                    config
                        .translation_value("unknown-callback")
                        .unwrap_or_else(|| UNKNOWN_CALLBACK.to_string())
                };
                self.api
                    .answer_callback_query(&callback.id, &answer)
                    .await?;
            }
        }
        Ok(())
    }

    fn resolve_target(&self, store: &StateStore, user_id: i64, payload: &str) -> Result<String> {
        let first = payload.split_whitespace().next().unwrap_or_default();
        if let Some(record) = store
            .state()
            .subscriptions
            .get(first)
            .filter(|record| record.can_access(user_id))
        {
            Ok(record.url.clone())
        } else if let Some(rule) = store
            .state()
            .rules
            .get(first)
            .filter(|rule| rule.can_access(user_id))
        {
            Ok(rule.url.clone())
        } else {
            Ok(payload.to_string())
        }
    }

    fn build_task_request(
        &self,
        config: &KoipyConfig,
        store: &StateStore,
        user_id: i64,
        kind: TaskKind,
        command_token: &str,
        rule_name: Option<&str>,
        payload: &str,
    ) -> Result<TaskRequest> {
        let rule = rule_name.and_then(|name| config_rule_by_name(config, name));
        let target = if payload.trim().is_empty() {
            rule.and_then(|rule| {
                if rule.url.trim().is_empty() {
                    None
                } else {
                    Some(rule.url.clone())
                }
            })
            .ok_or_else(|| anyhow::anyhow!("missing subscription URL or rule URL"))?
        } else {
            self.resolve_target(store, user_id, payload)?
        };

        let mut request = TaskRequest::new_url(kind, target);
        apply_runtime_defaults(&mut request, &config.runtime);
        if let Some(rule) = rule {
            apply_config_rule(&mut request, rule);
        }
        Ok(request.apply_command_options(command_token))
    }

    async fn send_rendered_image(
        &self,
        config: &KoipyConfig,
        chat_id: i64,
        rendered: &crate::image::RenderedResult,
        caption: &str,
    ) -> Result<()> {
        if should_send_as_photo(
            rendered.width,
            rendered.height,
            &config.image.pixel_threshold,
        ) {
            self.api
                .send_photo(
                    chat_id,
                    &rendered.path,
                    caption,
                    &SendOptions::from_config(config),
                )
                .await?;
        } else {
            self.api
                .send_document(
                    chat_id,
                    &rendered.path,
                    caption,
                    "image/png",
                    &SendOptions::from_config(config),
                )
                .await?;
        }
        cleanup_rendered(config, &rendered.path)
    }

    async fn send_rendered_video_or_fallback(
        &self,
        config: &KoipyConfig,
        chat_id: i64,
        rendered: &RenderedMedia,
        caption: &str,
    ) -> Result<()> {
        match rendered {
            RenderedMedia::Video {
                video,
                source_image,
            } => {
                self.api
                    .send_video(
                        chat_id,
                        &video.path,
                        caption,
                        &SendOptions::from_config(config),
                    )
                    .await?;
                cleanup_rendered(config, &video.path)?;
                cleanup_rendered(config, &source_image.path)
            }
            RenderedMedia::Image(image) => {
                self.send_rendered_image(config, chat_id, image, caption)
                    .await
            }
            RenderedMedia::FallbackImage { image, reason } => {
                let caption = format!("{caption}\nVideo fallback: {reason}");
                self.send_rendered_image(config, chat_id, image, &caption)
                    .await
            }
        }
    }
}

fn merge_json(target: &mut serde_json::Value, extra: serde_json::Value) {
    match (target, extra) {
        (serde_json::Value::Object(target), serde_json::Value::Object(extra)) => {
            for (key, value) in extra {
                target.insert(key, value);
            }
        }
        (target, extra) => *target = extra,
    }
}

fn realtime_slave_name(config: &KoipyConfig, request: &TaskRequest) -> String {
    let names: Vec<_> = request
        .requested_slave_ids()
        .iter()
        .filter_map(|id| {
            config
                .slave_config
                .slaves
                .iter()
                .find(|slave| slave.id == *id || slave.comment == *id)
        })
        .map(|slave| slave_display_name(config, slave))
        .collect();
    if !names.is_empty() {
        return names.join(", ");
    }
    config
        .slave_config
        .slaves
        .iter()
        .find(|slave| {
            !slave.hidden
                && !config.slave_config.default.is_empty()
                && (slave.id == config.slave_config.default
                    || slave.comment == config.slave_config.default)
        })
        .or_else(|| {
            config
                .slave_config
                .slaves
                .iter()
                .find(|slave| !slave.hidden)
        })
        .map(|slave| slave_display_name(config, slave))
        .unwrap_or_else(|| "default".to_string())
}

fn progress_message_text(config: &KoipyConfig, key: &str) -> String {
    config.translation_value(key).unwrap_or_else(|| match key {
        "realtime2" => "✔️Real-time Rendering".to_string(),
        "realtime" => "❌Real-time Rendering".to_string(),
        "progress-4" => "⚙️Slave:".to_string(),
        "progress-5" => "🎉Queue size:".to_string(),
        "progress-6" => "Progress:".to_string(),
        _ => key.to_string(),
    })
}

fn localized_text(config: &KoipyConfig, key: &str, fallback: &str) -> String {
    config
        .translation_value(key)
        .unwrap_or_else(|| fallback.to_string())
}

fn localized_template(config: &KoipyConfig, key: &str, fallback: &str, args: &[&str]) -> String {
    let mut text = localized_text(config, key, fallback);
    for arg in args {
        text = text.replacen("{}", arg, 1);
    }
    text
}

fn disabled_command_message(config: &KoipyConfig, name: &str) -> String {
    localized_template(
        config,
        "command-disabled",
        "`{}` command has been disabled",
        &[name],
    )
}

fn bypass_mode_message(config: &KoipyConfig, name: &str) -> String {
    config.translation_value("bypass").unwrap_or_else(|| {
        format!(
            "Bypass mode enabled. Built-in commands are disabled; only configured custom commands are available. ({name})"
        )
    })
}

fn slave_display_name(config: &KoipyConfig, slave: &crate::config::SlaveConfigEntry) -> String {
    match (slave.comment.trim().is_empty(), config.slave_config.show_id) {
        (true, _) => slave.id.clone(),
        (false, true) => format!("{}({})", slave.comment, slave.id),
        (false, false) => slave.comment.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlaveCheckStatus {
    Alive,
    Offline,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlaveCheckReport {
    id: String,
    address: String,
    kind: &'static str,
    hidden: bool,
    status: SlaveCheckStatus,
}

impl SlaveCheckReport {
    fn from_slave(slave: &crate::config::SlaveConfigEntry, status: SlaveCheckStatus) -> Self {
        Self {
            id: slave.id.clone(),
            address: slave.address.clone(),
            kind: slave_kind_label(&slave.r#type),
            hidden: slave.hidden,
            status,
        }
    }
}

fn slave_kind_label(kind: &SlaveType) -> &'static str {
    match kind {
        SlaveType::MiaoSpeed => "miaospeed",
        SlaveType::FullTclash => "fulltclash",
        SlaveType::Websocket => "websocket",
        SlaveType::Bot => "bot",
    }
}

fn slave_check_report_text(reports: &[SlaveCheckReport]) -> String {
    let mut lines = vec!["Slave Connectivity Test".to_string()];
    let alive = reports
        .iter()
        .filter(|report| report.status == SlaveCheckStatus::Alive)
        .count();
    let offline = reports
        .iter()
        .filter(|report| report.status == SlaveCheckStatus::Offline)
        .count();
    lines.push(format!("✅Alive Slaves {alive}"));
    lines.push(format!("❌Offline Slaves {offline}"));
    if reports.is_empty() {
        lines.push("❌No backends configured, cannot start task".to_string());
        return lines.join("\n");
    }
    for report in reports {
        let status = match report.status {
            SlaveCheckStatus::Alive => "online",
            SlaveCheckStatus::Offline => "offline",
            SlaveCheckStatus::Skipped => "not-pinged",
        };
        let hidden = if report.hidden { "hidden" } else { "visible" };
        lines.push(format!(
            "‣{} [{}] {} {} {}",
            report.id, report.kind, report.address, hidden, status
        ));
    }
    lines.join("\n")
}

async fn check_slaves_report(
    config: &KoipyConfig,
    lock: &Arc<Mutex<()>>,
) -> Result<Option<String>> {
    let Ok(_guard) = lock.try_lock() else {
        return Ok(Some(
            "❌Other user checking slaves, please wait...".to_string(),
        ));
    };
    let mut reports = Vec::new();
    for slave in &config.slave_config.slaves {
        let status = if matches!(slave.r#type, SlaveType::MiaoSpeed) {
            if ping_slave(slave).await {
                SlaveCheckStatus::Alive
            } else {
                SlaveCheckStatus::Offline
            }
        } else {
            SlaveCheckStatus::Skipped
        };
        reports.push(SlaveCheckReport::from_slave(slave, status));
    }
    Ok(Some(slave_check_report_text(&reports)))
}

fn apply_config_rule(request: &mut TaskRequest, rule: &RuleConfig) {
    let slave_ids: Vec<_> = rule
        .slaveid
        .iter()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .collect();
    if !slave_ids.is_empty() {
        request.set_slave_ids(slave_ids);
    }
    if !rule.scripts.is_empty() {
        request.selected_scripts = rule.scripts.clone();
    }
    if rule.sort != crate::config::SortType::Origin {
        request.sort = Some(rule.sort);
    }
    if let Some(runtime) = &rule.runtime {
        request.sort = Some(runtime.sort);
        if !runtime.include_filter.trim().is_empty() {
            request.include = runtime.include_filter.clone();
        }
        if !runtime.exclude_filter.trim().is_empty() {
            request.exclude = runtime.exclude_filter.clone();
        }
        if runtime.speed_threads > 0 {
            request.threading = Some(runtime.speed_threads as u64);
        }
    }
}

fn apply_runtime_defaults(request: &mut TaskRequest, runtime: &RuntimeConfig) {
    if runtime.speed_threads > 0 {
        request.threading = Some(runtime.speed_threads as u64);
    }
    if runtime.duration > 0 {
        request.duration = Some(runtime.duration);
    }
    if !runtime.include_filter.trim().is_empty() {
        request.include = runtime.include_filter.clone();
    }
    if !runtime.exclude_filter.trim().is_empty() {
        request.exclude = runtime.exclude_filter.clone();
    }
    if runtime.sort != crate::config::SortType::Origin {
        request.sort = Some(runtime.sort);
    }
    request.output = OutputMode::parse(&runtime.output);
    request.realtime = runtime.realtime;
    request.nocvt = runtime.disable_sub_cvt;
}

fn config_rule_by_name<'a>(config: &'a KoipyConfig, name: &str) -> Option<&'a RuleConfig> {
    config
        .rules
        .iter()
        .find(|rule| rule.name == name && rule.enable)
}

fn is_admin(config: &KoipyConfig, user_id: i64) -> bool {
    config
        .admin
        .iter()
        .any(|value| yaml_value_is_id(value, user_id))
}

fn is_user(config: &KoipyConfig, user_id: i64) -> bool {
    config
        .user
        .iter()
        .any(|value| yaml_value_is_id(value, user_id))
}

fn temporary_invite_active(store: &StateStore, user_id: i64) -> bool {
    store
        .state()
        .temporary_invites
        .get(&user_id)
        .is_some_and(|expires| *expires > Utc::now())
}

fn prune_expired_temporary_invites(store: &mut StateStore) -> bool {
    let now = Utc::now();
    let before = store.state().temporary_invites.len();
    store
        .state_mut()
        .temporary_invites
        .retain(|_, expires| *expires > now);
    before != store.state().temporary_invites.len()
}

fn prune_expired_pending_invites(store: &mut StateStore) -> bool {
    let now = Utc::now();
    let before = store.state().pending_invites.len();
    store
        .state_mut()
        .pending_invites
        .retain(|_, pending| pending.is_active(now));
    before != store.state().pending_invites.len()
}

fn prune_expired_config_edits(store: &mut StateStore) -> bool {
    let now = Utc::now();
    let before = store.state().pending_config_edits.len();
    store
        .state_mut()
        .pending_config_edits
        .retain(|_, pending| pending.is_active(now));
    before != store.state().pending_config_edits.len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingInviteAction {
    None,
    Command(BotCommand),
    Rejected(String),
}

impl PendingInviteAction {
    fn changed_state(&self) -> bool {
        matches!(self, Self::Command(_) | Self::Rejected(_))
    }
}

fn apply_config_update(config: &KoipyConfig, path: &str, value: &str) -> Result<KoipyConfig> {
    let mut reloaded = reload_config_from_source(config)?;
    let mut yaml = serde_yaml::to_value(&reloaded)?;
    let path = ConfigPath::parse(path)?;
    let parsed_value = parse_config_value(value)?;
    config_path_set(&mut yaml, &path, parsed_value)?;
    reloaded = serde_yaml::from_value(yaml)?;
    reloaded.source_path = config.source_path.clone();
    reloaded.save_to_source()?;
    reload_config_from_source(&reloaded)
}

fn apply_pending_config_edit(
    config: &KoipyConfig,
    store: &mut StateStore,
    user_id: i64,
    value: &str,
    pending: &crate::state::PendingConfigEdit,
) -> Result<(KoipyConfig, String)> {
    let updated = apply_config_update(config, &pending.path, value)?;
    store.state_mut().pending_config_edits.remove(&user_id);
    store.save()?;
    Ok((updated, format!("Config updated: {}", pending.path)))
}

async fn take_pending_invite_action(
    config: &KoipyConfig,
    store: &mut StateStore,
    user_id: i64,
    text: &str,
) -> Result<PendingInviteAction> {
    let target = text.trim();
    if !store.state().pending_invites.contains_key(&user_id) {
        return Ok(PendingInviteAction::None);
    }
    let Some(pending) = store.state_mut().pending_invites.remove(&user_id) else {
        return Ok(PendingInviteAction::None);
    };
    if !looks_like_subscription_url(target) {
        if target.starts_with('/') {
            store.state_mut().pending_invites.insert(user_id, pending);
            return Ok(PendingInviteAction::None);
        }
        return Ok(PendingInviteAction::Rejected(
            "Invalid URL for invite test; please run /invite again.".to_string(),
        ));
    }
    if !pending.is_active(Utc::now()) {
        return Ok(PendingInviteAction::Rejected(
            "Invite task expired; please run /invite again.".to_string(),
        ));
    }
    if let Err(err) = ensure_invite_target_allowed(config, Some(target)).await {
        return Ok(PendingInviteAction::Rejected(format!(
            "Invite subscription rejected: {err:#}"
        )));
    }
    Ok(PendingInviteAction::Command(BotCommand::Test {
        kind: invite_rule_task_kind(&pending.rule),
        command_token: format!("invite-{}", pending.rule),
        rule_name: Some(pending.rule),
        payload: target.to_string(),
    }))
}

fn looks_like_subscription_url(value: &str) -> bool {
    url::Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

const MAX_TELEGRAM_UPLOAD_BYTES: u64 = 10 * 1024 * 1024;
const FILE_TOO_LARGE: &str = "❌File too large (>10MB)";

fn non_text_message_reply(message: &TelegramMessage) -> Option<&'static str> {
    let document = message.document.as_ref()?;
    document
        .file_size
        .is_some_and(|size| size > MAX_TELEGRAM_UPLOAD_BYTES)
        .then_some(FILE_TOO_LARGE)
}

const SUBINFO_TOURIST_DENIED: &str = "❌Security settings active, tourists denied";
const SUBINFO_PROXY_WARNING: &str =
    "⚠️Tourists using /subinfo may leak IP. Set network.httpProxy to allow it.";

fn tourist_subinfo_target(config: &KoipyConfig, target: &str) -> Result<String> {
    let target = target.trim();
    if target.is_empty() {
        bail!("Usage: /subinfo <subscription URL>");
    }
    if config
        .network
        .http_proxy
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        bail!("{SUBINFO_TOURIST_DENIED}\n{SUBINFO_PROXY_WARNING}");
    }
    parse_subscription_url(target, &config.subconverter)
        .ok_or_else(|| anyhow::anyhow!("invalid subscription URL"))
}

fn subscription_info_text(
    name: &str,
    url: &str,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    traffic: Option<&SubscriptionTraffic>,
    traffic_text: &str,
) -> String {
    let mut lines = vec![
        format!(
            "🔍Query Time: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        ),
        format!("☁️Sub Name: {name}"),
        format!("☁️Sub URL: {url}"),
        format!("✈️Site Name: {}", site_name(url)),
    ];
    if let Some(created_at) = created_at {
        lines.push(format!("Created: {created_at}"));
    }
    if let Some(traffic) = traffic {
        lines.push(format!("Upload: {}", humanize_bytes(traffic.upload)));
        lines.push(format!("Download: {}", humanize_bytes(traffic.download)));
        lines.push(format!("Used: {}", humanize_bytes(traffic.used())));
        lines.push(format!("Total: {}", humanize_bytes(traffic.total)));
    }
    lines.push(traffic_text.to_string());
    lines.join("\n")
}

fn humanize_bytes(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}

fn invite_rule_task_kind(rule: &str) -> TaskKind {
    match rule {
        "speed" | "uspeed" => TaskKind::Speed,
        "analyze" => TaskKind::Topo,
        _ => TaskKind::Test,
    }
}

fn invite_group_allowed(config: &KoipyConfig, chat_id: i64) -> bool {
    config
        .bot
        .invite_group
        .iter()
        .any(|value| value.trim() == chat_id.to_string())
}

fn strict_callback_allowed(
    config: &KoipyConfig,
    store: &StateStore,
    key: &str,
    user_id: i64,
) -> bool {
    !config.bot.strict_mode
        || store
            .state()
            .pending_task_owners
            .get(key)
            .is_none_or(|owner| *owner == user_id)
}

const QUERY_NOT_FOUND: &str = "❌Button resource reclaimed";
const OPERATION_TIMEOUT: &str = "🗑️Operation timeout";
const SLAVE_SELECTOR_TIMEOUT: &str = "SlaveSelector timeout";
const SORT_SELECTOR_TIMEOUT: &str = "SortSelector timeout";
const UNKNOWN_CALLBACK: &str = "❌Unknown callback";
const TASK_CANCELLED: &str = "✅Task cancelled";
const TASK_CANCEL_BUTTON: &str = "👋Cancel Task";

fn known_callback_namespace(data: &str) -> bool {
    matches!(
        data.split_once(':')
            .map(|(prefix, _)| prefix)
            .unwrap_or(data),
        "panel" | "demo" | "invite" | "task"
    )
}

fn reclaim_pending_task(store: &mut StateStore, key: &str) -> bool {
    let existed = store.state_mut().pending_tasks.remove(key).is_some();
    store.state_mut().pending_task_owners.remove(key);
    store.state_mut().pending_script_pages.remove(key);
    store.state_mut().pending_script_selections.remove(key);
    existed
}

fn cancel_pending_task(store: &mut StateStore, key: &str) -> bool {
    reclaim_pending_task(store, key)
}

fn cancel_user_pending(store: &mut StateStore, user_id: i64) -> bool {
    let invite_removed = store.state_mut().pending_invites.remove(&user_id).is_some();
    let task_keys: Vec<String> = store
        .state()
        .pending_task_owners
        .iter()
        .filter(|(_, owner)| **owner == user_id)
        .map(|(key, _)| key.clone())
        .collect();
    let mut task_removed = false;
    for key in task_keys {
        task_removed |= reclaim_pending_task(store, &key);
    }
    invite_removed || task_removed
}

fn anti_group_enabled(config: &KoipyConfig, store: &StateStore) -> bool {
    config.bot.anti_group || store.state().anti_group
}

fn bot_token_user_id(config: &KoipyConfig) -> Option<i64> {
    config
        .bot
        .bot_token
        .as_deref()
        .and_then(|token| token.split_once(':'))
        .and_then(|(id, _)| id.parse().ok())
}

fn anti_group_should_leave(
    config: &KoipyConfig,
    store: &StateStore,
    message: &TelegramMessage,
) -> bool {
    if !anti_group_enabled(config, store) || message.chat.id >= 0 {
        return false;
    }
    let Some(bot_id) = bot_token_user_id(config) else {
        return false;
    };
    let bot_joined = message
        .new_chat_members
        .iter()
        .any(|member| member.id == bot_id);
    if !bot_joined {
        return false;
    }
    let inviter_id = message.from.as_ref().map(|user| user.id);
    !inviter_id.is_some_and(|user_id| is_admin(config, user_id))
}

fn panel_text(config: &KoipyConfig, store: &StateStore) -> String {
    format!(
        "koipy-rs panel\nsubscriptions: {}\nrules: {}\nlast task users: {}\nslaves: {}\nanti_group: {}\nnight_shift: {}",
        store.state().subscriptions.len(),
        store.state().rules.len(),
        store.state().last_tasks.len(),
        config.slave_config.slaves.len(),
        store.state().anti_group,
        store.state().night_shift,
    )
}

fn license_info_text(config: &KoipyConfig, user_id: i64, target: Option<&str>) -> String {
    let configured = !config.license.trim().is_empty();
    let target = target
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("current bot");
    let status = if configured {
        "configured (not verified)"
    } else {
        "not configured"
    };
    let version = if configured {
        "Koipy compatible local metadata"
    } else {
        "Free local mode"
    };
    format!(
        "**License Information**\nStatus: {status}\nVersion: {version}\nBot ID: {target}\nRequester UID: {user_id}\nSlave Limit: unlimited locally\nRule Limit: unlimited locally\nGroup Test: controlled by config permissions\nNote: koipy-rs does not replicate activation-code authorization or unlock paid limits."
    )
}

fn user_text(config: &KoipyConfig, store: &StateStore) -> String {
    let admins = yaml_user_list(&config.admin);
    let configured_users = yaml_user_list(&config.user);
    let runtime_users = sorted_i64_list(store.state().granted_users.iter().copied());
    let temporary_users = sorted_i64_list(store.state().temporary_invites.keys().copied());
    let pending_invites = sorted_i64_list(store.state().pending_invites.keys().copied());

    [
        "Users".to_string(),
        format!("Admins: {}", display_list(&admins)),
        format!("Configured users: {}", display_list(&configured_users)),
        format!("Runtime grants: {}", display_list(&runtime_users)),
        format!("Temporary invites: {}", display_list(&temporary_users)),
        format!("Pending invite inputs: {}", display_list(&pending_invites)),
    ]
    .join("\n")
}

const USAGE_GRANT: &str = "Usage 1: /grant <UID> ...\nUsage 2: /grant <reply to message>";

fn authorization_target_user_id(parsed: i64, message: &TelegramMessage) -> Result<i64> {
    if parsed > 0 {
        return Ok(parsed);
    }
    message
        .reply_to_message
        .as_deref()
        .and_then(|reply| reply.from.as_ref())
        .map(|user| user.id)
        .filter(|id| *id > 0)
        .ok_or_else(|| anyhow::anyhow!(USAGE_GRANT))
}

fn switch_translation_language(config: &mut KoipyConfig, lang: &str) -> Result<()> {
    let lang = lang.trim();
    let Some(path) = config.translation_resource_path(lang) else {
        bail!("Failed to switch language, language pack file for {lang} not found");
    };
    if !path.is_file() {
        bail!("Failed to switch language, language pack file for {lang} not found");
    }
    config.translation.lang = lang.to_string();
    Ok(())
}

fn yaml_user_list(users: &[crate::config::UserId]) -> Vec<String> {
    let mut values: BTreeSet<String> = BTreeSet::new();
    for value in users {
        values.insert(yaml_user_label(value));
    }
    values.into_iter().collect()
}

fn yaml_user_label(value: &crate::config::UserId) -> String {
    match value {
        serde_yaml::Value::Number(number) => number.to_string(),
        serde_yaml::Value::String(text) => text.clone(),
        other => serde_yaml::to_string(other)
            .unwrap_or_else(|_| format!("{other:?}"))
            .trim()
            .to_string(),
    }
}

fn sorted_i64_list(values: impl Iterator<Item = i64>) -> Vec<String> {
    values
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|value| value.to_string())
        .collect()
}

fn display_list(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn normalize_rule_name(name: &str, message_id: i64) -> Result<String> {
    let name = name.trim();
    let name = if name.is_empty() {
        format!("rule-{message_id}")
    } else {
        name.to_string()
    };
    if internal_rule_keyword(&name) {
        bail!("❌Internal keyword as rule name");
    }
    Ok(name)
}

fn internal_rule_keyword(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "help"
            | "version"
            | "system"
            | "restart"
            | "reboot"
            | "killme"
            | "test"
            | "speed"
            | "analyze"
            | "topo"
            | "full"
            | "ping"
            | "udptype"
            | "uspeed"
            | "re"
            | "invite"
            | "share"
            | "new"
            | "newrule"
            | "rule"
            | "sub"
            | "traffic"
            | "subinfo"
            | "remove"
            | "checkslave"
            | "checkslaves"
            | "reload"
            | "get"
            | "set"
            | "del"
            | "delete"
            | "setantigroup"
            | "panel"
            | "demo"
            | "license"
            | "logs"
            | "log"
            | "user"
            | "grant"
            | "ungrant"
            | "leave"
            | "nightshift"
    )
}

fn display_rule_names(names: &[String]) -> String {
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join(", ")
    }
}

fn rule_detail_text(
    config: &KoipyConfig,
    store: &StateStore,
    name: &str,
    user_id: i64,
    is_admin: bool,
) -> Result<String> {
    if let Some(rule) = store
        .state()
        .rules
        .get(name)
        .filter(|rule| rule.can_access(user_id) || is_admin)
    {
        return Ok(format!(
            "Rule: {}\nURL: {}\nCreated: {}",
            rule.name, rule.url, rule.created_at
        ));
    }
    if let Some(rule) = config
        .rules
        .iter()
        .find(|rule| rule.enable && rule.name == name)
    {
        return Ok(format!(
            "Preset rule: {}\nURL: {}\nScripts: {}\nSlaves: {}",
            rule.name,
            if rule.url.is_empty() { "-" } else { &rule.url },
            display_rule_names(&rule.scripts),
            display_rule_names(&rule.slaveid)
        ));
    }
    bail!("rule not found")
}

fn demo_text() -> String {
    [
        "This is the demo collection, available cases:",
        "",
        "Drawing demo uses current image configuration for preview.",
    ]
    .join("\n")
}

fn demo_result_table() -> TestResultTable {
    TestResultTable {
        rows: vec![
            TestResultRow {
                node_name: "Demo - low latency".to_string(),
                node_type: "ss".to_string(),
                http_latency_ms: Some(68.0),
                rtt_ms: Some(41.0),
                avg_speed_bytes: Some(48_600_000.0),
                max_speed_bytes: Some(82_400_000.0),
                udp_type: Some("Full Cone".to_string()),
                per_second_mb: vec![32.0, 45.0, 51.0, 63.0, 58.0],
                script_results: vec![
                    ("Netflix".to_string(), "Unlocked".to_string()),
                    ("OpenAI".to_string(), "Available".to_string()),
                ],
            },
            TestResultRow {
                node_name: "Demo - warning".to_string(),
                node_type: "vmess".to_string(),
                http_latency_ms: Some(328.0),
                rtt_ms: Some(185.0),
                avg_speed_bytes: Some(8_200_000.0),
                max_speed_bytes: Some(14_500_000.0),
                udp_type: Some("Restricted".to_string()),
                per_second_mb: vec![6.0, 8.0, 10.0, 7.0, 12.0],
                script_results: vec![
                    ("Disney+".to_string(), "Blocked".to_string()),
                    ("YouTube".to_string(), "OK".to_string()),
                ],
            },
        ],
        inbound: Some(serde_json::json!({"Country":"US","IP":"203.0.113.10"})),
        outbound: Some(serde_json::json!({"Country":"JP","IP":"198.51.100.20"})),
        raw: serde_json::json!({"demo": true}),
    }
}

fn script_names(config: &KoipyConfig) -> Vec<String> {
    config
        .script_config
        .scripts
        .iter()
        .map(|script| script.name.clone())
        .filter(|name| !name.is_empty())
        .collect()
}

fn reload_config_from_source(config: &KoipyConfig) -> Result<KoipyConfig> {
    let path = config
        .source_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("config source path is not known"))?;
    KoipyConfig::from_path(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigPath(Vec<ConfigPathSegment>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigPathSegment {
    Key(String),
    Index(usize),
}

impl ConfigPath {
    fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim().trim_start_matches('$').trim_start_matches('.');
        if raw.is_empty() {
            bail!("config path is required");
        }
        let mut segments = Vec::new();
        for part in raw.split('.') {
            if part.is_empty() {
                bail!("invalid empty config path segment");
            }
            parse_config_path_part(part, &mut segments)?;
        }
        Ok(Self(segments))
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for segment in &self.0 {
            match segment {
                ConfigPathSegment::Key(key) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(key);
                }
                ConfigPathSegment::Index(index) => {
                    out.push('[');
                    out.push_str(&index.to_string());
                    out.push(']');
                }
            }
        }
        out
    }
}

fn parse_config_path_part(part: &str, segments: &mut Vec<ConfigPathSegment>) -> Result<()> {
    let mut cursor = part;
    if let Some((key, rest)) = cursor.split_once('[') {
        if !key.is_empty() {
            segments.push(ConfigPathSegment::Key(key.to_string()));
        }
        cursor = rest;
    } else {
        segments.push(ConfigPathSegment::Key(cursor.to_string()));
        return Ok(());
    }

    loop {
        let (index, rest) = cursor
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("unclosed array index in config path"))?;
        if index.is_empty() {
            bail!("empty array index in config path");
        }
        segments.push(ConfigPathSegment::Index(index.parse::<usize>()?));
        if rest.is_empty() {
            break;
        }
        cursor = rest
            .strip_prefix('[')
            .ok_or_else(|| anyhow::anyhow!("invalid array index syntax in config path"))?;
    }
    Ok(())
}

fn config_path_get<'a>(
    value: &'a serde_yaml::Value,
    path: &ConfigPath,
) -> Option<&'a serde_yaml::Value> {
    let mut current = value;
    for segment in &path.0 {
        current = match segment {
            ConfigPathSegment::Key(key) => current.get(key)?,
            ConfigPathSegment::Index(index) => current.as_sequence()?.get(*index)?,
        };
    }
    Some(current)
}

fn config_path_set(
    value: &mut serde_yaml::Value,
    path: &ConfigPath,
    replacement: serde_yaml::Value,
) -> Result<()> {
    let Some((last, parents)) = path.0.split_last() else {
        bail!("config path is required");
    };
    let parent = config_path_parent_mut(value, parents)?;
    match last {
        ConfigPathSegment::Key(key) => {
            let mapping = parent
                .as_mapping_mut()
                .ok_or_else(|| anyhow::anyhow!("config path parent is not an object"))?;
            mapping.insert(serde_yaml::Value::String(key.clone()), replacement);
        }
        ConfigPathSegment::Index(index) => {
            let sequence = parent
                .as_sequence_mut()
                .ok_or_else(|| anyhow::anyhow!("config path parent is not an array"))?;
            let slot = sequence
                .get_mut(*index)
                .ok_or_else(|| anyhow::anyhow!("config array index out of range"))?;
            *slot = replacement;
        }
    }
    Ok(())
}

fn config_path_delete(value: &mut serde_yaml::Value, path: &ConfigPath) -> Result<()> {
    let Some((last, parents)) = path.0.split_last() else {
        bail!("config path is required");
    };
    let parent = config_path_parent_mut(value, parents)?;
    match last {
        ConfigPathSegment::Key(key) => {
            let mapping = parent
                .as_mapping_mut()
                .ok_or_else(|| anyhow::anyhow!("config path parent is not an object"))?;
            let removed = mapping
                .remove(serde_yaml::Value::String(key.clone()))
                .is_some();
            if !removed {
                bail!("config path not found");
            }
        }
        ConfigPathSegment::Index(index) => {
            let sequence = parent
                .as_sequence_mut()
                .ok_or_else(|| anyhow::anyhow!("config path parent is not an array"))?;
            if *index >= sequence.len() {
                bail!("config array index out of range");
            }
            sequence.remove(*index);
        }
    }
    Ok(())
}

fn config_path_parent_mut<'a>(
    value: &'a mut serde_yaml::Value,
    parents: &[ConfigPathSegment],
) -> Result<&'a mut serde_yaml::Value> {
    let mut current = value;
    for segment in parents {
        current = match segment {
            ConfigPathSegment::Key(key) => current
                .get_mut(key)
                .ok_or_else(|| anyhow::anyhow!("config path not found"))?,
            ConfigPathSegment::Index(index) => current
                .as_sequence_mut()
                .and_then(|sequence| sequence.get_mut(*index))
                .ok_or_else(|| anyhow::anyhow!("config array index out of range"))?,
        };
    }
    Ok(current)
}

fn parse_config_value(raw: &str) -> Result<serde_yaml::Value> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("config value is required");
    }
    serde_yaml::from_str(raw).or_else(|_| Ok(serde_yaml::Value::String(raw.to_string())))
}

fn render_config_value(value: &serde_yaml::Value) -> Result<String> {
    match value {
        serde_yaml::Value::Null => Ok("null".to_string()),
        serde_yaml::Value::Bool(value) => Ok(value.to_string()),
        serde_yaml::Value::Number(value) => Ok(value.to_string()),
        serde_yaml::Value::String(value) => Ok(value.clone()),
        _ => Ok(serde_yaml::to_string(value)?.trim().to_string()),
    }
}

fn allow_echo(store: &mut StateStore, user_id: i64, limit_seconds: f64) -> Result<bool> {
    if limit_seconds <= 0.0 {
        return Ok(true);
    }
    let now = Utc::now();
    if let Some(last) = store.state().last_echo_at.get(&user_id) {
        let elapsed = now.signed_duration_since(*last).num_milliseconds();
        let limit_ms = (limit_seconds * 1000.0).round().max(1.0) as i64;
        if elapsed >= 0 && elapsed < limit_ms {
            return Ok(false);
        }
    }
    store.state_mut().last_echo_at.insert(user_id, now);
    store.save()?;
    Ok(true)
}

async fn ensure_invite_target_allowed(config: &KoipyConfig, target: Option<&str>) -> Result<()> {
    let mut blacklist = config.bot.invite_blacklist_domain.clone();
    for url in config
        .bot
        .invite_blacklist_url
        .iter()
        .filter(|url| !url.trim().is_empty())
    {
        let bytes = SubscriptionCollector::new(config)?
            .fetch_config(url)
            .await?;
        blacklist.extend(parse_blacklist_domains(&String::from_utf8_lossy(&bytes)));
    }
    if let Some(target) = target {
        if invite_target_blocked(target, &blacklist) {
            bail!("subscription domain is blocked by invite blacklist");
        }
    }
    Ok(())
}

fn invite_target_blocked(target: &str, blacklist: &[String]) -> bool {
    let Some(host) = target_host(target) else {
        return false;
    };
    blacklist.iter().any(|entry| {
        let entry = normalize_blacklist_entry(entry);
        !entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))
    })
}

fn parse_blacklist_domains(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(normalize_blacklist_entry)
        .filter(|line| !line.is_empty())
        .collect()
}

fn normalize_blacklist_entry(value: &str) -> String {
    let trimmed = value.trim().trim_start_matches("*.").to_ascii_lowercase();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(host) = target_host(&trimmed) {
        host
    } else {
        trimmed
            .split('/')
            .next()
            .unwrap_or_default()
            .trim_matches('.')
            .to_string()
    }
}

fn target_host(target: &str) -> Option<String> {
    url::Url::parse(target)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
}

fn parse_pixel_threshold(value: &str) -> (u32, u32) {
    let mut parts = value.split('x');
    let width = parts.next().and_then(|value| value.trim().parse().ok());
    let height = parts.next().and_then(|value| value.trim().parse().ok());
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => (width, height),
        _ => (2500, 3500),
    }
}

fn should_send_as_photo(width: u32, height: u32, threshold: &str) -> bool {
    let (max_width, max_height) = parse_pixel_threshold(threshold);
    width < max_width && height < max_height
}

fn cleanup_rendered(config: &KoipyConfig, path: &std::path::Path) -> Result<()> {
    if !config.image.save && path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove rendered result {}", path.display()))?;
    }
    Ok(())
}

fn read_log_tail(lines: usize) -> Result<String> {
    let log_dir = std::path::Path::new("logs");
    let mut files: Vec<_> = if log_dir.is_dir() {
        std::fs::read_dir(log_dir)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .collect()
    } else {
        Vec::new()
    };
    files.sort_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());
    let Some(file) = files.pop() else {
        return Ok("No log file found".to_string());
    };
    let raw = std::fs::read_to_string(file.path())?;
    let tail = raw
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    Ok(tail)
}

fn yaml_value_is_id(value: &serde_yaml::Value, user_id: i64) -> bool {
    match value {
        serde_yaml::Value::Number(number) => number.as_i64() == Some(user_id),
        serde_yaml::Value::String(text) => text == &user_id.to_string(),
        _ => false,
    }
}

fn command_token_from_text(text: &str) -> String {
    text.trim()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string()
}

fn command_name_from_text(text: &str) -> String {
    let token = command_token_from_text(text);
    token.split('?').next().unwrap_or_default().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    Help,
    Version,
    System,
    Restart,
    Kill,
    Test {
        kind: TaskKind,
        command_token: String,
        rule_name: Option<String>,
        payload: String,
    },
    Re {
        payload: String,
    },
    Invite,
    Share {
        name: String,
        target: i64,
    },
    NewSubscription {
        url: String,
        name: String,
        password: Option<String>,
    },
    NewRule {
        url: String,
        name: String,
    },
    Rule {
        action: String,
        name: String,
    },
    ShowSubscription {
        name: String,
    },
    RemoveSubscription {
        names: Vec<String>,
    },
    CheckSlaves,
    Reload,
    GetConfig {
        path: String,
    },
    SetConfig {
        path: String,
        value: String,
    },
    DeleteConfig {
        path: String,
    },
    SetAntiGroup,
    Panel,
    Demo,
    License {
        target: Option<String>,
    },
    Logs {
        tail: Option<usize>,
    },
    User,
    SetCommands,
    Language {
        lang: Option<String>,
    },
    Grant {
        user_id: i64,
    },
    UnGrant {
        user_id: i64,
    },
    Leave,
    NightShift,
    Cancel,
    Disabled(String),
    Unknown(String),
}

#[derive(Debug, Default)]
pub struct BotCommandRouter;

impl BotCommandRouter {
    pub fn parse(text: &str) -> BotCommand {
        let trimmed = text.trim();
        let command = trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches('/');
        let command_name = command.split('?').next().unwrap_or(command);
        let payload = trimmed
            .split_once(char::is_whitespace)
            .map(|(_, rhs)| rhs.trim().to_string())
            .unwrap_or_default();
        let normalized_command = command_name;
        match normalized_command {
            "help" => BotCommand::Help,
            "version" => BotCommand::Version,
            "system" => BotCommand::System,
            "restart" | "reboot" => BotCommand::Restart,
            "killme" => BotCommand::Kill,
            "test" => BotCommand::Test {
                kind: TaskKind::Test,
                command_token: command.to_string(),
                rule_name: None,
                payload,
            },
            "speed" => BotCommand::Test {
                kind: TaskKind::Speed,
                command_token: command.to_string(),
                rule_name: None,
                payload,
            },
            "analyze" | "topo" => BotCommand::Test {
                kind: TaskKind::Topo,
                command_token: command.to_string(),
                rule_name: None,
                payload,
            },
            "re" => BotCommand::Re { payload },
            "invite" => BotCommand::Invite,
            "share" => {
                let mut args = payload.split_whitespace();
                BotCommand::Share {
                    name: args.next().unwrap_or_default().to_string(),
                    target: args
                        .next()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default(),
                }
            }
            "new" => {
                let mut args = payload.split_whitespace();
                BotCommand::NewSubscription {
                    url: args.next().unwrap_or_default().to_string(),
                    name: args.next().unwrap_or("default").to_string(),
                    password: args.next().map(ToString::to_string),
                }
            }
            "newrule" => {
                let mut args = payload.split_whitespace();
                BotCommand::NewRule {
                    url: args.next().unwrap_or_default().to_string(),
                    name: args.next().unwrap_or("default").to_string(),
                }
            }
            "rule" => {
                let mut args = payload.split_whitespace();
                let first = args.next().unwrap_or_default();
                if first.is_empty() {
                    BotCommand::Rule {
                        action: "list".to_string(),
                        name: String::new(),
                    }
                } else if looks_like_subscription_url(first) {
                    BotCommand::NewRule {
                        url: first.to_string(),
                        name: args.next().unwrap_or_default().to_string(),
                    }
                } else if matches!(first, "list" | "show" | "delete" | "remove") {
                    BotCommand::Rule {
                        action: first.to_string(),
                        name: args.next().unwrap_or_default().to_string(),
                    }
                } else {
                    BotCommand::Rule {
                        action: "show".to_string(),
                        name: first.to_string(),
                    }
                }
            }
            "sub" | "traffic" | "subinfo" => BotCommand::ShowSubscription {
                name: payload
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            },
            "remove" => BotCommand::RemoveSubscription {
                names: payload
                    .split_whitespace()
                    .map(ToString::to_string)
                    .collect(),
            },
            "checkslave" | "checkslaves" => BotCommand::CheckSlaves,
            "reload" => BotCommand::Reload,
            "get" => BotCommand::GetConfig { path: payload },
            "set" => {
                let (path, value) = payload
                    .split_once(char::is_whitespace)
                    .map(|(path, value)| (path.trim().to_string(), value.trim().to_string()))
                    .unwrap_or_else(|| (payload, String::new()));
                BotCommand::SetConfig { path, value }
            }
            "del" | "delete" => BotCommand::DeleteConfig { path: payload },
            "setantigroup" => BotCommand::SetAntiGroup,
            "panel" => BotCommand::Panel,
            "demo" => BotCommand::Demo,
            "license" => BotCommand::License {
                target: payload
                    .split_whitespace()
                    .next()
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string),
            },
            "logs" | "log" => BotCommand::Logs {
                tail: payload.parse::<usize>().ok(),
            },
            "user" => BotCommand::User,
            "setcmd" => BotCommand::SetCommands,
            "lang" | "language" => BotCommand::Language {
                lang: payload
                    .split_whitespace()
                    .next()
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string),
            },
            "grant" => BotCommand::Grant {
                user_id: payload
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
            },
            "ungrant" => BotCommand::UnGrant {
                user_id: payload
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default(),
            },
            "leave" => BotCommand::Leave,
            "nightshift" => BotCommand::NightShift,
            "cancel" => BotCommand::Cancel,
            other => BotCommand::Unknown(other.to_string()),
        }
    }

    pub fn parse_with_custom(
        text: &str,
        custom_commands: &[BotCommandConfig],
        rules: &[RuleConfig],
    ) -> BotCommand {
        let parsed = Self::parse(text);
        if !matches!(parsed, BotCommand::Unknown(_)) {
            return parsed;
        }
        Self::parse_custom_or(text, custom_commands, rules, parsed)
    }

    pub fn parse_for_config(text: &str, config: &KoipyConfig) -> BotCommand {
        if config.bot.bypass_mode {
            return Self::parse_custom_or(
                text,
                &config.bot.command,
                &config.rules,
                BotCommand::Unknown(command_name_from_text(text)),
            );
        }
        Self::parse_with_custom(text, &config.bot.command, &config.rules)
    }

    fn parse_custom_or(
        text: &str,
        custom_commands: &[BotCommandConfig],
        rules: &[RuleConfig],
        fallback: BotCommand,
    ) -> BotCommand {
        let trimmed = text.trim();
        let command = command_token_from_text(text);
        let command_name = command.split('?').next().unwrap_or(command.as_str());
        if custom_commands
            .iter()
            .any(|custom| !custom.enable && custom.name == command_name)
        {
            return BotCommand::Disabled(command_name.to_string());
        }
        if let Some(custom) = custom_commands
            .iter()
            .find(|custom| custom.is_test_command() && custom.name == command_name)
        {
            let legacy_rule = custom.rule == "test";
            let mapped_rule_exists = rules
                .iter()
                .any(|rule| rule.enable && rule.name == custom.rule);
            if !legacy_rule && !mapped_rule_exists {
                return BotCommand::Unknown(command_name.to_string());
            }
            let payload = trimmed
                .split_once(char::is_whitespace)
                .map(|(_, rhs)| rhs.trim().to_string())
                .unwrap_or_default();
            BotCommand::Test {
                kind: TaskKind::Test,
                command_token: command.to_string(),
                rule_name: if legacy_rule {
                    None
                } else {
                    Some(custom.rule.clone())
                },
                payload,
            }
        } else {
            fallback
        }
    }

    pub fn help_text(is_admin: bool, is_user: bool) -> String {
        let tourist =
            "/help show help\n/version show version\n/traffic or /subinfo show subscription info";
        let user = "/test run connectivity/script test\n/speed run download speed test\n/analyze or /topo run topology test\n/re rerun last task\n/invite grant temporary test access\n/share share subscription\n/new add subscription\n/sub show subscription\n/checkslaves check slaves\n/demo show drawing demo";
        let admin = "/system show system info\n/user show users\n/remove remove subscriptions\n/reload reload config\n/setantigroup toggle anti-group\n/restart restart\n/panel control panel\n/license show local license metadata\n/logs show logs\nkillme stop process";
        match (is_admin, is_user) {
            (true, _) => format!("{}\n\n{}\n\n{}", tourist, user, admin),
            (false, true) => format!("{}\n\n{}", tourist, user),
            _ => tourist.to_string(),
        }
    }

    pub fn system_info() -> String {
        let mut sys = System::new_all();
        sys.refresh_memory();
        format!(
            "System: {}\nTime: {}\nMemory: {}MB/{}MB",
            System::name().unwrap_or_else(|| "unknown".to_string()),
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            sys.used_memory() / 1024 / 1024,
            sys.total_memory() / 1024 / 1024,
        )
    }
}

#[derive(Debug, Clone)]
struct TelegramApi {
    client: Client,
    base_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SendOptions {
    parse_mode: Option<String>,
    protect_content: bool,
    disable_notification: bool,
}

impl SendOptions {
    fn from_config(config: &KoipyConfig) -> Self {
        Self {
            parse_mode: config.bot.parse_mode.as_option(),
            protect_content: config.runtime.protect_content,
            disable_notification: config.bot.disable_notification,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct TelegramBotCommand {
    command: String,
    description: String,
}

fn pinned_bot_commands(config: &KoipyConfig) -> Vec<TelegramBotCommand> {
    config
        .bot
        .command
        .iter()
        .filter(|command| command.enable && command.pin)
        .filter_map(|command| {
            let name = command.name.trim().trim_start_matches('/').to_string();
            if name.is_empty() {
                return None;
            }
            let description = if command.text.trim().is_empty() {
                name.clone()
            } else {
                command.text.trim().to_string()
            };
            Some(TelegramBotCommand {
                command: name,
                description,
            })
        })
        .collect()
}

fn bot_commands_text(commands: &[TelegramBotCommand]) -> String {
    commands
        .iter()
        .map(|command| format!("/{} - {}", command.command, command.description))
        .collect::<Vec<_>>()
        .join("\n")
}

fn telegram_payload(mut payload: serde_json::Value, options: &SendOptions) -> serde_json::Value {
    if let serde_json::Value::Object(fields) = &mut payload {
        if let Some(parse_mode) = &options.parse_mode {
            fields.insert(
                "parse_mode".to_string(),
                serde_json::Value::String(parse_mode.clone()),
            );
        }
        fields.insert(
            "protect_content".to_string(),
            serde_json::Value::Bool(options.protect_content),
        );
        fields.insert(
            "disable_notification".to_string(),
            serde_json::Value::Bool(options.disable_notification),
        );
    }
    payload
}

fn add_multipart_options(
    mut form: reqwest::multipart::Form,
    options: &SendOptions,
) -> reqwest::multipart::Form {
    if let Some(parse_mode) = &options.parse_mode {
        form = form.text("parse_mode", parse_mode.clone());
    }
    form.text("protect_content", options.protect_content.to_string())
        .text(
            "disable_notification",
            options.disable_notification.to_string(),
        )
}

impl TelegramApi {
    fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(45))
                .build()?,
            base_url: format!("https://api.telegram.org/bot{token}"),
        })
    }

    async fn get_updates(&self, offset: i64) -> Result<Vec<TelegramUpdate>> {
        let response: TelegramResponse<Vec<TelegramUpdate>> = self
            .client
            .get(format!("{}/getUpdates", self.base_url))
            .query(&[("timeout", "30"), ("offset", &offset.to_string())])
            .send()
            .await
            .context("Telegram getUpdates failed")?
            .json()
            .await
            .context("Telegram getUpdates response decode failed")?;
        if response.ok {
            Ok(response.result.unwrap_or_default())
        } else {
            bail!("Telegram getUpdates returned ok=false")
        }
    }

    async fn delete_my_commands(&self) -> Result<()> {
        let response: TelegramResponse<bool> = self
            .client
            .post(format!("{}/deleteMyCommands", self.base_url))
            .send()
            .await
            .context("Telegram deleteMyCommands failed")?
            .json()
            .await
            .context("Telegram deleteMyCommands response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram deleteMyCommands returned ok=false")
        }
    }

    async fn set_my_commands(&self, commands: &[TelegramBotCommand]) -> Result<()> {
        let response: TelegramResponse<bool> = self
            .client
            .post(format!("{}/setMyCommands", self.base_url))
            .json(&serde_json::json!({ "commands": commands }))
            .send()
            .await
            .context("Telegram setMyCommands failed")?
            .json()
            .await
            .context("Telegram setMyCommands response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram setMyCommands returned ok=false")
        }
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        options: &SendOptions,
    ) -> Result<TelegramMessage> {
        let payload = telegram_payload(
            serde_json::json!({
                "chat_id": chat_id,
                "text": text,
                "disable_web_page_preview": true,
            }),
            options,
        );
        let response: TelegramResponse<TelegramMessage> = self
            .client
            .post(format!("{}/sendMessage", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("Telegram sendMessage failed")?
            .json()
            .await
            .context("Telegram sendMessage response decode failed")?;
        if response.ok {
            response
                .result
                .ok_or_else(|| anyhow::anyhow!("Telegram sendMessage returned no result"))
        } else {
            bail!("Telegram sendMessage returned ok=false")
        }
    }

    async fn send_message_markup(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: InlineKeyboardMarkup,
        options: &SendOptions,
    ) -> Result<()> {
        let payload = telegram_payload(
            serde_json::json!({
                "chat_id": chat_id,
                "text": text,
                "disable_web_page_preview": true,
                "reply_markup": reply_markup,
            }),
            options,
        );
        let response: TelegramResponse<TelegramMessage> = self
            .client
            .post(format!("{}/sendMessage", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("Telegram sendMessage markup failed")?
            .json()
            .await
            .context("Telegram sendMessage markup response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram sendMessage markup returned ok=false")
        }
    }

    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
        options: &SendOptions,
    ) -> Result<()> {
        let payload = telegram_payload(
            serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "text": text,
                "reply_markup": reply_markup,
            }),
            options,
        );
        let response: TelegramResponse<TelegramMessage> = self
            .client
            .post(format!("{}/editMessageText", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("Telegram editMessageText failed")?
            .json()
            .await
            .context("Telegram editMessageText response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram editMessageText returned ok=false")
        }
    }

    async fn answer_callback_query(&self, callback_query_id: &str, text: &str) -> Result<()> {
        let response: TelegramResponse<bool> = self
            .client
            .post(format!("{}/answerCallbackQuery", self.base_url))
            .json(&serde_json::json!({
                "callback_query_id": callback_query_id,
                "text": text,
                "show_alert": false,
            }))
            .send()
            .await
            .context("Telegram answerCallbackQuery failed")?
            .json()
            .await
            .context("Telegram answerCallbackQuery response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram answerCallbackQuery returned ok=false")
        }
    }

    async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let response: TelegramResponse<bool> = self
            .client
            .post(format!("{}/deleteMessage", self.base_url))
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
            }))
            .send()
            .await
            .context("Telegram deleteMessage failed")?
            .json()
            .await
            .context("Telegram deleteMessage response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram deleteMessage returned ok=false")
        }
    }

    async fn send_photo(
        &self,
        chat_id: i64,
        path: &std::path::Path,
        caption: &str,
        options: &SendOptions,
    ) -> Result<()> {
        let form = add_multipart_options(
            reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .text("caption", caption.to_string())
                .part(
                    "photo",
                    reqwest::multipart::Part::bytes(std::fs::read(path)?)
                        .file_name(
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("result.png")
                                .to_string(),
                        )
                        .mime_str("image/png")?,
                ),
            options,
        );
        let response: TelegramResponse<TelegramMessage> = self
            .client
            .post(format!("{}/sendPhoto", self.base_url))
            .multipart(form)
            .send()
            .await
            .context("Telegram sendPhoto failed")?
            .json()
            .await
            .context("Telegram sendPhoto response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram sendPhoto returned ok=false")
        }
    }

    async fn send_document(
        &self,
        chat_id: i64,
        path: &std::path::Path,
        caption: &str,
        mime: &str,
        options: &SendOptions,
    ) -> Result<()> {
        let form = add_multipart_options(
            reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .text("caption", caption.to_string())
                .part(
                    "document",
                    reqwest::multipart::Part::bytes(std::fs::read(path)?)
                        .file_name(
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("result.json")
                                .to_string(),
                        )
                        .mime_str(mime)?,
                ),
            options,
        );
        let response: TelegramResponse<TelegramMessage> = self
            .client
            .post(format!("{}/sendDocument", self.base_url))
            .multipart(form)
            .send()
            .await
            .context("Telegram sendDocument failed")?
            .json()
            .await
            .context("Telegram sendDocument response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram sendDocument returned ok=false")
        }
    }

    async fn send_video(
        &self,
        chat_id: i64,
        path: &std::path::Path,
        caption: &str,
        options: &SendOptions,
    ) -> Result<()> {
        let form = add_multipart_options(
            reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .text("caption", caption.to_string())
                .part(
                    "video",
                    reqwest::multipart::Part::bytes(std::fs::read(path)?)
                        .file_name(
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("result.mp4")
                                .to_string(),
                        )
                        .mime_str("video/mp4")?,
                ),
            options,
        );
        let response: TelegramResponse<TelegramMessage> = self
            .client
            .post(format!("{}/sendVideo", self.base_url))
            .multipart(form)
            .send()
            .await
            .context("Telegram sendVideo failed")?
            .json()
            .await
            .context("Telegram sendVideo response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram sendVideo returned ok=false")
        }
    }

    async fn leave_chat(&self, chat_id: i64) -> Result<()> {
        let response: TelegramResponse<bool> = self
            .client
            .post(format!("{}/leaveChat", self.base_url))
            .json(&serde_json::json!({ "chat_id": chat_id }))
            .send()
            .await
            .context("Telegram leaveChat failed")?
            .json()
            .await
            .context("Telegram leaveChat response decode failed")?;
        if response.ok {
            Ok(())
        } else {
            bail!("Telegram leaveChat returned ok=false")
        }
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TelegramMessage {
    message_id: i64,
    chat: TelegramChat,
    from: Option<TelegramUser>,
    text: Option<String>,
    document: Option<TelegramDocument>,
    reply_to_message: Option<Box<TelegramMessage>>,
    #[serde(default)]
    new_chat_members: Vec<TelegramUser>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TelegramDocument {
    file_id: Option<String>,
    file_name: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TelegramUser {
    id: i64,
    username: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TelegramCallbackQuery {
    id: String,
    from: TelegramUser,
    message: Option<TelegramMessage>,
    data: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct InlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Clone, Serialize)]
struct InlineKeyboardButton {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

impl InlineKeyboardButton {
    fn callback(text: impl Into<String>, callback_data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: Some(callback_data.into()),
            url: None,
        }
    }

    fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: None,
            url: Some(url.into()),
        }
    }
}

fn panel_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![
                InlineKeyboardButton::callback("Toggle anti-group", "panel:anti"),
                InlineKeyboardButton::callback("Toggle night", "panel:night"),
            ],
            vec![InlineKeyboardButton::callback(
                "Check slaves",
                "panel:slaves",
            )],
            vec![InlineKeyboardButton::callback("Close", "panel:close")],
        ],
    }
}

fn demo_keyboard(config: &KoipyConfig) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![
            InlineKeyboardButton::callback(
                localized_text(config, "demo2", "Generate drawing demo"),
                "demo:image",
            ),
            InlineKeyboardButton::url(
                localized_text(config, "demo4", "Open color palette"),
                "https://htmlcolorcodes.com/color-picker/",
            ),
        ]],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InviteAction {
    text: String,
    rule: String,
}

const BUILTIN_INVITE_RULES: [(&str, &str); 7] = [
    ("test", "Test"),
    ("analyze", "Analyze"),
    ("speed", "Speed"),
    ("full", "Full"),
    ("ping", "Ping"),
    ("udptype", "UDP Type"),
    ("uspeed", "Upload Speed"),
];

fn invite_actions(config: &KoipyConfig) -> Vec<InviteAction> {
    let mut actions = Vec::new();
    for (name, default_text) in BUILTIN_INVITE_RULES {
        let override_command = config
            .bot
            .command
            .iter()
            .find(|command| command.name == name && command.attach_to_invite);
        if override_command.is_some_and(|command| !command.enable) {
            continue;
        }
        let (text, rule) = override_command
            .map(|command| {
                let text = if command.text.trim().is_empty() {
                    default_text.to_string()
                } else {
                    command.text.trim().to_string()
                };
                let rule = if command.rule.trim().is_empty() {
                    name.to_string()
                } else {
                    command.rule.trim().to_string()
                };
                (text, rule)
            })
            .unwrap_or_else(|| (default_text.to_string(), name.to_string()));
        actions.push(InviteAction { text, rule });
    }

    for command in config
        .bot
        .command
        .iter()
        .filter(|command| command.attach_to_invite && command.enable)
        .filter(|command| {
            !BUILTIN_INVITE_RULES
                .iter()
                .any(|(name, _)| command.name == *name)
        })
    {
        let rule = command.rule.trim();
        if rule.is_empty() {
            continue;
        }
        let text = if command.text.trim().is_empty() {
            command.name.trim().to_string()
        } else {
            command.text.trim().to_string()
        };
        if !text.is_empty() {
            actions.push(InviteAction {
                text,
                rule: rule.to_string(),
            });
        }
    }

    actions
}

fn invite_keyboard(config: &KoipyConfig) -> InlineKeyboardMarkup {
    let rows = invite_actions(config)
        .chunks(2)
        .map(|chunk| {
            chunk
                .iter()
                .map(|action| {
                    InlineKeyboardButton::callback(
                        action.text.clone(),
                        format!("invite:rule:{}", action.rule),
                    )
                })
                .collect()
        })
        .collect();
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

fn slave_keyboard(config: &KoipyConfig, key: &str) -> InlineKeyboardMarkup {
    let mut rows = Vec::new();
    for slave in config.visible_slaves() {
        rows.push(vec![InlineKeyboardButton::callback(
            slave_display_name(config, slave),
            format!("task:slave:{key}:{}", slave.id),
        )]);
    }
    rows.push(vec![InlineKeyboardButton::callback(
        localized_text(config, "b-cancel", TASK_CANCEL_BUTTON),
        format!("task:cancel:{key}"),
    )]);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

fn sort_keyboard(config: &KoipyConfig, key: &str) -> InlineKeyboardMarkup {
    let sorts = [
        ("b-origin", "Original", "origin"),
        ("b-http", "HTTP asc", "http"),
        ("b-rhttp", "HTTP desc", "rhttp"),
        ("b-rtt", "RTT asc", "rtt"),
        ("b-rrtt", "RTT desc", "rrtt"),
        ("b-aspeed", "Avg speed asc", "aspeed"),
        ("b-arspeed", "Avg speed desc", "arspeed"),
        ("b-mspeed", "Max speed asc", "mspeed"),
        ("b-mrspeed", "Max speed desc", "mrspeed"),
    ];
    InlineKeyboardMarkup {
        inline_keyboard: sorts
            .chunks(2)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|(label_key, fallback, value)| {
                        InlineKeyboardButton::callback(
                            localized_text(config, label_key, fallback),
                            format!("task:sort:{key}:{value}"),
                        )
                    })
                    .collect()
            })
            .chain(std::iter::once(vec![InlineKeyboardButton::callback(
                localized_text(config, "b-cancel", TASK_CANCEL_BUTTON),
                format!("task:cancel:{key}"),
            )]))
            .collect(),
    }
}

const SCRIPT_PAGE_SIZE: usize = 8;

fn script_keyboard(
    key: &str,
    config: &KoipyConfig,
    store: &StateStore,
    page: usize,
) -> InlineKeyboardMarkup {
    let names: Vec<String> = config
        .script_config
        .scripts
        .iter()
        .map(|script| script.name.clone())
        .filter(|name| !name.is_empty())
        .collect();
    let selected = store
        .state()
        .pending_script_selections
        .get(key)
        .cloned()
        .unwrap_or_default();
    let start = page.saturating_mul(SCRIPT_PAGE_SIZE).min(names.len());
    let end = (start + SCRIPT_PAGE_SIZE).min(names.len());
    let mut rows: Vec<Vec<InlineKeyboardButton>> = names[start..end]
        .chunks(2)
        .map(|chunk| {
            chunk
                .iter()
                .map(|name| {
                    InlineKeyboardButton::callback(
                        format!(
                            "{}{}",
                            if selected.contains(name) {
                                "[x] "
                            } else {
                                "[ ] "
                            },
                            name
                        ),
                        format!("task:scripts:{key}:{name}"),
                    )
                })
                .collect()
        })
        .collect();
    rows.push(vec![
        InlineKeyboardButton::callback(
            localized_text(config, "page1", "Prev"),
            format!("task:scripts:{key}:prev"),
        ),
        InlineKeyboardButton::callback(
            format!("{} {}", localized_text(config, "page", "Page"), page + 1),
            format!("task:scripts:{key}:noop"),
        ),
        InlineKeyboardButton::callback(
            localized_text(config, "page2", "Next"),
            format!("task:scripts:{key}:next"),
        ),
    ]);
    rows.push(vec![
        InlineKeyboardButton::callback(
            localized_text(config, "b-all", "All"),
            format!("task:scripts:{key}:all"),
        ),
        InlineKeyboardButton::callback(
            localized_text(config, "b-reverse", "Reverse"),
            format!("task:scripts:{key}:reverse"),
        ),
        InlineKeyboardButton::callback("None", format!("task:scripts:{key}:none")),
    ]);
    rows.push(vec![InlineKeyboardButton::callback(
        localized_text(config, "b-ok2", "OK"),
        format!("task:scripts:{key}:ok"),
    )]);
    rows.push(vec![InlineKeyboardButton::callback(
        localized_text(config, "b-cancel", TASK_CANCEL_BUTTON),
        format!("task:cancel:{key}"),
    )]);
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BotParseMode;
    use std::sync::Arc as StdArc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    fn closed_zh_config() -> KoipyConfig {
        let mut config = KoipyConfig::default();
        config.translation.lang = "zh_CN".to_string();
        config.translation.resources.insert(
            "zh-CN".to_string(),
            "./resources/localization/zh-CN.yml".to_string(),
        );
        config
    }

    #[test]
    fn parses_unimplemented_documented_commands() {
        assert!(matches!(
            BotCommandRouter::parse("/topo https://example.com/sub"),
            BotCommand::Test {
                kind: TaskKind::Topo,
                ..
            }
        ));
        assert!(matches!(
            BotCommandRouter::parse("/checkslaves"),
            BotCommand::CheckSlaves
        ));
        assert!(matches!(BotCommandRouter::parse("/demo"), BotCommand::Demo));
        assert!(matches!(
            BotCommandRouter::parse("/setcmd"),
            BotCommand::SetCommands
        ));
        assert!(matches!(
            BotCommandRouter::parse("/cancel"),
            BotCommand::Cancel
        ));
        assert_eq!(
            BotCommandRouter::parse("/lang en-us"),
            BotCommand::Language {
                lang: Some("en-us".to_string())
            }
        );
        assert_eq!(
            BotCommandRouter::parse("/license 123456"),
            BotCommand::License {
                target: Some("123456".to_string())
            }
        );
    }

    #[test]
    fn parses_subscription_commands() {
        assert_eq!(
            BotCommandRouter::parse("/new https://example.com/sub airport pass"),
            BotCommand::NewSubscription {
                url: "https://example.com/sub".to_string(),
                name: "airport".to_string(),
                password: Some("pass".to_string())
            }
        );
    }

    #[test]
    fn parses_rule_commands() {
        assert_eq!(
            BotCommandRouter::parse("/newrule https://example.com/sub hk"),
            BotCommand::NewRule {
                url: "https://example.com/sub".to_string(),
                name: "hk".to_string(),
            }
        );
        assert_eq!(
            BotCommandRouter::parse("/rule show hk"),
            BotCommand::Rule {
                action: "show".to_string(),
                name: "hk".to_string(),
            }
        );
        assert_eq!(
            BotCommandRouter::parse("/rule https://example.com/sub hk"),
            BotCommand::NewRule {
                url: "https://example.com/sub".to_string(),
                name: "hk".to_string(),
            }
        );
        assert_eq!(
            BotCommandRouter::parse("/rule hk"),
            BotCommand::Rule {
                action: "show".to_string(),
                name: "hk".to_string(),
            }
        );
    }

    #[test]
    fn rule_helpers_match_documented_create_show_and_preset_surface() {
        assert_eq!(normalize_rule_name("", 42).expect("generated"), "rule-42");
        assert!(
            normalize_rule_name("test", 42)
                .expect_err("internal keyword")
                .to_string()
                .contains("Internal keyword")
        );

        let store = StateStore::open(
            std::env::temp_dir().join(format!("koipy-rs-rule-helper-{}.json", std::process::id())),
        )
        .expect("state");
        let mut config = KoipyConfig::default();
        config.rules = vec![RuleConfig {
            name: "preset".to_string(),
            enable: true,
            url: "https://preset.example/sub".to_string(),
            scripts: vec!["Netflix".to_string()],
            slaveid: vec!["local".to_string()],
            ..Default::default()
        }];

        let detail = rule_detail_text(&config, &store, "preset", 1001, false).expect("preset");
        assert!(detail.contains("Preset rule: preset"));
        assert!(detail.contains("Netflix"));

        let presets: Vec<_> = config
            .rules
            .iter()
            .filter(|rule| rule.enable)
            .map(|rule| rule.name.clone())
            .collect();
        assert_eq!(display_rule_names(&presets), "preset");
    }

    #[test]
    fn parses_grant_command() {
        assert_eq!(BotCommandRouter::parse("/user"), BotCommand::User);
        assert_eq!(
            BotCommandRouter::parse("/grant 12345"),
            BotCommand::Grant { user_id: 12345 }
        );
        assert_eq!(
            BotCommandRouter::parse("/ungrant 12345"),
            BotCommand::UnGrant { user_id: 12345 }
        );
    }

    #[test]
    fn authorization_target_can_come_from_replied_message() {
        let message: TelegramMessage = serde_json::from_value(serde_json::json!({
            "message_id": 20,
            "chat": {"id": 1},
            "from": {"id": 1001},
            "text": "/grant",
            "reply_to_message": {
                "message_id": 19,
                "chat": {"id": 1},
                "from": {"id": 2002},
                "text": "hello"
            }
        }))
        .expect("reply message");

        assert_eq!(
            authorization_target_user_id(0, &message).expect("reply uid"),
            2002
        );
        assert_eq!(
            authorization_target_user_id(3003, &message).expect("explicit uid"),
            3003
        );

        let no_reply: TelegramMessage = serde_json::from_value(serde_json::json!({
            "message_id": 21,
            "chat": {"id": 1},
            "from": {"id": 1001},
            "text": "/grant"
        }))
        .expect("plain message");
        assert_eq!(
            authorization_target_user_id(0, &no_reply)
                .expect_err("usage")
                .to_string(),
            USAGE_GRANT
        );
    }

    #[test]
    fn user_text_reports_config_runtime_and_invite_users() {
        let path = std::env::temp_dir().join(format!(
            "koipy-rs-user-text-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut store = StateStore::open(&path).expect("state");
        store.state_mut().granted_users = vec![3003, 2002, 2002];
        store
            .state_mut()
            .temporary_invites
            .insert(4004, Utc::now() + Duration::minutes(10));
        store.state_mut().pending_invites.insert(
            5005,
            PendingInvite::new("test".to_string(), 1, 2, Utc::now() + Duration::minutes(10)),
        );

        let mut config = KoipyConfig::default();
        config.admin.push(serde_yaml::Value::Number(1001.into()));
        config
            .user
            .push(serde_yaml::Value::String("2002".to_string()));

        let text = user_text(&config, &store);
        assert!(text.contains("Admins: 1001"));
        assert!(text.contains("Configured users: 2002"));
        assert!(text.contains("Runtime grants: 2002, 3003"));
        assert!(text.contains("Temporary invites: 4004"));
        assert!(text.contains("Pending invite inputs: 5005"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn translation_language_switch_requires_existing_resource_file() {
        let dir = std::env::temp_dir().join(format!(
            "koipy-rs-lang-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let config_path = dir.join("config.yaml");
        let lang_path = dir.join("en-us.yml");
        std::fs::write(&config_path, "translation:\n  lang: zh-CN\n").expect("config");
        std::fs::write(&lang_path, "current_lang: 'Current language: {}'\n").expect("lang");

        let mut config = KoipyConfig::default();
        config.source_path = Some(config_path);
        config.translation.lang = "zh-CN".to_string();
        config
            .translation
            .resources
            .insert("en-us".to_string(), "en-us.yml".to_string());
        config
            .translation
            .resources
            .insert("zh-CN".to_string(), "en-us.yml".to_string());
        config
            .translation
            .resources
            .insert("missing".to_string(), "missing.yml".to_string());

        switch_translation_language(&mut config, "en-us").expect("switch");
        assert_eq!(config.translation.lang, "en-us");
        switch_translation_language(&mut config, "zh_CN").expect("switch alias");
        assert_eq!(config.translation.lang, "zh_CN");
        assert!(
            switch_translation_language(&mut config, "missing")
                .expect_err("missing")
                .to_string()
                .contains("language pack file for missing not found")
        );
        assert_eq!(config.translation_resource_path("zh_CN").is_some(), true);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn panel_keyboard_has_callbacks() {
        let keyboard = panel_keyboard();
        let callbacks: Vec<_> = keyboard
            .inline_keyboard
            .into_iter()
            .flatten()
            .filter_map(|button| button.callback_data)
            .collect();
        assert!(callbacks.contains(&"panel:anti".to_string()));
        assert!(callbacks.contains(&"panel:slaves".to_string()));
    }

    #[test]
    fn demo_keyboard_has_drawing_and_palette_actions() {
        let config = closed_zh_config();
        let keyboard = demo_keyboard(&config);
        let buttons: Vec<_> = keyboard.inline_keyboard.into_iter().flatten().collect();

        assert!(
            buttons
                .iter()
                .any(|button| button.callback_data.as_deref() == Some("demo:image"))
        );
        assert!(buttons.iter().any(|button| {
            button.text == localized_text(&config, "demo2", "Generate drawing demo")
        }));
        assert!(buttons.iter().any(|button| {
            button
                .url
                .as_deref()
                .is_some_and(|url| url.contains("color-picker"))
        }));
        assert!(buttons.iter().any(|button| {
            button.text == localized_text(&config, "demo4", "Open color palette")
        }));
    }

    #[test]
    fn invite_keyboard_uses_builtin_and_attached_custom_rules() {
        let mut config = KoipyConfig::default();
        config.bot.command = vec![
            BotCommandConfig {
                name: "ping".to_string(),
                enable: false,
                rule: "ping".to_string(),
                pin: false,
                text: String::new(),
                title: String::new(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "speed".to_string(),
                enable: true,
                rule: "speed-lite".to_string(),
                pin: false,
                text: "Quick Speed".to_string(),
                title: String::new(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "nf".to_string(),
                enable: true,
                rule: "netflix".to_string(),
                pin: false,
                text: "Netflix".to_string(),
                title: String::new(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "hidden".to_string(),
                enable: true,
                rule: "hidden".to_string(),
                pin: false,
                text: "Hidden".to_string(),
                title: String::new(),
                attach_to_invite: false,
            },
        ];

        let actions = invite_actions(&config);
        assert!(actions.contains(&InviteAction {
            text: "Test".to_string(),
            rule: "test".to_string(),
        }));
        assert!(actions.contains(&InviteAction {
            text: "Quick Speed".to_string(),
            rule: "speed-lite".to_string(),
        }));
        assert!(actions.contains(&InviteAction {
            text: "Netflix".to_string(),
            rule: "netflix".to_string(),
        }));
        assert!(!actions.iter().any(|action| action.rule == "ping"));
        assert!(!actions.iter().any(|action| action.rule == "hidden"));

        let keyboard = invite_keyboard(&config);
        let buttons: Vec<_> = keyboard.inline_keyboard.into_iter().flatten().collect();
        assert!(buttons.iter().any(|button| {
            button.text == "Netflix"
                && button.callback_data.as_deref() == Some("invite:rule:netflix")
        }));
        assert!(buttons.iter().any(|button| {
            button.text == "Quick Speed"
                && button.callback_data.as_deref() == Some("invite:rule:speed-lite")
        }));
    }

    #[test]
    fn demo_result_table_renders_with_current_image_config() {
        let dir = std::env::temp_dir().join(format!("koipy-rs-demo-{}", std::process::id()));
        let rendered = ResultRenderer::new(KoipyConfig::default())
            .render_table(&demo_result_table(), &dir)
            .expect("demo render");

        assert!(rendered.path.exists());
        assert!(rendered.width > 0);
        assert!(rendered.height > 0);
        let _ = std::fs::remove_file(rendered.path);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn license_info_is_metadata_only() {
        let mut config = KoipyConfig::default();
        let empty = license_info_text(&config, 1001, None);
        assert!(empty.contains("not configured"));
        assert!(empty.contains("does not replicate activation-code authorization"));

        config.license = "SAMPLE-LICENSE".to_string();
        let configured = license_info_text(&config, 1001, Some("bot-42"));
        assert!(configured.contains("configured (not verified)"));
        assert!(configured.contains("Bot ID: bot-42"));
        assert!(configured.contains("Slave Limit: unlimited locally"));
    }

    #[test]
    fn custom_command_maps_to_test() {
        let commands = vec![BotCommandConfig {
            name: "mytest".to_string(),
            enable: true,
            rule: "ping".to_string(),
            pin: true,
            text: "My test".to_string(),
            title: String::new(),
            attach_to_invite: true,
        }];
        let rules = vec![RuleConfig {
            name: "ping".to_string(),
            enable: true,
            url: "https://rule.example/sub".to_string(),
            ..Default::default()
        }];
        assert!(matches!(
            BotCommandRouter::parse_with_custom("/mytest?sort=http https://example.com/sub", &commands, &rules),
            BotCommand::Test {
                kind: TaskKind::Test,
                command_token,
                rule_name,
                payload,
                ..
            } if command_token == "mytest?sort=http" && rule_name.as_deref() == Some("ping") && payload == "https://example.com/sub"
        ));
    }

    #[test]
    fn disabled_custom_command_reports_closed_package_disabled_status() {
        let commands = vec![BotCommandConfig {
            name: "off".to_string(),
            enable: false,
            rule: "ping".to_string(),
            pin: false,
            text: String::new(),
            title: String::new(),
            attach_to_invite: true,
        }];
        assert_eq!(
            BotCommandRouter::parse_with_custom("/off https://example.com/sub", &commands, &[]),
            BotCommand::Disabled("off".to_string())
        );
    }

    #[test]
    fn disabled_and_bypass_messages_use_closed_package_translation_resources() {
        let mut config = KoipyConfig::default();
        config.translation.lang = "zh_CN".to_string();
        config.translation.resources.insert(
            "zh-CN".to_string(),
            "./resources/localization/zh-CN.yml".to_string(),
        );

        assert_eq!(
            disabled_command_message(&config, "off"),
            "`off` 指令已被禁用"
        );
        assert_eq!(
            bypass_mode_message(&config, "test"),
            "旁路模式已启用，bot所有内置指令已被禁用，仅可以使用配置中的自定义指令~"
        );
    }

    #[test]
    fn callback_flow_messages_use_closed_package_translation_resources() {
        let mut config = KoipyConfig::default();
        config.translation.lang = "zh_CN".to_string();
        config.translation.resources.insert(
            "zh-CN".to_string(),
            "./resources/localization/zh-CN.yml".to_string(),
        );

        assert_eq!(
            localized_text(&config, "demo3", "Generating..."),
            "正在生成..."
        );
        assert_eq!(
            localized_text(&config, "invite-10", "Waiting for subscription link"),
            "⏳正在等待上传订阅链接~"
        );
        assert_eq!(
            localized_text(&config, "realtime3", "Permission denied"),
            "❌没有权限执行此操作"
        );
        assert_eq!(
            localized_text(&config, "error-8", "Bad callback"),
            "❌非法参数，请检查!"
        );
        assert_eq!(
            localized_text(&config, "sort-select", "Select result sorting"),
            "请选择排序方式: \n"
        );
        assert_eq!(
            localized_text(&config, "script-select", "Select scripts"),
            "请选择要测试的脚本名称: \n"
        );
        assert_eq!(
            localized_text(&config, "script-ok", "Running task..."),
            "⏳正在生成测试任务......"
        );
    }

    #[test]
    fn custom_command_without_existing_rule_is_ignored() {
        let commands = vec![BotCommandConfig {
            name: "missing".to_string(),
            enable: true,
            rule: "missing-rule".to_string(),
            pin: false,
            text: String::new(),
            title: String::new(),
            attach_to_invite: true,
        }];
        assert_eq!(
            BotCommandRouter::parse_with_custom("/missing https://example.com/sub", &commands, &[]),
            BotCommand::Unknown("missing".to_string())
        );
    }

    #[test]
    fn applies_config_rule_defaults_before_command_options() {
        let rule = RuleConfig {
            name: "ping".to_string(),
            enable: true,
            url: "https://rule.example/sub".to_string(),
            scripts: vec!["Netflix".to_string()],
            slaveid: vec!["rule-slave".to_string(), "backup".to_string()],
            sort: crate::config::SortType::MaxSpeedDesc,
            ..Default::default()
        };
        let mut request = TaskRequest::new_url(TaskKind::Test, rule.url.clone());
        apply_config_rule(&mut request, &rule);
        assert_eq!(request.slave_ids, vec!["rule-slave", "backup"]);
        let request = request.apply_command_options("ping?s=cli-slave&sort=http");

        assert_eq!(request.raw_target, "https://rule.example/sub");
        assert_eq!(request.selected_scripts, vec!["Netflix"]);
        assert_eq!(request.slave_id.as_deref(), Some("cli-slave"));
        assert_eq!(request.slave_ids, vec!["cli-slave"]);
        assert_eq!(request.sort, Some(crate::config::SortType::HttpAsc));
    }

    #[test]
    fn applies_runtime_defaults_to_task_request() {
        let runtime = RuntimeConfig {
            speed_threads: 8,
            duration: 12,
            include_filter: "HK".to_string(),
            exclude_filter: "0.1".to_string(),
            sort: crate::config::SortType::AvgSpeedDesc,
            output: "video".to_string(),
            realtime: true,
            disable_sub_cvt: true,
            ..Default::default()
        };
        let mut request =
            TaskRequest::new_url(TaskKind::Speed, "https://example.com/sub".to_string());
        apply_runtime_defaults(&mut request, &runtime);

        assert_eq!(request.threading, Some(8));
        assert_eq!(request.duration, Some(12));
        assert_eq!(request.include, "HK");
        assert_eq!(request.exclude, "0.1");
        assert_eq!(request.sort, Some(crate::config::SortType::AvgSpeedDesc));
        assert_eq!(request.output, OutputMode::Video);
        assert!(request.realtime);
        assert!(request.nocvt);
    }

    #[test]
    fn realtime_progress_uses_selected_slave_name() {
        let mut config = KoipyConfig::default();
        config.slave_config.slaves = vec![
            crate::config::SlaveConfigEntry {
                id: "local".to_string(),
                comment: "Local backend".to_string(),
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
                option: crate::config::MiaoSpeedOption::default(),
            },
            crate::config::SlaveConfigEntry {
                id: "backup".to_string(),
                comment: "Backup backend".to_string(),
                hidden: false,
                token: "token".to_string(),
                r#type: SlaveType::MiaoSpeed,
                address: "127.0.0.1:8766".to_string(),
                path: "/".to_string(),
                proxy: None,
                skip_cert_verify: true,
                tls: false,
                invoker: None,
                buildtoken: None,
                option: crate::config::MiaoSpeedOption::default(),
            },
        ];
        let mut request =
            TaskRequest::new_url(TaskKind::Speed, "https://example.com/sub".to_string());
        request.set_slave_ids(vec!["local".to_string(), "backup".to_string()]);

        assert_eq!(
            realtime_slave_name(&config, &request),
            "Local backend(local), Backup backend(backup)"
        );
    }

    #[test]
    fn slave_check_report_uses_closed_package_sections() {
        let reports = vec![
            SlaveCheckReport {
                id: "local".to_string(),
                address: "127.0.0.1:8765".to_string(),
                kind: "miaospeed",
                hidden: false,
                status: SlaveCheckStatus::Alive,
            },
            SlaveCheckReport {
                id: "backup".to_string(),
                address: "127.0.0.1:8766".to_string(),
                kind: "miaospeed",
                hidden: true,
                status: SlaveCheckStatus::Offline,
            },
            SlaveCheckReport {
                id: "bot".to_string(),
                address: "bot".to_string(),
                kind: "bot",
                hidden: false,
                status: SlaveCheckStatus::Skipped,
            },
        ];
        let text = slave_check_report_text(&reports);
        assert!(text.starts_with("Slave Connectivity Test"));
        assert!(text.contains("✅Alive Slaves 1"));
        assert!(text.contains("❌Offline Slaves 1"));
        assert!(text.contains("‣local [miaospeed] 127.0.0.1:8765 visible online"));
        assert!(text.contains("‣backup [miaospeed] 127.0.0.1:8766 hidden offline"));
        assert!(text.contains("‣bot [bot] bot visible not-pinged"));

        let empty = slave_check_report_text(&[]);
        assert!(empty.contains("❌No backends configured, cannot start task"));
    }

    #[tokio::test]
    async fn check_slaves_report_blocks_concurrent_requests() {
        let config = KoipyConfig::default();
        let lock = Arc::new(Mutex::new(()));
        let held = lock.clone().lock_owned().await;
        let result = check_slaves_report(&config, &lock).await.expect("check");
        drop(held);
        assert_eq!(
            result.as_deref(),
            Some("❌Other user checking slaves, please wait...")
        );
    }

    #[test]
    fn reloads_config_from_source_path() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-reload-{}.yaml", std::process::id()));
        std::fs::write(&path, "bot:\n  bypassMode: false\n").expect("seed");
        let config = KoipyConfig::from_path(&path).expect("load");
        std::fs::write(&path, "bot:\n  bypassMode: true\n").expect("update");
        let reloaded = reload_config_from_source(&config).expect("reload");
        assert!(reloaded.bot.bypass_mode);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn set_config_command_parser_keeps_path_and_value() {
        assert_eq!(
            BotCommandRouter::parse("/set bot.parseMode HTML"),
            BotCommand::SetConfig {
                path: "bot.parseMode".to_string(),
                value: "HTML".to_string()
            }
        );
        assert_eq!(
            BotCommandRouter::parse("/set bot.parseMode"),
            BotCommand::SetConfig {
                path: "bot.parseMode".to_string(),
                value: String::new()
            }
        );
    }

    #[tokio::test]
    async fn pending_config_edit_is_processed_before_echo_limit() {
        let path = std::env::temp_dir().join(format!(
            "koipy-rs-pending-config-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "admin: [1001]\nuser: [1001]\nbot:\n  bot-token: 1:TEST\n  echo-limit: 60\n",
        )
        .expect("seed config");
        let mut config = KoipyConfig::default();
        config.source_path = Some(path.clone());
        config.bot.bot_token = Some("1:TEST".to_string());
        config.admin.push(serde_yaml::Value::Number(1001.into()));
        config.user.push(serde_yaml::Value::Number(1001.into()));
        let store_path = std::env::temp_dir().join(format!(
            "koipy-rs-pending-config-state-{}.json",
            std::process::id()
        ));
        let mut store = StateStore::open(&store_path).expect("state");
        store.state_mut().last_echo_at.insert(1001, Utc::now());
        store.state_mut().pending_config_edits.insert(
            1001,
            crate::state::PendingConfigEdit::new(
                "bot.parse-mode".to_string(),
                1,
                1,
                Utc::now() + Duration::seconds(60),
            ),
        );
        store.save().expect("save state");
        let runtime = BotRuntime::new(config.clone()).expect("runtime");
        let message = TelegramMessage {
            message_id: 2,
            chat: TelegramChat { id: 1 },
            from: Some(TelegramUser {
                id: 1001,
                username: None,
            }),
            text: None,
            document: None,
            reply_to_message: None,
            new_chat_members: Vec::new(),
        };
        let reply = runtime
            .handle_message(&config, &mut store, &message, "HTML")
            .await
            .expect("reply");
        assert!(
            reply
                .as_deref()
                .is_some_and(|text| text.contains("Config updated: bot.parse-mode"))
        );
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(store_path);
    }

    #[test]
    fn send_options_are_applied_to_telegram_json_payloads() {
        let options = SendOptions {
            parse_mode: Some("MarkdownV2".to_string()),
            protect_content: true,
            disable_notification: true,
        };
        let payload = telegram_payload(
            serde_json::json!({
                "chat_id": 1,
                "text": "*hello*",
            }),
            &options,
        );
        assert_eq!(payload["parse_mode"], "MarkdownV2");
        assert_eq!(payload["protect_content"], true);
        assert_eq!(payload["disable_notification"], true);
    }

    #[test]
    fn parse_mode_config_matches_closed_package_enum_values() {
        let mut config = KoipyConfig::default();

        config.bot.parse_mode = BotParseMode::Default;
        assert_eq!(SendOptions::from_config(&config).parse_mode, None);

        config.bot.parse_mode = BotParseMode::Disabled;
        assert_eq!(SendOptions::from_config(&config).parse_mode, None);

        config.bot.parse_mode = BotParseMode::Markdown;
        assert_eq!(
            SendOptions::from_config(&config).parse_mode.as_deref(),
            Some("Markdown")
        );

        config.bot.parse_mode = BotParseMode::Html;
        assert_eq!(
            SendOptions::from_config(&config).parse_mode.as_deref(),
            Some("HTML")
        );

        config.bot.parse_mode = BotParseMode::MarkdownV2;
        assert_eq!(
            SendOptions::from_config(&config).parse_mode.as_deref(),
            Some("MarkdownV2")
        );
    }

    #[tokio::test]
    async fn telegram_api_posts_json_markup_and_photo_to_bot_api() {
        let captured = StdArc::new(Mutex::new(Vec::new()));
        let base_url = spawn_telegram_api_server(captured.clone(), 2).await;
        let api = TelegramApi {
            client: Client::new(),
            base_url,
        };
        let options = SendOptions {
            parse_mode: Some("Markdown".to_string()),
            protect_content: true,
            disable_notification: true,
        };

        api.send_message_markup(
            99,
            "hello",
            InlineKeyboardMarkup {
                inline_keyboard: vec![vec![InlineKeyboardButton::callback("OK", "demo:ok")]],
            },
            &options,
        )
        .await
        .expect("send markup");

        let photo_path = std::env::temp_dir().join(format!(
            "koipy-rs-telegram-photo-{}.png",
            std::process::id()
        ));
        std::fs::write(&photo_path, b"fake-png").expect("photo file");
        api.send_photo(99, &photo_path, "caption", &options)
            .await
            .expect("send photo");
        let _ = std::fs::remove_file(photo_path);

        let captured = captured.lock().await;
        assert_eq!(captured.len(), 2);
        assert!(captured[0].starts_with("POST /botTEST/sendMessage HTTP/1.1"));
        assert!(captured[0].contains("\"reply_markup\""));
        assert!(captured[0].contains("\"callback_data\":\"demo:ok\""));
        assert!(captured[0].contains("\"protect_content\":true"));
        assert!(captured[1].starts_with("POST /botTEST/sendPhoto HTTP/1.1"));
        assert!(captured[1].contains("name=\"photo\""));
        assert!(captured[1].contains("name=\"parse_mode\""));
        assert!(captured[1].contains("Markdown"));
        assert!(captured[1].contains("name=\"protect_content\""));
        assert!(captured[1].contains("true"));
    }

    #[tokio::test]
    async fn auto_reset_commands_posts_delete_and_pinned_setmycommands() {
        let mut config = KoipyConfig::default();
        config.bot.command = vec![
            BotCommandConfig {
                name: "ping".to_string(),
                enable: true,
                rule: "ping".to_string(),
                pin: true,
                text: "PING test".to_string(),
                title: "PING title".to_string(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "hidden".to_string(),
                enable: true,
                rule: "hidden".to_string(),
                pin: false,
                text: "Hidden".to_string(),
                title: String::new(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "off".to_string(),
                enable: false,
                rule: "off".to_string(),
                pin: true,
                text: "Disabled".to_string(),
                title: String::new(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "/fallback".to_string(),
                enable: true,
                rule: "fallback".to_string(),
                pin: true,
                text: String::new(),
                title: String::new(),
                attach_to_invite: true,
            },
        ];
        let commands = pinned_bot_commands(&config);
        assert_eq!(
            commands,
            vec![
                TelegramBotCommand {
                    command: "ping".to_string(),
                    description: "PING test".to_string(),
                },
                TelegramBotCommand {
                    command: "fallback".to_string(),
                    description: "fallback".to_string(),
                },
            ]
        );
        assert_eq!(
            bot_commands_text(&commands),
            "/ping - PING test\n/fallback - fallback"
        );

        let captured = StdArc::new(Mutex::new(Vec::new()));
        let base_url = spawn_telegram_bool_api_server(captured.clone(), 2).await;
        let api = TelegramApi {
            client: Client::new(),
            base_url,
        };

        api.delete_my_commands().await.expect("delete commands");
        api.set_my_commands(&commands).await.expect("set commands");

        let captured = captured.lock().await;
        assert_eq!(captured.len(), 2);
        assert!(captured[0].starts_with("POST /botTEST/deleteMyCommands HTTP/1.1"));
        assert!(captured[1].starts_with("POST /botTEST/setMyCommands HTTP/1.1"));
        assert!(captured[1].contains("\"command\":\"ping\""));
        assert!(captured[1].contains("\"description\":\"PING test\""));
        assert!(captured[1].contains("\"command\":\"fallback\""));
        assert!(!captured[1].contains("hidden"));
        assert!(!captured[1].contains("off"));
    }

    #[tokio::test]
    async fn manual_setcmd_posts_only_enabled_pinned_commands() {
        let mut config = KoipyConfig::default();
        config.bot.command = vec![
            BotCommandConfig {
                name: "ping".to_string(),
                enable: true,
                rule: "ping".to_string(),
                pin: true,
                text: "PING test".to_string(),
                title: String::new(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "hidden".to_string(),
                enable: true,
                rule: "hidden".to_string(),
                pin: false,
                text: "Hidden".to_string(),
                title: String::new(),
                attach_to_invite: true,
            },
        ];
        let commands = pinned_bot_commands(&config);
        assert_eq!(bot_commands_text(&commands), "/ping - PING test");

        let captured = StdArc::new(Mutex::new(Vec::new()));
        let base_url = spawn_telegram_bool_api_server(captured.clone(), 1).await;
        let api = TelegramApi {
            client: Client::new(),
            base_url,
        };

        api.set_my_commands(&commands).await.expect("manual setcmd");

        let captured = captured.lock().await;
        assert_eq!(captured.len(), 1);
        assert!(captured[0].starts_with("POST /botTEST/setMyCommands HTTP/1.1"));
        assert!(captured[0].contains("\"command\":\"ping\""));
        assert!(!captured[0].contains("hidden"));
    }

    #[test]
    fn bypass_mode_disables_builtin_commands_but_keeps_configured_custom_rules() {
        let mut config = KoipyConfig::default();
        config.bot.bypass_mode = true;
        config.bot.command = vec![
            BotCommandConfig {
                name: "custom".to_string(),
                enable: true,
                rule: "ping".to_string(),
                pin: false,
                text: String::new(),
                title: String::new(),
                attach_to_invite: true,
            },
            BotCommandConfig {
                name: "off".to_string(),
                enable: false,
                rule: "ping".to_string(),
                pin: false,
                text: String::new(),
                title: String::new(),
                attach_to_invite: true,
            },
        ];
        config.rules = vec![RuleConfig {
            name: "ping".to_string(),
            enable: true,
            url: "https://rule.example/sub".to_string(),
            ..Default::default()
        }];

        assert_eq!(
            BotCommandRouter::parse_for_config("/test https://example.com/sub", &config),
            BotCommand::Unknown("test".to_string())
        );
        assert_eq!(
            BotCommandRouter::parse_for_config("/help", &config),
            BotCommand::Unknown("help".to_string())
        );
        assert_eq!(
            BotCommandRouter::parse_for_config("/off https://example.com/sub", &config),
            BotCommand::Disabled("off".to_string())
        );
        assert!(matches!(
            BotCommandRouter::parse_for_config("/custom?sort=http https://example.com/sub", &config),
            BotCommand::Test {
                command_token,
                rule_name,
                payload,
                ..
            } if command_token == "custom?sort=http" && rule_name.as_deref() == Some("ping") && payload == "https://example.com/sub"
        ));
    }

    #[test]
    fn invite_group_allows_invite_without_general_user_permission() {
        let mut config = KoipyConfig::default();
        config.bot.invite_group = vec!["-100123".to_string()];

        assert!(invite_group_allowed(&config, -100123));
        assert!(!invite_group_allowed(&config, -100456));
        assert!(!is_user(&config, 42));
    }

    #[test]
    fn temporary_invite_grants_user_access_until_expired() {
        let path = std::env::temp_dir().join(format!(
            "koipy-rs-temporary-invite-{}.json",
            std::process::id()
        ));
        let mut store = StateStore::open(&path).expect("state");
        store
            .state_mut()
            .temporary_invites
            .insert(1001, Utc::now() + Duration::minutes(30));
        store
            .state_mut()
            .temporary_invites
            .insert(2002, Utc::now() - Duration::minutes(1));

        assert!(temporary_invite_active(&store, 1001));
        assert!(!temporary_invite_active(&store, 2002));
        assert!(prune_expired_temporary_invites(&mut store));
        assert!(store.state().temporary_invites.contains_key(&1001));
        assert!(!store.state().temporary_invites.contains_key(&2002));
        assert!(!prune_expired_temporary_invites(&mut store));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn pending_invite_consumes_next_subscription_url_as_rule_task() {
        let path = std::env::temp_dir().join(format!(
            "koipy-rs-pending-invite-{}.json",
            std::process::id()
        ));
        let mut store = StateStore::open(&path).expect("state");
        let config = KoipyConfig::default();
        store.state_mut().pending_invites.insert(
            1001,
            PendingInvite::new(
                "speed".to_string(),
                99,
                7,
                Utc::now() + Duration::seconds(60),
            ),
        );

        assert_eq!(
            take_pending_invite_action(&config, &mut store, 1001, "/help")
                .await
                .expect("command skip"),
            PendingInviteAction::None
        );
        assert!(store.state().pending_invites.contains_key(&1001));

        assert_eq!(
            take_pending_invite_action(&config, &mut store, 1001, "https://example.com/sub")
                .await
                .expect("consume"),
            PendingInviteAction::Command(BotCommand::Test {
                kind: TaskKind::Speed,
                command_token: "invite-speed".to_string(),
                rule_name: Some("speed".to_string()),
                payload: "https://example.com/sub".to_string(),
            })
        );
        assert!(!store.state().pending_invites.contains_key(&1001));

        store.state_mut().pending_invites.insert(
            2002,
            PendingInvite::new(
                "analyze".to_string(),
                99,
                8,
                Utc::now() - Duration::seconds(1),
            ),
        );
        assert!(prune_expired_pending_invites(&mut store));
        assert!(!store.state().pending_invites.contains_key(&2002));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn pending_invite_rejects_invalid_or_blacklisted_subscription_url() {
        let path = std::env::temp_dir().join(format!(
            "koipy-rs-pending-invite-reject-{}.json",
            std::process::id()
        ));
        let mut store = StateStore::open(&path).expect("state");
        let mut config = KoipyConfig::default();
        store.state_mut().pending_invites.insert(
            1001,
            PendingInvite::new(
                "test".to_string(),
                99,
                7,
                Utc::now() + Duration::seconds(60),
            ),
        );

        assert!(matches!(
            take_pending_invite_action(&config, &mut store, 1001, "not a url")
                .await
                .expect("invalid"),
            PendingInviteAction::Rejected(message) if message.contains("Invalid URL")
        ));
        assert!(!store.state().pending_invites.contains_key(&1001));

        config.bot.invite_blacklist_domain = vec!["blocked.example".to_string()];
        store.state_mut().pending_invites.insert(
            1001,
            PendingInvite::new(
                "test".to_string(),
                99,
                7,
                Utc::now() + Duration::seconds(60),
            ),
        );
        assert!(matches!(
            take_pending_invite_action(&config, &mut store, 1001, "https://blocked.example/sub")
                .await
                .expect("blacklist"),
            PendingInviteAction::Rejected(message) if message.contains("invite blacklist")
        ));
        assert!(!store.state().pending_invites.contains_key(&1001));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn strict_mode_limits_task_callbacks_to_owner() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-strict-mode-{}.json", std::process::id()));
        let mut store = StateStore::open(&path).expect("state");
        store
            .state_mut()
            .pending_task_owners
            .insert("1:2".to_string(), 1001);
        let mut config = KoipyConfig::default();

        assert!(strict_callback_allowed(&config, &store, "1:2", 2002));
        config.bot.strict_mode = true;
        assert!(strict_callback_allowed(&config, &store, "1:2", 1001));
        assert!(!strict_callback_allowed(&config, &store, "1:2", 2002));
        assert!(strict_callback_allowed(&config, &store, "legacy", 2002));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn anti_group_leaves_only_when_bot_is_added_by_non_admin() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-anti-group-{}.json", std::process::id()));
        let mut store = StateStore::open(&path).expect("state");
        store.state_mut().anti_group = true;
        let mut config = KoipyConfig::default();
        config.bot.bot_token = Some("777:TEST".to_string());
        config.admin = vec![serde_yaml::Value::Number(1001.into())];

        let non_admin_invite = TelegramMessage {
            message_id: 1,
            chat: TelegramChat { id: -100123 },
            from: Some(TelegramUser {
                id: 2002,
                username: None,
            }),
            text: None,
            document: None,
            reply_to_message: None,
            new_chat_members: vec![TelegramUser {
                id: 777,
                username: Some("koipy_bot".to_string()),
            }],
        };
        assert!(anti_group_should_leave(&config, &store, &non_admin_invite));

        let admin_invite = TelegramMessage {
            from: Some(TelegramUser {
                id: 1001,
                username: None,
            }),
            ..non_admin_invite.clone()
        };
        assert!(!anti_group_should_leave(&config, &store, &admin_invite));

        let other_member_joined = TelegramMessage {
            new_chat_members: vec![TelegramUser {
                id: 3003,
                username: None,
            }],
            ..non_admin_invite.clone()
        };
        assert!(!anti_group_should_leave(
            &config,
            &store,
            &other_member_joined
        ));

        store.state_mut().anti_group = false;
        config.bot.anti_group = true;
        assert!(anti_group_should_leave(&config, &store, &non_admin_invite));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn oversized_document_message_gets_closed_package_reply() {
        let message: TelegramMessage = serde_json::from_value(serde_json::json!({
            "message_id": 10,
            "chat": {"id": 42},
            "from": {"id": 1001},
            "document": {
                "file_id": "file-id",
                "file_name": "large.yaml",
                "mime_type": "application/x-yaml",
                "file_size": MAX_TELEGRAM_UPLOAD_BYTES + 1
            }
        }))
        .expect("telegram document message");

        assert_eq!(non_text_message_reply(&message), Some(FILE_TOO_LARGE));

        let mut small = message;
        small.document.as_mut().expect("document").file_size = Some(MAX_TELEGRAM_UPLOAD_BYTES);
        assert_eq!(non_text_message_reply(&small), None);
    }

    #[test]
    fn echo_limit_throttles_repeated_replies() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-echo-limit-{}.json", std::process::id()));
        let mut store = StateStore::open(&path).expect("state");
        assert!(allow_echo(&mut store, 1001, 60.0).expect("first"));
        assert!(!allow_echo(&mut store, 1001, 60.0).expect("second"));
        assert!(allow_echo(&mut store, 1002, 60.0).expect("other user"));
        assert!(!allow_echo(&mut store, 1002, 0.8).expect("fractional limit"));
        assert!(allow_echo(&mut store, 1001, 0.0).expect("disabled"));
        let _ = std::fs::remove_file(path);
    }

    async fn spawn_telegram_api_server(
        captured: StdArc<Mutex<Vec<String>>>,
        request_count: usize,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("telegram api listener");
        let addr = listener.local_addr().expect("telegram api addr");
        tokio::spawn(async move {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().await.expect("telegram api accept");
                let raw = read_http_request(&mut stream)
                    .await
                    .expect("telegram api request");
                captured.lock().await.push(raw);
                let body = serde_json::json!({
                    "ok": true,
                    "result": {
                        "message_id": 7,
                        "chat": {"id": 99},
                        "from": {"id": 1, "username": "bot"},
                        "text": "ok"
                    }
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("telegram api response");
            }
        });
        format!("http://{addr}/botTEST")
    }

    async fn spawn_telegram_bool_api_server(
        captured: StdArc<Mutex<Vec<String>>>,
        request_count: usize,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("telegram bool api listener");
        let addr = listener.local_addr().expect("telegram bool api addr");
        tokio::spawn(async move {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().await.expect("telegram bool api accept");
                let raw = read_http_request(&mut stream)
                    .await
                    .expect("telegram bool api request");
                captured.lock().await.push(raw);
                let body = serde_json::json!({
                    "ok": true,
                    "result": true
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("telegram bool api response");
            }
        });
        format!("http://{addr}/botTEST")
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Result<String> {
        let mut data = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buf).await?;
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buf[..read]);
            if let Some(header_end) = find_header_end(&data) {
                let header = String::from_utf8_lossy(&data[..header_end]).to_string();
                let content_length = header
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or_default();
                let total = header_end + 4 + content_length;
                while data.len() < total {
                    let read = stream.read(&mut buf).await?;
                    if read == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..read]);
                }
                break;
            }
        }
        Ok(String::from_utf8_lossy(&data).to_string())
    }

    fn find_header_end(data: &[u8]) -> Option<usize> {
        data.windows(4).position(|window| window == b"\r\n\r\n")
    }

    #[test]
    fn invite_blacklist_blocks_domains_and_subdomains() {
        let blacklist = vec!["blocked.example".to_string(), "*.wild.example".to_string()];
        assert!(invite_target_blocked(
            "https://blocked.example/sub",
            &blacklist
        ));
        assert!(invite_target_blocked(
            "https://a.wild.example/sub",
            &blacklist
        ));
        assert!(!invite_target_blocked(
            "https://allowed.example/sub",
            &blacklist
        ));
    }

    #[test]
    fn parses_remote_invite_blacklist_lines() {
        let parsed = parse_blacklist_domains(
            r#"
# comment
https://blocked.example/path
*.wild.example
plain.example/list
"#,
        );
        assert_eq!(
            parsed,
            vec![
                "blocked.example".to_string(),
                "wild.example".to_string(),
                "plain.example".to_string(),
            ]
        );
    }

    #[test]
    fn tourist_subinfo_requires_http_proxy_before_fetching_url() {
        let mut config = KoipyConfig::default();
        let err = tourist_subinfo_target(&config, "https://example.com/sub")
            .expect_err("tourist subinfo should be denied without proxy")
            .to_string();
        assert!(err.contains(SUBINFO_TOURIST_DENIED));
        assert!(err.contains("network.httpProxy"));

        config.network.http_proxy = Some("http://127.0.0.1:7890".to_string());
        assert_eq!(
            tourist_subinfo_target(&config, "https://example.com/sub").expect("proxied target"),
            "https://example.com/sub"
        );
        assert!(tourist_subinfo_target(&config, "not a url").is_err());
    }

    #[test]
    fn subscription_info_text_uses_closed_package_fields() {
        let text = subscription_info_text(
            "airport",
            "https://example.com/sub",
            None,
            Some(&SubscriptionTraffic {
                upload: 1024,
                download: 2048,
                total: 4096,
                expire: Some(2_000_000_000),
            }),
            "Upload: 1.0 KiB",
        );
        assert!(text.contains("Query Time:"));
        assert!(text.contains("Sub Name: airport"));
        assert!(text.contains("Sub URL: https://example.com/sub"));
        assert!(text.contains("Site Name: *.com"));
        assert!(text.contains("Upload: 1.0 KiB"));
    }

    #[test]
    fn pixel_threshold_controls_photo_vs_document() {
        assert_eq!(parse_pixel_threshold("2500x3500"), (2500, 3500));
        assert_eq!(parse_pixel_threshold("bad"), (2500, 3500));
        assert!(should_send_as_photo(2499, 3499, "2500x3500"));
        assert!(!should_send_as_photo(2500, 100, "2500x3500"));
        assert!(!should_send_as_photo(100, 3500, "2500x3500"));
    }

    #[test]
    fn cleanup_respects_image_save_flag() {
        let mut config = KoipyConfig::default();
        config.image.save = false;
        let path = std::env::temp_dir().join("koipy-rs-cleanup-test.txt");
        std::fs::write(&path, b"temporary").expect("write");
        cleanup_rendered(&config, &path).expect("cleanup");
        assert!(!path.exists());

        config.image.save = true;
        std::fs::write(&path, b"temporary").expect("write");
        cleanup_rendered(&config, &path).expect("keep");
        assert!(path.exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn task_keyboards_have_callbacks() {
        let mut config = closed_zh_config();
        let sort = sort_keyboard(&config, "1:2");
        let sort_buttons: Vec<_> = sort.inline_keyboard.into_iter().flatten().collect();
        let sort_callbacks: Vec<_> = sort_buttons
            .iter()
            .filter_map(|button| button.callback_data.clone())
            .collect();
        assert!(
            sort_callbacks
                .iter()
                .any(|value| value.starts_with("task:sort:"))
        );
        assert!(sort_callbacks.contains(&"task:cancel:1:2".to_string()));
        assert!(
            sort_buttons.iter().any(
                |button| button.text == localized_text(&config, "b-cancel", TASK_CANCEL_BUTTON)
            )
        );
        assert!(sort_callbacks.contains(&"task:sort:1:2:rtt".to_string()));
        assert!(sort_callbacks.contains(&"task:sort:1:2:rrtt".to_string()));
        assert!(
            sort_buttons
                .iter()
                .any(|button| button.text == localized_text(&config, "b-origin", "Original"))
        );
        assert!(
            sort_buttons
                .iter()
                .any(|button| button.text == localized_text(&config, "b-rtt", "RTT asc"))
        );

        config.slave_config.slaves = vec![crate::config::SlaveConfigEntry {
            id: "local".to_string(),
            comment: "Local".to_string(),
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
            option: crate::config::MiaoSpeedOption::default(),
        }];
        let slave_callbacks: Vec<_> = slave_keyboard(&config, "1:2")
            .inline_keyboard
            .into_iter()
            .flatten()
            .filter_map(|button| button.callback_data)
            .collect();
        assert!(slave_callbacks.contains(&"task:cancel:1:2".to_string()));

        config.script_config.scripts = vec![crate::config::Script {
            name: "Netflix".to_string(),
            ..Default::default()
        }];
        let store = StateStore::open(std::env::temp_dir().join("koipy-rs-keyboard-state.json"))
            .expect("state");
        let scripts = script_keyboard("1:2", &config, &store, 0);
        let script_callbacks: Vec<_> = scripts
            .inline_keyboard
            .into_iter()
            .flatten()
            .filter_map(|button| button.callback_data)
            .collect();
        assert!(script_callbacks.contains(&"task:scripts:1:2:all".to_string()));
        assert!(script_callbacks.contains(&"task:cancel:1:2".to_string()));

        let scripts = script_keyboard("1:2", &config, &store, 0);
        let script_buttons: Vec<_> = scripts.inline_keyboard.into_iter().flatten().collect();
        assert!(
            script_buttons
                .iter()
                .any(|button| button.text == localized_text(&config, "page1", "Prev"))
        );
        assert!(
            script_buttons
                .iter()
                .any(|button| button.text == localized_text(&config, "page2", "Next"))
        );
        assert!(script_buttons.iter().any(|button| {
            button.text == format!("{} {}", localized_text(&config, "page", "Page"), 1)
        }));
        assert!(
            script_buttons
                .iter()
                .any(|button| button.text == localized_text(&config, "b-all", "All"))
        );
        assert!(
            script_buttons
                .iter()
                .any(|button| button.text == localized_text(&config, "b-reverse", "Reverse"))
        );
        assert!(
            script_buttons
                .iter()
                .any(|button| button.text == localized_text(&config, "b-ok2", "OK"))
        );
        assert!(
            script_buttons.iter().any(
                |button| button.text == localized_text(&config, "b-cancel", TASK_CANCEL_BUTTON)
            )
        );
    }

    #[test]
    fn callback_namespace_helper_preserves_permission_vs_unknown_callback() {
        assert!(known_callback_namespace("task:sort:1:http"));
        assert!(known_callback_namespace("panel:anti"));
        assert!(known_callback_namespace("invite:rule:test"));
        assert!(!known_callback_namespace("mystery:action"));
        let mut config = KoipyConfig::default();
        config.translation.resources.insert(
            "zh-CN".to_string(),
            "./resources/localization/zh-CN.yml".to_string(),
        );
        assert_eq!(
            config.translation_value("unknown-callback").as_deref(),
            Some("❌ 未知的回调类型")
        );
        assert_eq!(TASK_CANCELLED, "✅Task cancelled");
    }

    #[test]
    fn cancel_pending_task_clears_all_task_selection_state() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-cancel-task-{}.json", std::process::id()));
        let mut store = StateStore::open(&path).expect("state");
        store.state_mut().pending_tasks.insert(
            "1:2".to_string(),
            TaskRequest::new_url(TaskKind::Test, "https://example.com/sub".to_string()),
        );
        store
            .state_mut()
            .pending_task_owners
            .insert("1:2".to_string(), 1001);
        store
            .state_mut()
            .pending_script_pages
            .insert("1:2".to_string(), 2);
        store
            .state_mut()
            .pending_script_selections
            .insert("1:2".to_string(), crate::state::ScriptSelection::default());

        assert!(cancel_pending_task(&mut store, "1:2"));
        assert!(!store.state().pending_tasks.contains_key("1:2"));
        assert!(!store.state().pending_task_owners.contains_key("1:2"));
        assert!(!store.state().pending_script_pages.contains_key("1:2"));
        assert!(!store.state().pending_script_selections.contains_key("1:2"));
        assert!(!cancel_pending_task(&mut store, "1:2"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn text_cancel_clears_user_pending_invite_and_owned_tasks_only() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-text-cancel-{}.json", std::process::id()));
        let mut store = StateStore::open(&path).expect("state");
        store.state_mut().pending_invites.insert(
            1001,
            PendingInvite::new("test".to_string(), 1, 2, Utc::now() + Duration::minutes(10)),
        );
        for (key, owner) in [("owned", 1001), ("other", 2002)] {
            store.state_mut().pending_tasks.insert(
                key.to_string(),
                TaskRequest::new_url(TaskKind::Test, "https://example.com/sub".to_string()),
            );
            store
                .state_mut()
                .pending_task_owners
                .insert(key.to_string(), owner);
            store
                .state_mut()
                .pending_script_pages
                .insert(key.to_string(), 1);
            store
                .state_mut()
                .pending_script_selections
                .insert(key.to_string(), crate::state::ScriptSelection::default());
        }

        assert!(cancel_user_pending(&mut store, 1001));
        assert!(!store.state().pending_invites.contains_key(&1001));
        assert!(!store.state().pending_tasks.contains_key("owned"));
        assert!(!store.state().pending_task_owners.contains_key("owned"));
        assert!(!store.state().pending_script_pages.contains_key("owned"));
        assert!(
            !store
                .state()
                .pending_script_selections
                .contains_key("owned")
        );
        assert!(store.state().pending_tasks.contains_key("other"));
        assert!(store.state().pending_task_owners.contains_key("other"));
        assert!(!cancel_user_pending(&mut store, 1001));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reclaimed_task_callback_clears_stale_selector_state() {
        let path = std::env::temp_dir().join(format!(
            "koipy-rs-reclaimed-task-{}.json",
            std::process::id()
        ));
        let mut store = StateStore::open(&path).expect("state");
        store
            .state_mut()
            .pending_task_owners
            .insert("1:2".to_string(), 1001);
        store
            .state_mut()
            .pending_script_pages
            .insert("1:2".to_string(), 2);
        store
            .state_mut()
            .pending_script_selections
            .insert("1:2".to_string(), crate::state::ScriptSelection::default());

        assert!(!reclaim_pending_task(&mut store, "1:2"));
        assert!(!store.state().pending_task_owners.contains_key("1:2"));
        assert!(!store.state().pending_script_pages.contains_key("1:2"));
        assert!(!store.state().pending_script_selections.contains_key("1:2"));
        assert_eq!(QUERY_NOT_FOUND, "❌Button resource reclaimed");
        assert_eq!(OPERATION_TIMEOUT, "🗑️Operation timeout");
        assert_eq!(SLAVE_SELECTOR_TIMEOUT, "SlaveSelector timeout");
        assert_eq!(SORT_SELECTOR_TIMEOUT, "SortSelector timeout");

        let _ = std::fs::remove_file(path);
    }
}
