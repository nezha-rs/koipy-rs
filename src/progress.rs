#[derive(Debug, Clone)]
pub struct ProgressItem {
    pub area: &'static str,
    pub percent: u8,
    pub note: &'static str,
}

#[derive(Debug, Clone)]
pub struct ProgressReport {
    items: Vec<ProgressItem>,
}

impl ProgressReport {
    pub fn current() -> Self {
        Self {
            items: vec![
                ProgressItem {
                    area: "Project understanding",
                    percent: 100,
                    note: "koipy 1.0 source, current GitBook command/config/webhook docs, and the 2026-06-15 closed linux amd64 package resources have been mapped",
                },
                ProgressItem {
                    area: "Rust project skeleton",
                    percent: 100,
                    note: "Cargo project, CLI, async runtime, and module boundaries are in place",
                },
                ProgressItem {
                    area: "Configuration system",
                    percent: 100,
                    note: "Core YAML, license/log-level, closed-package callbacks, documented subscription.age, bot runtime options including antiGroup/strictMode camel-case aliases, structured runtime dns enable/nameserver injection, runtime entrance/ipstack/localip/enableDNSInject, image/watermark options, bot.commands objects, runtime/rule fields including multi-slave ids, subconverter mode/template/defaults, rule/slave healthCheck/showID/scheduling/script/webapi server config including TLS cert/key aliases are migrated",
                },
                ProgressItem {
                    area: "Subscription and node cleaning",
                    percent: 99,
                    note: "URL parsing, cacheTime subscription caching, age X25519 ASCII armor decrypt, public-key request header, configurable protocol/HTTP subscription conversion templates, HTTP fetch, Clash YAML parsing, runtime DNS injection, regex filters, and runtime.speedNodes limits work",
                },
                ProgressItem {
                    area: "MiaoSpeed protocol",
                    percent: 99,
                    note: "Request body, matrices, signing, documented WebSocket path, skipCertVerify TLS, backend HTTP proxy tunnel handling, WebSocket send, script content file loading for resources/scripts paths, Progress callback parsing, backend error handling, taskRetry retries, taskTimeout/dnsServer/dnsServers/apiVersion/upload options, default slave selection, active slave ping, UDP/topology/per-second result conversion, and runtime overrides are implemented",
                },
                ProgressItem {
                    area: "Telegram transport",
                    percent: 100,
                    note: "Long polling, inbound document size guard with closed-package file-too-large reply, text/photo/document/video send, closed-package parseMode enum normalization, protectContent/notification options, image threshold send mode, leaveChat, callback answers, edit/delete, autoResetCommands deleteMyCommands/setMyCommands plus manual /setcmd with pinned custom command payloads, slave showID labels, paged script selector, multi-step inline keyboards, and local Bot API transport verification are implemented",
                },
                ProgressItem {
                    area: "Documented but missing commands",
                    percent: 100,
                    note: "/panel has inline controls; /reload hot-swaps config; /test can prompt for slave, sort, paged script multi-select with cancel buttons and text /cancel, realtime progress edits, output=json/video, duration/thread overrides, /user lists local permission sources, /grant and /ungrant support UID and reply-to-message authorization targets, /setcmd manually publishes pinned custom commands to Telegram, /lang and /language report the current language or switch to an existing configured language-pack file with closed-package status messages, /checkslaves renders closed-package style alive/offline connectivity reports, /traffic and /subinfo support saved user subscriptions plus closed-package tourist URL security/httpProxy gating, /rule supports documented URL creation/name lookup plus legacy list/show/delete, preset rule listing, and closed-package internal-keyword rejection, /get, /set, /del, closed-package /demo drawing preview, /invite shows closed-package style type selection with attachToInvite custom rules and built-in rule overrides, remembers selected invite rules, validates the next subscription URL, reuses invite blacklists, and consumes accepted URLs into the normal test flow, disabled custom commands report the closed-package command-disabled status, non-authorizing /license metadata display, documented custom command to rule mappings, and multi-slave rule/command execution work; activation-code authorization is intentionally not replicated",
                },
                ProgressItem {
                    area: "State and permissions",
                    percent: 99,
                    note: "JSON state plus hot-reloaded config rules support subscriptions, saved rules, command-mapped config rules, persistent grants/ungrants, last task, pending task cancellation/cleanup with closed-package cancel labels, closed-package callback reclaimed/selector-timeout/unknown-callback messages with stale selector-state cleanup, task callback ownership for strictMode, inviteGroup permission override for /invite, expiring temporary invite grants and pending invite rule selections with stale-entry cleanup, closed-package /checkslaves concurrency lock message, closed-package /traffic and /subinfo summary fields, invite blacklist domain/URL-list enforcement, bypassMode built-in command disablement with custom-command-only routing, fractional echoLimit throttling, Web API password auth/config mutation/TLS serving, anti-group service-message leave enforcement, and night-shift flags",
                },
                ProgressItem {
                    area: "Image rendering",
                    percent: 99,
                    note: "PNG table renders configured background/line/font colors, end-color gradients, speed formats, invert mode, save cleanup, pixel threshold behavior, compression mode, UDP status, speed curve, topology footer, protocol logos, unsafe backend tips, commercial/non-commercial tiled watermark with trace UID, configured/system font fallback, emoji enable/source compatibility, and output=video with documented ffmpeg/image fallback",
                },
                ProgressItem {
                    area: "Testing and validation",
                    percent: 100,
                    note: "cargo test passes; 111 unit tests cover routing, hot reload, closed-package config parsing, documented subscription.age/bot runtime/image/watermark/webapi/slave path/proxy/showID config, callbacks fallback and closed-package callback timeout/reclaimed/unknown-callback/cancel messages, inbound Telegram document file-too-large guard, Telegram reply-to-message grant/ungrant target parsing, Web API auth/health/config mutation/TLS validation, config persistence, language pack switching, cacheTime, age decrypt, fractional echoLimit, invite blacklist matching, temporary invite authorization expiry, pending invite rule-to-URL consumption and invalid/blacklisted URL rejection, invite attachToInvite keyboard/rule overrides, text /cancel cleanup, tourist subinfo httpProxy safety gating, documented rule create/show routing and closed-package preset/internal-keyword behavior, manual /setcmd command publishing, closed-package slave connectivity report sections, disabled custom-command routing, strictMode callback ownership, pending task cancel cleanup, inviteGroup override, bypassMode custom-only routing, antiGroup bot-join leave enforcement, autoResetCommands delete/setMyCommands payloads, parseMode enum normalization, DNS injection, Telegram send options and local Bot API POST verification, local subscription-to-MiaoSpeed WebSocket end-to-end execution, script content file loading, realtime progress parsing, result send thresholds, PNG compression, save cleanup, user permission listing, grants/ungrants, license metadata display, rule mappings, custom commands, demo drawing preview, paged keyboards, colors, logo/unsafe-tip/watermark/emoji/font selection, command options including d/t aliases and multi-slave selection, runtime defaults/overrides/speedNodes limits, subconverter templates, MiaoSpeed signing/path/skipCertVerify/proxy/new backend options handling, result merging, parsing, and rendering",
                },
            ],
        }
    }

    pub fn overall(&self) -> u8 {
        let total: u16 = self.items.iter().map(|item| item.percent as u16).sum();
        (total / self.items.len() as u16).min(100) as u8
    }

    pub fn render_markdown(&self) -> String {
        let mut out = format!("Rust rewrite overall progress: {}%\n\n", self.overall());
        out.push_str("| Area | Progress | Notes |\n|---|---:|---|\n");
        for item in &self.items {
            out.push_str(&format!(
                "| {} | {}% | {} |\n",
                item.area, item.percent, item.note
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_overall_does_not_round_up_to_complete() {
        let report = ProgressReport::current();
        assert_eq!(report.overall(), 99);
        assert!(
            report
                .render_markdown()
                .starts_with("Rust rewrite overall progress: 99%")
        );
    }
}
