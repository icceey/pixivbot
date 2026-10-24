# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.94%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

简体中文 | [English](docs/README.en.md) | [繁體中文](docs/README.zh-TW.md) | [文言文](docs/README.lzh.md) | [Esperanto](docs/README.eo.md) | [Ελληνικά](docs/README.el.md) | [संस्कृतम्](docs/README.sa.md) | [火星语](docs/README.mars.md) | [Kiswahili](docs/README.sw.md)

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
    - Rust 1.94（使用 `rust-toolchain.toml` 指定版本）
    - SQLite
    - FFmpeg 开发库和 `pkg-config`（启用 ugoira 转码时需要）

2. **克隆并配置**：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # 编辑 config.toml 填入你的凭证
   ```

3. **构建并运行**：

   ```bash
   cargo run --release --features ffmpeg-codec
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
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | 所有者用户 ID；未配置时首位交互用户可成为所有者 | 未设置 |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` 或 `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | 数据库连接 URL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | 日志级别（info、debug、warn） | `"info"` |
| `scheduler.cache_retention_days` | - | 缓存保留天数 | `7` |

## 命令

### 用户命令

- `/start` - 启动机器人
- `/help` - 显示帮助信息
- `/info` - 显示机器人系统状态，启用 EH 时附带最近 24 小时／7 天的下载流量和 GP 消耗；已启用的私聊和群聊可用，管理员可直接私聊使用
- `/sub <id,...> [+tag1 -tag2]` - 订阅画师
- `/subrank <mode> [+tag1 -tag2]` - 订阅排行榜（如 day、week、month）
- `/subtag <标签搜索词> [b=500] [l=<图片数>] [+tag1 -tag2]` - 订阅 Pixiv 标签近似热榜
- `/unsubtag <标签搜索词>` - 取消标签热榜订阅
- `/unsub <id,...>` - 取消订阅画师
- `/unsubrank <mode>` - 取消订阅排行榜
- `/list` - 列出活跃的订阅
- `/settings` - 显示和管理聊天设置（交互式界面，带有内联按钮）
  - 切换敏感内容模糊
  - 编辑敏感标签
  - 编辑排除标签
- `/cancel` - 取消当前设置操作
- `/download <url|id>` - 下载原图（或回复消息）

Pixiv 标记为 AI 生成的作品会在标签首位显示 `#AI生成`，并参与标签过滤和敏感遮罩匹配：

- `/sub 123456 -AI生成` 或 `/subrank day -AI生成`：订阅排除 AI 作品。
- `/sub 123456 +AI生成`：订阅仅保留匹配 `AI生成` 的作品。
- 在 `/settings` 的排除标签中添加 `AI生成`：当前聊天的订阅统一排除；手动发送作品链接仍会推送。
- 在 `/settings` 的敏感标签中添加 `AI生成` 并启用敏感内容模糊：对匹配作品应用遮罩，包含手动链接推送。

原始标签中已有的 `AI生成` 也会参与匹配。多个 `+标签` 匹配任意一个即可，`-标签` 和聊天排除标签优先。未指定 AI 属性的作品不会自动补标；此标记不用于保证其他作品完全未使用 AI。

排除规则在订阅筛选新作品时生效。已进入续传流程的画师作品会沿用现有重试逻辑，不会因中途修改排除标签而停止续传。

`/subtag 原神 b=300 l=10 -AI生成` 订阅该标签下收藏量达到 300、图片不超过 10 张的近期作品。`b` / `bookmarks` 为收藏阈值，省略时采用 `scheduler.pixiv_tag_min_bookmarks`（默认 500）；`l` / `limit` 为图片数量上限，省略时不限数量，两者均须为正整数。重复订阅可修改阈值、数量上限和标签过滤。频道订阅在参数前加 `ch=<频道ID>`。搜索词按 Pixiv 的精确标签规则查询，不同语言的搜索词分别订阅；`+tag` / `-tag` 匹配作品原始标签及 AI 标记。

标签订阅默认周期性扫描最近 7 天的作品，观察窗口和扫描频率在 [config.toml.example](config.toml.example) 的 `[scheduler]` 中配置。首次订阅也会推送窗口内已达标作品，热门标签可能产生较多消息。

