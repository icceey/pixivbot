# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**語言 / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

以 Rust 之術，造 Pixiv Telegram 機關之器。

## 功用

- **畫師訂閱**：訂 Pixiv 畫師，新作自至。
- **榜訂閱**：訂 Pixiv 日榜、週榜、月榜。
- **Pixiv 鏈接偵測**：自偵訊息中 Pixiv 作品及使用者之鏈接。
  - 作品鏈接：傳完整圖像。
  - 使用者鏈接：供快速訂閱之選。
- **智圖像處理**：
  - 自組多圖為冊。
  - 存圖像以減伺服器負荷及 Pixiv API 之呼。
  - 支援敏感內容（R-18、NSFW）之模糊處理。
- **靈活調度**：隨機輪詢間隔，擬真人之行，避速率限制。
- **訪問控制**：
  - 管理員/擁有者之角色，以管群組聊天中之機關。
  - 可設「私」（僅邀）或「公」之模式。
- **可自定**：
  - 每聊天之敏感標籤設置。
  - 可設之存儲及日誌。

## 安裝與用法

吾輩薦以 Docker 佈署，蓋其自理依存及環境設置也。

### 用 Docker Compose（薦）

1. 複製庫藏：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. 設機關：

   ```bash
   cp config.toml.example config.toml
   # 編 config.toml 填汝之令符
   ```

3. 以 Docker Compose 啟之：

   ```bash
   docker compose up -d
   ```

   （根目錄含 `docker-compose.yml` 檔）

### 從源構建

若汝欲直接於機上行之：

1. **前置要求**：
    - Rust（最新穩定版）
    - SQLite

2. **複製並設**：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # 編 config.toml 填汝之憑據
   ```

3. **構建並行**：

   ```bash
   cargo run --release
   ```

## 取所需令符

於設機關之前，須得二必需之令符：

### 1. Telegram Bot Token

1. 於 Telegram 搜 [@BotFather](https://t.me/BotFather)
2. 傳 `/newbot` 並循指示
3. 為汝機關擇名及使用者名
4. 汝將得如 `123456789:ABCdefGHIjklMNOpqrsTUVwxyz` 之令符
5. 將此令符抄至 `config.toml` 之 `telegram.bot_token` 欄

### 2. Pixiv Refresh Token

**薦法**：用 [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

得 refresh token 後，抄至 `config.toml` 之 `pixiv.refresh_token` 欄。

⚠️ **要**：妥保汝令符，勿提交至版本控制也！

## 設定

`config.toml` 或環境變數（前綴 `PIX__`，用雙底線）所支援之設定：

| 設定鍵 | 環境變數 | 說明 | 預設值 |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | 擁有者使用者 ID | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` 或 `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | 資料庫連線 URL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | 日誌級別（info、debug、warn） | `"info"` |
| `scheduler.cache_retention_days` | - | 存儲保留日數 | `7` |

## 命令

### 使用者命令

- `/start` - 啟機關
- `/help` - 示助訊
- `/sub <id,...> [+tag1 -tag2]` - 訂畫師
- `/subrank <mode>` - 訂榜（daily、weekly、monthly）
- `/unsub <id,...>` - 退訂畫師
- `/unsubrank <mode>` - 退訂榜
- `/list` - 列活訂閱
- `/settings` - 示當前聊天設置
- `/blursensitive <on|off>` - 啟/止敏感內容之模糊處理
- `/sensitivetags <tag,...>` - 設自定敏感標籤
- `/clearsensitivetags` - 清敏感標籤
- `/excludetags <tag,...>` - 設排除標籤（有此標籤之圖將不傳）
- `/clearexcludedtags` - 清排除標籤

### 管理員命令

- `/enablechat [chat_id]` - 於聊天中啟機關（若處私模式）
- `/disablechat [chat_id]` - 於聊天中止機關

### 擁有者命令

- `/setadmin <user_id>` - 升使用者為管理員
- `/unsetadmin <user_id>` - 降管理員為使用者
- `/info` - 示機關系統狀態

## 貢獻

開發指南，見 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 許可

[MIT](LICENSE)

## 致謝

- **[PixivPy](https://github.com/upbit/pixivpy)**：Pixiv API 實作之要參考。無其工，此專案不可成也。
- **AI 助手**：特謝 GitHub Copilot、Claude 及 Gemini 於開發中所供技術支援及代碼生成。
