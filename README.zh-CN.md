# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**语言 / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

基于 Rust 的 Pixiv Telegram 机器人。

## 功能特性

- **作者订阅**：订阅 Pixiv 画师，自动获取新作品更新通知。
- **排行榜订阅**：订阅 Pixiv 日榜、周榜或月榜。
- **Pixiv 链接检测**：自动检测消息中的 Pixiv 作品和用户链接。
  - 作品链接：发送完整图片。
  - 用户链接：提供快速订阅选项。
- **智能图片处理**：
  - 自动将多张图片组合成相册。
  - 缓存图片以减少服务器负载和 Pixiv API 调用。
  - 支持对敏感内容（R-18、NSFW）进行模糊处理。
- **灵活的调度**：随机化轮询间隔，模拟真人用户行为，避免触发速率限制。
- **访问控制**：
  - 管理员/所有者角色，用于管理群组聊天中的机器人。
  - 可配置"私有"（仅邀请）或"公开"模式。
- **可自定义**：
  - 每个聊天的敏感标签设置。
  - 可配置的缓存和日志记录。

## 安装与使用

我们推荐使用 Docker 进行部署，因为它会自动处理依赖项和环境设置。

### 使用 Docker Compose（推荐）

1. 克隆仓库：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. 配置机器人：

   ```bash
   cp config.toml.example config.toml
   # 编辑 config.toml 填入你的令牌
   ```

3. 使用 Docker Compose 启动：

   ```bash
   docker compose up -d
   ```

   （根目录中包含 `docker-compose.yml` 文件）

### 从源码构建

如果你更喜欢直接在机器上运行：

1. **前置要求**：
    - Rust（最新稳定版）
    - SQLite

2. **克隆并配置**：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # 编辑 config.toml 填入你的凭证
   ```

3. **构建并运行**：

   ```bash
   cargo run --release
   ```

## 获取所需令牌

在配置机器人之前，你需要获取两个必需的令牌：

### 1. Telegram Bot Token

1. 在 Telegram 中搜索 [@BotFather](https://t.me/BotFather)
2. 发送 `/newbot` 并按照指示操作
3. 为你的机器人选择名称和用户名
4. 你将收到类似 `123456789:ABCdefGHIjklMNOpqrsTUVwxyz` 的令牌
5. 将此令牌复制到 `config.toml` 的 `telegram.bot_token` 字段

### 2. Pixiv Refresh Token

**推荐方法**：使用 [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

获取 refresh token 后，将其复制到 `config.toml` 的 `pixiv.refresh_token` 字段。

⚠️ **重要提示**：请妥善保管你的令牌，切勿将其提交到版本控制系统！

## 配置

`config.toml` 或环境变量（前缀 `PIX__`，使用双下划线）支持的配置选项：

| 配置键 | 环境变量 | 说明 | 默认值 |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | 所有者用户 ID | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` 或 `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | 数据库连接 URL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | 日志级别（info、debug、warn） | `"info"` |
| `scheduler.cache_retention_days` | - | 缓存保留天数 | `7` |

## 命令

### 用户命令

- `/start` - 启动机器人
- `/help` - 显示帮助信息
- `/sub <id,...> [+tag1 -tag2]` - 订阅画师
- `/subrank <mode>` - 订阅排行榜（daily、weekly、monthly）
- `/unsub <id,...>` - 取消订阅画师
- `/unsubrank <mode>` - 取消订阅排行榜
- `/list` - 列出活跃的订阅
- `/settings` - 显示当前聊天设置
- `/blursensitive <on|off>` - 启用/禁用敏感内容的模糊处理
- `/sensitivetags <tag,...>` - 设置自定义敏感标签
- `/clearsensitivetags` - 清除敏感标签
- `/excludetags <tag,...>` - 设置排除标签（带有这些标签的图片将不会被发送）
- `/clearexcludedtags` - 清除排除标签

### 管理员命令

- `/enablechat [chat_id]` - 在聊天中启用机器人（如果处于私有模式）
- `/disablechat [chat_id]` - 在聊天中禁用机器人

### 所有者命令

- `/setadmin <user_id>` - 将用户提升为管理员
- `/unsetadmin <user_id>` - 将管理员降级为用户
- `/info` - 显示机器人系统状态

## 贡献

有关开发指南，请参阅 [CONTRIBUTING.md](CONTRIBUTING.md)。

### 手动合并 PR 冲突示例（以 #25 为例，可替换为目标 PR 编号）

1. 切换到主分支并更新：`git checkout master && git pull origin master`。
2. 拉取并检出 PR 分支（将 `<PR_NUMBER>` 换成目标 PR，例如 #25）：`git fetch origin pull/<PR_NUMBER>/head:pr-<PR_NUMBER> && git checkout pr-<PR_NUMBER>`。
3. 合并主分支：`git merge master`。
4. 如果出现冲突，打开带有 `<<<<<<<` 标记的文件，按期望保留内容并删除冲突标记（`<<<<<<<`/`=======`/`>>>>>>>`）。
5. 使用 `git status` 确认冲突已处理，随后 `git add <文件>` 并 `git commit` 记录解决。
6. 运行基本校验（如 `make quick` 或 `make ci`）确保编译和测试通过。
7. 推送回 PR 分支：`git push origin pr-<PR_NUMBER>`，GitHub 会自动更新对应 PR（如 #25）。

## 许可证

[MIT](LICENSE)

## 致谢

- **[PixivPy](https://github.com/upbit/pixivpy)**：Pixiv API 实现的重要参考。没有他们的工作，这个项目就不可能实现。
- **AI 助手**：特别感谢 GitHub Copilot、Claude 和 Gemini 在开发过程中提供的技术支持和代码生成。
