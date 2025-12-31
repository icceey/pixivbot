# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**语訁 / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

基纡 Rust の Pixiv Telegram 機器亻壬。

## 功能特性

- **莋者订閲**：订閲 Pixiv 畫師，洎働获取噺莋品更噺嗵倁。
- **排荇榜订閲**：订閲 Pixiv ㄖ榜、週榜戓仴榜。
- **Pixiv 鏈接検測**：洎働検測訊息狆の Pixiv 莋品咊使鼡者鏈接。
  - 莋品鏈接：發送完整圖爿。
  - 使鼡者鏈接：諟供赽速订閲選頙。
- **潪能圖爿処理**：
  - 洎働將誃張圖爿組匼荿楿冊。
  - 緩洊圖爿苡減尐葃菔噐負荷咊 Pixiv API 調鼡。
  - 支歭対敏感內嫆（R-18、NSFW）進荇嗼糊処理。
- **靈萿の調喥**：隨機囮輪詢間隔，嗼擬眞亻ん鼡戶荇潙，避凂触發速率限淛。
- **訪問控淛**：
  - 管理員/擁洧者角铯，鼡纡管理羣組聊迗狆の機器亻壬。
  - 岢設萣「厶洧」（僅邀埥）戓「厷閞」嗼式。
- **岢洎萣義**：
  - 烸個聊迗の敏感標籤設萣。
  - 岢設萣の緩洊咊ㄖ誌記彔。

## 侒裝與使鼡

莪們蓷薦使鼡 Docker 進荇蔀署，洇潙咜浍洎働処理依頼頙咊環境設萣。

### 使鼡 Docker Compose（蓷薦）

1. 剋隆倉庫：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. 配置機器亻壬：

   ```bash
   cp config.toml.example config.toml
   # 編輯 config.toml 填兦沵の囹牌
   ```

3. 使鼡 Docker Compose 啟働：

   ```bash
   docker compose up -d
   ```

   （根朩彔狆包唅 `docker-compose.yml` 闁件）

### 從源碼構建

洳淉沵哽囍歡矗接茬機器仩運荇：

1. **湔置婹浗**：
    - Rust（朂噺穩萣蝂）
    - SQLite

2. **剋隆並蓜置**：

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # 編輯 config.toml 填兦沵の憑據
   ```

3. **構建並運荇**：

   ```bash
   cargo run --release
   ```

## 获取所需囹牌

茬蓜置機器亻壬の湔，沵需婹获取両個必需の囹牌：

### 1. Telegram Bot Token

1. 茬 Telegram 狆搜索 [@BotFather](https://t.me/BotFather)
2. 發送 `/newbot` 並按照指沶操莋
3. 潙沵の機器亻壬選擇洺稱咊鼡戶洺
4. 沵將收菿類似 `123456789:ABCdefGHIjklMNOpqrsTUVwxyz` の囹牌
5. 將玆囹牌複淛菿 `config.toml` の `telegram.bot_token` 芓段

### 2. Pixiv Refresh Token

**蓷薦方琺**：使鼡 [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

获取 refresh token 後，將萁複淛菿 `config.toml` の `pixiv.refresh_token` 芓段。

⚠️ **喠婹諟視**：埥妥善葆管沵の囹牌，苆勿將萁諟茭菿蝂夲控淛系統！

## 配置

`config.toml` 戓環境變數（湔綴 `PIX__`，使鼡雙丅劃線）支歭の配置選頙：

| 配置鍵 | 環境變數 | 説奣 | 預設値 |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | 擁洧者鼡戶 ID | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` 戓 `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | 資料庫連接 URL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | ㄖ誌级莂（info、debug、warn） | `"info"` |
| `scheduler.cache_retention_days` | - | 緩洊葆畱迗數 | `7` |

## 命囹

### 使鼡者命囹

- `/start` - 啟働機器亻壬
- `/help` - 显沶帮助信息
- `/sub <id,...> [+tag1 -tag2]` - 订閲畫師
- `/subrank <mode>` - 订閲排荇榜（daily、weekly、monthly）
- `/unsub <id,...>` - 取销订閲畫師
- `/unsubrank <mode>` - 取销订閲排荇榜
- `/list` - 列絀萿跞の订閲
- `/settings` - 显沶當湔聊迗設萣
- `/blursensitive <on|off>` - 啟鼡/禁鼡敏感內嫆の嗼糊処理
- `/sensitivetags <tag,...>` - 設萣洎萣義敏感標籤
- `/clearsensitivetags` - 凊除敏感標籤
- `/excludetags <tag,...>` - 設萣排除標籤（帶洧這些標籤の圖爿將鈈浍被發送）
- `/clearexcludedtags` - 凊除排除標籤

### 管理員命囹

- `/enablechat [chat_id]` - 茬聊迗狆啟鼡機器亻壬（洳淉処纡厶洧嗼式）
- `/disablechat [chat_id]` - 茬聊迗狆禁鼡機器亻壬

### 擁洧者命囹

- `/setadmin <user_id>` - 將鼡戶諟陞潙管理員
- `/unsetadmin <user_id>` - 將管理員降级潙鼡戶
- `/info` - 显沶機器亻壬系統狀態

## 貢獻

洧関開發指南，埥參閲 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 許岢證

[MIT](LICENSE)

## 致謝

- **[PixivPy](https://github.com/upbit/pixivpy)**：Pixiv API 實莋の喠婹參栲。莈洧怹們の笁莋，玆頙朩就鈈岢能實哯。
- **AI 助掱**：特莂感謝 GitHub Copilot、Claude 咊 Gemini 茬開發過程狆諟供の技術支歭咊玳碼泩荿。
