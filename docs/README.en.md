# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

[简体中文](../README.md) | English | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

Rust based Pixiv Telegram bot.

## Features

- **Author Subscription**: Subscribe to Pixiv artists and get automatic updates for new illustrations.
- **Ranking Subscription**: Subscribe to daily, weekly, or monthly Pixiv rankings.
- **Pixiv Link Detection**: Automatically detects Pixiv illustration and user links in messages.
  - Sends full images for illustration links.
  - Offers quick subscription for user links.
- **Smart Image Handling**:
  - Automatically groups multiple images into albums.
  - Caches images to reduce server load and Pixiv API calls.
  - Supports spoiler blurring for sensitive content (R-18, NSFW).
- **Flexible Scheduling**: Randomized polling intervals to behave more like a human user and avoid rate limits.
- **Access Control**:
  - Admin/Owner roles for managing the bot in group chats.
  - Configurable "Private" (invite-only) or "Public" modes.
- **Customizable**:
  - Per-chat settings for sensitive tags.
  - Configurable caching and logging.

## Installation & Usage

We recommend using Docker for deployment as it handles dependencies and environment setup automatically.

### Using Docker Compose (Recommended)

1. Clone the repository:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. Configure the bot:

   ```bash
   cp config.toml.example config.toml
   # Edit config.toml with your tokens
   ```

3. Start with Docker Compose:

   ```bash
   docker compose up -d
   ```

   (A `docker-compose.yml` file is included in the root directory)

### Build from Source

If you prefer running directly on your machine:

1. **Prerequisites**:
    - Rust (latest stable)
    - SQLite

2. **Clone and Configure**:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # Edit config.toml with your credentials
   ```

3. **Build and Run**:

   ```bash
   cargo run --release
   ```

## Getting Required Tokens

Before configuring the bot, you need to obtain two essential tokens:

### 1. Telegram Bot Token

1. Open Telegram and search for [@BotFather](https://t.me/BotFather)
2. Send `/newbot` and follow the instructions
3. Choose a name and username for your bot
4. You'll receive a token like `123456789:ABCdefGHIjklMNOpqrsTUVwxyz`
5. Copy this token to `config.toml` under `telegram.bot_token`

### 2. Pixiv Refresh Token

**Recommended Method**: Use [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

Once you have the refresh token, copy it to `config.toml` under `pixiv.refresh_token`.

⚠️ **Important**: Keep your tokens secure and never commit them to version control!

## Configuration

Supported configuration options in `config.toml` or via Environment Variables (prefix `PIX__` with double underscores):

| Config Key | Env Var | Description | Default |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | Owner User ID | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` or `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | Database Connection URL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | Log Level (info, debug, warn) | `"info"` |
| `scheduler.cache_retention_days` | - | Cache retention (days) | `7` |

## Commands

### User Commands

- `/start` - Start the bot
- `/help` - Show help message
- `/sub <id,...> [+tag1 -tag2]` - Subscribe to an artist
- `/subrank <mode>` - Subscribe to a ranking (daily, weekly, monthly)
- `/unsub <id,...>` - Unsubscribe from an artist
- `/unsubrank <mode>` - Unsubscribe from a ranking
- `/list` - List active subscriptions
- `/settings` - Show and manage chat settings (interactive UI with inline buttons)
  - Toggle blur for sensitive content
  - Edit sensitive tags
  - Edit excluded tags
- `/cancel` - Cancel current settings operation
- `/download <url|id>` - Download original images (or reply to a message)

### Admin Commands

- `/enablechat [chat_id]` - Enable bot in a chat (if in private mode)
- `/disablechat [chat_id]` - Disable bot in a chat

### Owner Commands

- `/setadmin <user_id>` - Promote user to Admin
- `/unsetadmin <user_id>` - Demote Admin to User
- `/info` - Show bot system status

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development guidelines.

## License

[MIT](LICENSE)

## Acknowledgements

- **[PixivPy](https://github.com/upbit/pixivpy)**: Critical reference for Pixiv API implementation. This project would not be possible without their work.
- **AI Assistance**: Special thanks to GitHub Copilot, Claude, and Gemini for their technical support and code generation during development.