画师和标签订阅在同一聊天内按作品去重，取消重订后保留发送记录；日榜、周榜等榜单独立推送。每个作品单独发送，多图每组最多 10 张。发送失败自动重试，重启后从已保存的进度继续。

### 聊天设置

`/settings` 管理敏感内容模糊、敏感标签和排除标签。标签编辑面板第一行为“追加 / 删减”，第二行为“覆盖 / 清空”；选择后在同一条消息中显示当前标签与操作提示。

| 操作 | 效果 |
| --- | --- |
| 追加 | 保留现有标签，按完整文本去重后添加，保持首次出现顺序 |
| 删减 | 删除完全匹配的标签，忽略不存在的项 |
| 覆盖 | 用输入替换完整列表 |
| 清空 | 点击“确认清空”后删除全部标签 |

输入支持中英文逗号分隔，去除首尾空白并忽略空项；空输入不修改列表，`clear` 也是普通标签。追加和删减使用提交时的最新列表。长列表仅截断展示，实际操作使用完整数据。

模式选择、输入和清空确认各限3分钟；重复点击不延长时限。输入和确认均提供“取消”按钮，也可发送 `/cancel` 或重开 `/settings`。会话仅限发起者在当前聊天使用，旧按钮无效；群管理员提交时重新鉴权。未过期会话中的 `/cancel`、`/settings` 可省略 `@bot` 后缀。禁用群聊后，旧设置面板会提示先启用群聊，已有会话仍可用 `/cancel` 取消。

### 管理员命令

- `/enablechat [chat_id]` - 在聊天中启用机器人（如果处于私有模式）
- `/disablechat [chat_id]` - 在聊天中禁用机器人

### 所有者命令

- `/setadmin <user_id|@username>` - 设置全局 Bot Admin
- `/unsetadmin <user_id|@username>` - 取消全局 Bot Admin
- 仅 Owner 可执行，不能修改 Owner。支持不带参数回复用户消息，显式参数优先；转发消息以实际发送者为目标。
- 回复真人消息的 `/setadmin` 可自动建档；数字 ID、用户名和 `/unsetadmin` 要求目标已有记录。已有用户的用户名不会被历史回复覆盖。

### 部署与权限范围

群主和 Telegram 群管理员自动获得当前群的管理权限：启用/禁用该群、修改 `/settings`，以及在该群使用管理员下载按钮权限。此权限不写入数据库全局角色、不授予私聊权限，也不能管理其他群或设置全局管理员。全局 Bot Admin 和 Owner 可跨聊天管理。`/unsetadmin` 只取消全局角色，不会撤销 Telegram 本身的群管理身份。

将机器人设为群管理员以确保 [getChatMember](https://core.telegram.org/bots/api#getchatmember) 能可靠查询其他成员。每次受保护操作实时查询；API 查询失败时不授予群权限，无缓存授权。匿名管理员、`sender_chat`（包括以频道身份发送）、机器人发送者均不被当作真实操作者；请切换为个人身份。正常消息的群启用与 @bot 要求保持不变，启停命令仍可在禁用群使用。

Bot API 不支持按任意个人 @username 查询用户 ID。用户名大小写不敏感，数据库只提供候选 ID；群内通过 `getChatMember` 核对当前 username，私聊通过数字 ID 调用 `getChat` 核对当前私聊 username。未知、已变更、重复歧义或无法验证的用户名明确报错，不猜测 ID、不使用转发来源。用户名改变后，用户应先与机器人交互刷新记录；也可使用数字 ID 或回复实际用户消息。

`[telegram].owner_id` 是可选数字 ID，在用户首次交互建档时用于识别 Owner；部署时建议明确配置。

## 贡献

有关开发指南，请参阅 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

[MIT](LICENSE)

## 致谢

- **[PixivPy](https://github.com/upbit/pixivpy)**：Pixiv API 实现的重要参考。没有他们的工作，这个项目就不可能实现。
- **AI 助手**：特别感谢 GitHub Copilot、Claude 和 Gemini 在开发过程中提供的技术支持和代码生成。
