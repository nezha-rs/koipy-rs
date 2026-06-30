# koipy Rust Rewrite Progress

## Current Target

The public repository is koipy 1.0. The original Python code is mainly a Telegram frontend that fetches subscriptions, cleans Clash nodes, talks to MiaoSpeed over WebSocket, and renders test results. The 2026-06-15 closed linux amd64 package has also been unpacked for static resource/config/string-surface comparison.

This Rust rewrite now keeps the same product shape and also adds routed handlers for commands that were documented or shown in help text but not fully registered in koipy 1.0.

## Quantified Progress

| Area | Progress | Status |
|---|---:|---|
| Project understanding | 100% | koipy 1.0 source, current GitBook command/config/webhook docs, and the 2026-06-15 closed linux amd64 package resources have been mapped |
| Rust project skeleton | 100% | Cargo project, CLI, async runtime, and module boundaries are in place |
| Configuration system | 100% | Core YAML, license/log-level, closed-package callbacks, documented subscription.age, bot runtime options including `antiGroup`/`strictMode` camel-case aliases, structured runtime dns enable/nameserver injection, runtime entrance/ipstack/localip/enableDNSInject, image/watermark options, bot.commands objects, runtime/rule fields including multi-slave ids, subconverter mode/template/defaults, rule/slave healthCheck/showID/scheduling/script/webapi server config including TLS cert/key aliases are migrated |
| Subscription and node cleaning | 99% | URL parsing, `cacheTime` subscription caching, age X25519 ASCII armor decrypt, public-key request header, configurable protocol/HTTP subscription conversion templates, HTTP fetch, Clash YAML parsing, runtime DNS injection, regex filters, and runtime.speedNodes limits work |
| MiaoSpeed protocol | 99% | Request body, matrices, signing, documented WebSocket path, `skipCertVerify` TLS, backend HTTP proxy tunnel handling, WebSocket send, script content file loading for `resources/scripts` paths, Progress callback parsing, backend error handling, `taskRetry` retries, `taskTimeout`/`dnsServer`/`dnsServers`/`apiVersion`/upload options, default slave selection, active slave ping, UDP/topology/per-second result conversion, and runtime overrides are implemented |
| Telegram transport | 100% | Long polling, inbound document size guard with closed-package `file-too-large` reply, text/photo/document/video send, closed-package `parseMode` enum normalization, protectContent/notification options, image threshold send mode, leaveChat, callback answers, edit/delete, `autoResetCommands` `deleteMyCommands`/`setMyCommands` plus manual `/setcmd` with pinned custom command payloads, slave showID labels, paged script selector, multi-step inline keyboards, and local Bot API transport verification are implemented |
| Documented but missing commands | 100% | `/panel` has inline controls; `/reload` hot-swaps config; `/test` can prompt for slave, sort, paged script multi-select with cancel buttons and text `/cancel`, realtime progress edits, `output=json/video`, `duration/thread` overrides, `/user` lists local permission sources, `/grant` and `/ungrant` support UID and reply-to-message authorization targets, `/setcmd` manually publishes pinned custom commands to Telegram, `/lang`/`/language` reports the current language or switches to an existing configured language-pack file with closed-package status messages, `/checkslaves` renders closed-package style alive/offline connectivity reports, `/traffic` and `/subinfo` support saved user subscriptions plus closed-package tourist URL security/`httpProxy` gating, `/rule` supports documented URL creation/name lookup plus legacy list/show/delete, preset rule listing, and closed-package internal-keyword rejection, `/get`, `/set`, `/del`, closed-package `/demo` drawing preview, `/invite` shows closed-package style type selection with `attachToInvite` custom rules and built-in rule overrides, remembers selected invite rules, validates the next subscription URL, reuses invite blacklists, and consumes accepted URLs into the normal test flow, disabled custom commands report the closed-package command-disabled status, non-authorizing `/license` metadata display, documented custom command to rule mappings, and multi-slave rule/command execution work; activation-code authorization is intentionally not replicated |
| State and permissions | 99% | JSON state plus hot-reloaded config rules support subscriptions, saved rules, command-mapped config rules, persistent grants/ungrants, last task, pending task cancellation/cleanup with closed-package cancel labels, closed-package callback reclaimed/selector-timeout/unknown-callback messages with stale selector-state cleanup, task callback ownership for strictMode, inviteGroup permission override for `/invite`, expiring temporary invite grants and pending invite rule selections with stale-entry cleanup, closed-package `/checkslaves` concurrency lock message, invite blacklist domain/URL-list enforcement, `bypassMode` built-in command disablement with custom-command-only routing, fractional `echoLimit` throttling, Web API password auth/config mutation/TLS serving, anti-group service-message leave enforcement, and night-shift flags |
| Image rendering | 99% | PNG table renders configured background/line/font colors, end-color gradients, speed formats, invert mode, save cleanup, pixel threshold behavior, compression mode, UDP status, speed curve, topology footer, protocol logos, unsafe backend tips, commercial/non-commercial tiled watermark with trace UID, configured/system font fallback, emoji enable/source compatibility, and `output=video` with documented ffmpeg/image fallback |
| Testing and validation | 100% | `cargo test` passes; 111 unit tests cover routing, hot reload, closed-package config parsing, documented subscription.age/bot runtime/image/watermark/webapi/slave path/proxy/showID config, callbacks fallback and closed-package callback timeout/reclaimed/unknown-callback/cancel messages, inbound Telegram document `file-too-large` guard, Telegram reply-to-message grant/ungrant target parsing, Web API auth/health/config mutation/TLS validation, config persistence, language pack switching, `cacheTime`, age decrypt, fractional `echoLimit`, invite blacklist matching, temporary invite authorization expiry, pending invite rule-to-URL consumption and invalid/blacklisted URL rejection, invite `attachToInvite` keyboard/rule overrides, text `/cancel` cleanup, tourist subinfo `httpProxy` safety gating, documented rule create/show routing and closed-package preset/internal-keyword behavior, manual `/setcmd` command publishing, closed-package slave connectivity report sections, disabled custom-command routing, strictMode callback ownership, pending task cancel cleanup, inviteGroup override, `bypassMode` custom-only routing, antiGroup bot-join leave enforcement, `autoResetCommands` delete/setMyCommands payloads, `parseMode` enum normalization, DNS injection, Telegram send options and local Bot API POST verification, local subscription-to-MiaoSpeed WebSocket end-to-end execution, script content file loading, realtime progress parsing, result send thresholds, PNG compression, save cleanup, user permission listing, grants/ungrants, license metadata display, rule mappings, custom commands, demo drawing preview, paged keyboards, colors, logo/unsafe-tip/watermark/emoji/font selection, command options including `d`/`t` aliases and multi-slave selection, runtime defaults/overrides/`speedNodes` limits, subconverter templates, MiaoSpeed signing/path/`skipCertVerify`/proxy/new backend options handling, result merging, parsing, and rendering |

