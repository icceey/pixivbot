# PixivBot

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
- `/settings` - Show current chat settings
- `/blursensitive <on|off>` - Enable/disable spoiler for sensitive content
- `/sensitivetags <tag,...>` - Set custom sensitive tags
- `/clearsensitivetags` - Clear sensitive tags
- `/excludetags <tag,...>` - Set excluded tags (images with these tags won't be sent)
- `/clearexcludedtags` - Clear excluded tags

### Admin Custom Commands

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
