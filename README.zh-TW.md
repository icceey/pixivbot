# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**語言 / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

基於 Rust 的 Pixiv Telegram 機器人。

## 功能特性

- **作者訂閱**：訂閱 Pixiv 畫師，自動獲取新作品更新通知。
- **排行榜訂閱**：訂閱 Pixiv 日榜、週榜或月榜。
- **Pixiv 連結檢測**：自動檢測訊息中的 Pixiv 作品和使用者連結。
  - 作品連結：傳送完整圖片。
  - 使用者連結：提供快速訂閱選項。
- **智慧圖片處理**：
  - 自動將多張圖片組合成相簿。
  - 快取圖片以減少伺服器負載和 Pixiv API 呼叫。
  - 支援對敏感內容（R-18、NSFW）進行模糊處理。
- **彈性的排程**：隨機化輪詢間隔，模擬真人使用者行為，避免觸發速率限制。
- **存取控制**：
  - 管理員/擁有者角色，用於管理群組聊天中的機器人。
  - 可設定「私有」（僅邀請）或「公開」模式。
- **可自訂**：
  - 每個聊天的敏感標籤設定。
  - 可設定的快取和日誌記錄。

## 安裝與使用

我們推薦使用 Docker 進行部署，因為它會自動處理相依性和環境設定。

### 使用 Docker Compose（推薦）

1. 複製儲存庫：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. 設定機器人：

   ```bash
   cp config.toml.example config.toml
   # 編輯 config.toml 填入你的權杖
   ```

3. 使用 Docker Compose 啟動：

   ```bash
   docker compose up -d
   ```

   （根目錄中包含 `docker-compose.yml` 檔案）

### 從原始碼建置

如果你更喜歡直接在機器上執行：

1. **前置要求**：
    - Rust（最新穩定版）
    - SQLite

2. **複製並設定**：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # 編輯 config.toml 填入你的憑證
   ```

3. **建置並執行**：

   ```bash
   cargo run --release
   ```

## 取得所需權杖

在設定機器人之前，你需要取得兩個必需的權杖：

### 1. Telegram Bot Token

1. 在 Telegram 中搜尋 [@BotFather](https://t.me/BotFather)
2. 傳送 `/newbot` 並按照指示操作
3. 為你的機器人選擇名稱和使用者名稱
4. 你將收到類似 `123456789:ABCdefGHIjklMNOpqrsTUVwxyz` 的權杖
5. 將此權杖複製到 `config.toml` 的 `telegram.bot_token` 欄位

### 2. Pixiv Refresh Token

**推薦方法**：使用 [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

取得 refresh token 後，將其複製到 `config.toml` 的 `pixiv.refresh_token` 欄位。

⚠️ **重要提示**：請妥善保管你的權杖，切勿將其提交到版本控制系統！

## 設定

`config.toml` 或環境變數（前綴 `PIX__`，使用雙底線）支援的設定選項：

| 設定鍵 | 環境變數 | 說明 | 預設值 |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | 擁有者使用者 ID | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` 或 `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | 資料庫連線 URL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | 日誌級別（info、debug、warn） | `"info"` |
| `scheduler.cache_retention_days` | - | 快取保留天數 | `7` |

## 命令

### 使用者命令

- `/start` - 啟動機器人
- `/help` - 顯示說明資訊
- `/sub <id,...> [+tag1 -tag2]` - 訂閱畫師
- `/subrank <mode>` - 訂閱排行榜（daily、weekly、monthly）
- `/unsub <id,...>` - 取消訂閱畫師
- `/unsubrank <mode>` - 取消訂閱排行榜
- `/list` - 列出活躍的訂閱
- `/settings` - 顯示和管理聊天設定（互動式介面，帶有內嵌按鈕）
  - 切換敏感內容模糊
  - 編輯敏感標籤
  - 編輯排除標籤
- `/cancel` - 取消目前設定操作
- `/download <url|id>` - 下載原圖（或回覆訊息）

### 管理員命令

- `/enablechat [chat_id]` - 在聊天中啟用機器人（如果處於私有模式）
- `/disablechat [chat_id]` - 在聊天中停用機器人

### 擁有者命令

- `/setadmin <user_id>` - 將使用者提升為管理員
- `/unsetadmin <user_id>` - 將管理員降級為使用者
- `/info` - 顯示機器人系統狀態

## 貢獻

有關開發指南，請參閱 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 授權條款

[MIT](LICENSE)

## 致謝

- **[PixivPy](https://github.com/upbit/pixivpy)**：Pixiv API 實作的重要參考。沒有他們的工作，這個專案就不可能實現。
- **AI 助手**：特別感謝 GitHub Copilot、Claude 和 Gemini 在開發過程中提供的技術支援和程式碼生成。