Current overall progress: 99%.

## Implemented Files

- `src/main.rs`: CLI entry, supports `progress`, `check`, `test`, `serve`
- `src/config.rs`: koipy YAML config model
- `src/cleaner.rs`: URL/protocol conversion, Clash YAML parsing, node regex filtering
- `src/subscription.rs`: subscription HTTP fetcher
- `src/task.rs`: task request and prepared task model
- `src/app.rs`: service layer and MiaoSpeed task execution
- `src/miaospeed.rs`: MiaoSpeed request model, matrices, signing, WebSocket call
- `src/result.rs`: MiaoSpeed result table conversion and sorting
- `src/bot.rs`: Telegram Bot API long polling, command routing, permissions, command handlers
- `src/state.rs`: JSON state store for subscriptions, last tasks, invites, and toggles
- `src/webhooks.rs`: GitBook-style `onMessage`, `onPreSend`, and `onResult` HTTP callbacks
- `src/image.rs`: basic PNG result renderer
- `src/progress.rs`: quantified progress report

## Remaining Work Toward 100%

1. Add live integration tests with an external real MiaoSpeed backend and a real Telegram sandbox bot account.
2. Polish image output against upstream/closed-package screenshots when reference screenshots are available.
3. Keep activation-code license authorization intentionally absent while preserving non-authorizing metadata/config display.
