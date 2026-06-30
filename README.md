# koipy-rs

Rust rewrite of koipy 1.0.

## What this is

This project reimplements the koipy Telegram bot in Rust, including:

- Telegram bot command routing and long polling
- subscription fetching, URL/protocol conversion, and Clash YAML parsing
- MiaoSpeed WebSocket backend requests and result rendering
- config loading, hot reload, state persistence, permissions, invites, and callbacks
- image/table rendering for speed, topology, and connectivity reports
- web API endpoints for config management

License activation handling is intentionally not replicated.

## Requirements

- Rust toolchain (stable)
- A Telegram bot token
- A valid koipy-style YAML config file
- Optional: a MiaoSpeed backend, optional web API TLS cert/key, optional subscription converter backend

## Build

```bash
cargo build --release
```

## Run

Use a config file path explicitly:

```bash
cargo run -- --config config.example.yaml check
cargo run -- --config config.example.yaml progress
cargo run -- --config config.example.yaml serve
```

## CLI commands

### `progress`
Prints the current rewrite progress table.

### `check`
Validates the config and prints a short runtime summary.

### `test <url>`
Normalizes and prepares a subscription or protocol URL without starting Telegram.

Examples:

```bash
cargo run -- --config config.example.yaml test https://example.com/sub
cargo run -- --config config.example.yaml test vmess://example
cargo run -- --config config.example.yaml test https://example.com/sub --include "HK" --exclude "CN" --kind test
```

Flags:

- `--include`: regex filter for keeping nodes
- `--exclude`: regex filter for dropping nodes
- `--kind`: `test`, `speed`, `analyze`, or `topo`

### `serve`
Starts the bot service.

## Configuration

The main config file is YAML and follows the koipy surface.

Important top-level sections:

- `admin`: admin user IDs
- `network`: proxy and user agent settings
- `subscription`: age-decryption config
- `webapi`: embedded config API
- `bot`: Telegram bot token, commands, runtime bot behavior
- `image`: rendering and theme settings
- `runtime`: task-level defaults
- `scriptConfig`: script definitions
- `slaveConfig`: backend definitions
- `rules`: saved rules
- `subconverter`: subscription conversion backend
- `translation`: language packs
- `callbacks`: HTTP hooks
- `license`: kept for compatibility, but activation is not implemented
- `log-level`: log verbosity
- `user`: granted users list

## Example config

[`config.example.yaml`](./config.example.yaml) contains a full sample with the supported surface.

Highlights:

- `bot.commands` supports custom command entries
- `runtime.dns` supports structured `enable` and `nameserver`
- `slaveConfig.slaves[].option.dnsServer` is accepted as an alias for the backend DNS list
- `subconverter.template.backend` supports placeholder expansion like `$Host`, `$Port`, `$Target`, `$EncodedURL`
- `translation.resources` maps language keys to YAML files

## Bot commands

User commands:

- `/test`
- `/speed`
- `/analyze` or `/topo`
- `/re`
- `/invite`
- `/share`
- `/new`
- `/sub`
- `/traffic`
- `/subinfo`
- `/checkslaves`
- `/demo`

Admin commands:

- `/system`
- `/user`
- `/remove`
- `/reload`
- `/setantigroup`
- `/restart`
- `/panel`
- `/license`
- `/logs`
- `killme`
- `/grant`
- `/ungrant`
- `/setcmd`
- `/lang` or `/language`
- `/rule`
- `/get`
- `/set`
- `/del`

## Common workflows

### 1. First launch

1. Create or copy a config file.
2. Set `bot.bot-token`.
3. Set at least one backend under `slaveConfig.slaves`.
4. Run `cargo run -- --config <your-config> check`.
5. Start the service with `cargo run -- --config <your-config> serve`.

### 2. Testing a subscription manually

```bash
cargo run -- --config config.example.yaml test https://example.com/sub
```

Add filters if needed:

```bash
cargo run -- --config config.example.yaml test https://example.com/sub --include "HK|JP" --exclude "CN"
```

### 3. Using a protocol link

If `subconverter.enable = true`, protocol links like `vmess://`, `vless://`, `tuic://`, and `trojan://` can be converted through the configured template.

### 4. Running with a custom web API

Set `webapi.enable = true`, configure `webapi.password`, and optionally set `webapi.tls`, `webapi.certPath`, and `webapi.keyPath`.

### 5. Using scripts

Scripts can be written inline or loaded from files. File paths in `scriptConfig.scripts[].content` are resolved relative to the config file.

Example:

```yaml
scriptConfig:
  scripts:
    - type: gojajs
      name: OpenAI
      rank: 0
      content: resources/scripts/builtin/openai.js
```

## Notes on compatibility

- The project is intentionally compatible with the koipy 1.0 surface.
- License activation is intentionally absent.
- Temporary debug artifacts and extracted closed-package files are not part of the published repo.

## Development

Run tests:

```bash
cargo test
```

Format, if needed:

```bash
cargo fmt
```

## Repository layout

- `src/`: implementation
- `config.example.yaml`: sample config
- `RUST_REWRITE_PROGRESS.md`: quantitative rewrite status
- `Cargo.toml` / `Cargo.lock`: Rust package metadata

## Why the progress file exists

`RUST_REWRITE_PROGRESS.md` tracks the rewrite by subsystem and keeps the current quantified state visible while work continues.
