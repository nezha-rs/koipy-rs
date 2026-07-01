# koipy-rs

koipy 1.0 的 Rust 重构版。

## 项目说明

本项目用 Rust 重写 koipy Telegram 机器人，覆盖内容包括：

- Telegram 机器人指令路由和长轮询
- 订阅抓取、URL / 协议转换、Clash YAML 解析
- MiaoSpeed WebSocket 后端请求与结果渲染
- 配置加载、热重载、状态持久化、权限、邀请、回调
- 速度图、拓扑图、连通性图的图片渲染
- 用于配置管理的 Web API 接口

激活码 license 授权逻辑不复刻。

## 环境要求

- Rust 稳定版工具链
- Telegram bot token
- 一份有效的 koipy 风格 YAML 配置
- 可选：MiaoSpeed 后端、Web API TLS 证书/私钥、订阅转换后端

## 下载二进制启动

Release 页面提供 Linux amd64 发布包，里面包含二进制和 `resources/`。普通用户推荐直接下载发布包启动，不需要在服务器上安装 Rust。

搭建 bot 的最短路径就是：准备 Telegram `api-id`、`api-hash` 和 `bot-token`，再填好管理员 `admin` 与至少一个 `slaveConfig.slaves` 后端，然后按下面步骤启动。

### 一键部署

下面这段会把发布包下载安装到 `/opt/koipy-rs`，生成配置文件，并注册成 `systemd` 服务：

```bash
sudo bash -c '
set -e
apt-get update
apt-get install -y wget tar
install_dir=/opt/koipy-rs
mkdir -p "$install_dir"
cd /tmp
wget -O koipy-rs-linux-amd64.tar.gz https://github.com/nezha-rs/koipy-rs/releases/latest/download/koipy-rs-linux-amd64.tar.gz
tar -xzf koipy-rs-linux-amd64.tar.gz
cp -r koipy-rs-linux-amd64/* "$install_dir"/
chmod +x "$install_dir"/koipy-rs-linux-amd64
if [ ! -f "$install_dir/config.yaml" ]; then
  cp "$install_dir/config.example.yaml" "$install_dir/config.yaml"
fi
cat >/etc/systemd/system/koipy-rs.service <<EOF
[Unit]
Description=koipy-rs
After=network-online.target

[Service]
Type=simple
WorkingDirectory=/opt/koipy-rs
ExecStart=/opt/koipy-rs/koipy-rs-linux-amd64 --config /opt/koipy-rs/config.yaml serve
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now koipy-rs
'
```

部署完成后，编辑 `/opt/koipy-rs/config.yaml`，至少填好 `bot.api-id`、`bot.api-hash`、`bot.bot-token`、`admin` 和 `slaveConfig.slaves`，然后执行：

```bash
sudo systemctl restart koipy-rs
sudo systemctl status koipy-rs
```

### 1. 下载

```bash
wget https://github.com/nezha-rs/koipy-rs/releases/latest/download/koipy-rs-linux-amd64.tar.gz
tar -xzf koipy-rs-linux-amd64.tar.gz
cd koipy-rs-linux-amd64
chmod +x koipy-rs-linux-amd64
```

### 2. 准备配置

复制示例配置并按需修改：

```bash
wget https://raw.githubusercontent.com/nezha-rs/koipy-rs/master/config.example.yaml -O config.yaml
nano config.yaml
```

最少要改这几项：

- `bot.api-id`
- `bot.api-hash`
- `bot.bot-token`
- `admin`
- `slaveConfig.slaves`

### 3. 启动前检查

```bash
./koipy-rs-linux-amd64 --config config.yaml check
```

### 4. 启动机器人

```bash
./koipy-rs-linux-amd64 --config config.yaml serve
```

### 5. 后台运行

`nohup`：

```bash
nohup ./koipy-rs-linux-amd64 --config config.yaml serve > koipy-rs.log 2>&1 &
```

`systemd`：

```ini
[Unit]
Description=koipy-rs
After=network-online.target

[Service]
Type=simple
WorkingDirectory=/opt/koipy-rs
ExecStart=/opt/koipy-rs/koipy-rs-linux-amd64 --config /opt/koipy-rs/config.yaml serve
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

保存为 `/etc/systemd/system/koipy-rs.service` 后执行：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now koipy-rs
sudo systemctl status koipy-rs
```

### 6. 首次搭建 bot 的检查顺序

1. 先确认 Telegram `api-id`、`api-hash`、`bot-token` 都已填写。
2. 再确认 `admin` 和至少一个 `slaveConfig.slaves` 后端已经配置好。
3. 先跑 `check`，没报错再跑 `serve`。
4. 如果要公开部署，优先用 `systemd` 托管。

## 从源码构建

```bash
cargo build --release
```

## 运行

源码运行时也建议显式指定配置文件：

```bash
cargo run -- --config config.example.yaml check
cargo run -- --config config.example.yaml progress
cargo run -- --config config.example.yaml serve
```

## CLI 命令

### `progress`
打印当前重构进度表。

### `check`
校验配置并输出运行摘要。

### `test <url>`
在不启动 Telegram 机器人的情况下，规范化并准备一个订阅或协议链接。

示例：

```bash
cargo run -- --config config.example.yaml test https://example.com/sub
cargo run -- --config config.example.yaml test vmess://example
cargo run -- --config config.example.yaml test https://example.com/sub --include "HK" --exclude "CN" --kind test
```

参数：

- `--include`：保留节点的正则过滤
- `--exclude`：排除节点的正则过滤
- `--kind`：`test`、`speed`、`analyze`、`topo`

### `serve`
启动机器人服务。

## 配置说明

主配置文件为 YAML，整体兼容 koipy 1.0 的配置面。

常见顶层字段：

- `admin`：管理员 UID
- `network`：代理与 UA
- `subscription`：age 解密配置
- `webapi`：内置配置 API
- `bot`：Telegram token、命令、机器人行为
- `image`：绘图与主题
- `runtime`：任务级默认值
- `scriptConfig`：脚本定义
- `slaveConfig`：后端定义
- `rules`：保存的规则
- `subconverter`：订阅转换后端
- `translation`：语言包
- `callbacks`：HTTP 回调
- `license`：兼容保留，但不实现激活
- `log-level`：日志级别
- `user`：已授权用户列表

## 示例配置

[`config.example.yaml`](./config.example.yaml) 提供了完整示例。

重点功能：

- `bot.commands` 支持自定义命令
- `runtime.dns` 支持结构化的 `enable` 和 `nameserver`
- `slaveConfig.slaves[].option.dnsServer` 兼容后端 DNS 列表
- `subconverter.template.backend` 支持 `$Host`、`$Port`、`$Target`、`$EncodedURL` 等占位符
- `translation.resources` 用于映射语言包文件

## 机器人命令

用户命令：

- `/test`
- `/speed`
- `/analyze` 或 `/topo`
- `/re`
- `/invite`
- `/share`
- `/new`
- `/sub`
- `/traffic`
- `/subinfo`
- `/checkslaves`
- `/demo`

管理员命令：

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
- `/lang` 或 `/language`
- `/rule`
- `/get`
- `/set`
- `/del`

## 常见使用流程

### 1. 首次启动

1. 复制一份配置文件。
2. 填好 `bot.bot-token`。
3. 至少配置一个 `slaveConfig.slaves` 后端。
4. 先执行 `cargo run -- --config <你的配置> check`。
5. 再执行 `cargo run -- --config <你的配置> serve`。

### 2. 手动测试订阅

```bash
cargo run -- --config config.example.yaml test https://example.com/sub
```

需要过滤时：

```bash
cargo run -- --config config.example.yaml test https://example.com/sub --include "HK|JP" --exclude "CN"
```

### 3. 使用协议链接

当 `subconverter.enable = true` 时，`vmess://`、`vless://`、`tuic://`、`trojan://` 等协议链接可以通过模板转换。

### 4. 启用 Web API

设置 `webapi.enable = true`，配置 `webapi.password`，并按需设置 `webapi.tls`、`webapi.certPath`、`webapi.keyPath`。

### 5. 使用脚本

脚本既可以内联写，也可以引用文件。`scriptConfig.scripts[].content` 如果是文件路径，会按配置文件所在目录解析。

示例：

```yaml
scriptConfig:
  scripts:
    - type: gojajs
      name: OpenAI
      rank: 0
      content: resources/scripts/builtin/openai.js
```

## 兼容性说明

- 项目刻意保持与 koipy 1.0 的配置和行为面兼容。
- license 激活逻辑不实现。
- 发布仓库里不包含临时调试产物和解包出来的闭源文件。

## 开发

运行测试：

```bash
cargo test
```

格式化代码：

```bash
cargo fmt
```

## 仓库结构

- `src/`：实现代码
- `resources/`：字体、图片、脚本、语言包和证书资源
- `config.example.yaml`：示例配置
- `Cargo.toml` / `Cargo.lock`：Rust 包元数据
