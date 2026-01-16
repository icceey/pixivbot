# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**Lugha / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

Boti ya Telegram ya Pixiv iliyojengwa kwa Rust.

## Vipengele

- **Usajili wa Msanii**: Jiandikishe kwa wasanii wa Pixiv na upokee masasisho ya moja kwa moja ya michoro mipya.
- **Usajili wa Orodha**: Jiandikishe kwa orodha za kila siku, kila wiki, au kila mwezi za Pixiv.
- **Utambuzi wa Viungo vya Pixiv**: Hutambua kiotomatiki viungo vya michoro na watumiaji wa Pixiv kwenye ujumbe.
  - Hutuma picha kamili kwa viungo vya michoro.
  - Hutoa chaguo la usajili wa haraka kwa viungo vya watumiaji.
- **Ushughulikaji wa Picha wa Akili**:
  - Huunganisha picha nyingi kiotomatiki kwenye albamu.
  - Huhifadhi picha ili kupunguza mzigo wa seva na wito wa Pixiv API.
  - Inasaidia kubana kwa maudhui nyeti (R-18, NSFW).
- **Ratiba Kubadilika**: Muda wa uchunguzi uliochanganywa kwa nasibu ili kutenda kama mtumiaji wa binadamu na kuepuka vikomo vya kasi.
- **Udhibiti wa Ufikiaji**:
  - Majukumu ya Msimamizi/Mmiliki kwa kusimamia boti katika mazungumzo ya kikundi.
  - Modi za "Binafsi" (kwa mwaliko tu) au "Umma" zinazoweza kusanidiwa.
- **Inaweza Kubinafsishwa**:
  - Mipangilio ya kila mazungumzo kwa lebo nyeti.
  - Hifadhi na kurekodi kunaweza kusanidiwa.

## Usakinishaji na Matumizi

Tunapendekeza kutumia Docker kwa usambazaji kwa sababu inashughulikia tegemezi na usanidi wa mazingira kiotomatiki.

### Kutumia Docker Compose (Inashauriwa)

1. Nakili hifadhi:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. Sanidi boti:

   ```bash
   cp config.toml.example config.toml
   # Hariri config.toml na tokeni zako
   ```

3. Anzisha na Docker Compose:

   ```bash
   docker compose up -d
   ```

   (Faili ya `docker-compose.yml` imejumuishwa katika saraka la mizizi)

### Jenga kutoka kwa Msimbo wa Chanzo

Ikiwa unapendelea kuendesha moja kwa moja kwenye mashine yako:

1. **Mahitaji ya Awali**:
    - Rust (sasisho la hivi karibuni)
    - SQLite

2. **Nakili na Sanidi**:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # Hariri config.toml na vitambulisho vyako
   ```

3. **Jenga na Endesha**:

   ```bash
   cargo run --release
   ```

## Kupata Tokeni Zinazohitajika

Kabla ya kusanidi boti, unahitaji kupata tokeni mbili muhimu:

### 1. Telegram Bot Token

1. Fungua Telegram na tafuta [@BotFather](https://t.me/BotFather)
2. Tuma `/newbot` na fuata maagizo
3. Chagua jina na jina la mtumiaji kwa boti yako
4. Utapokea tokeni kama `123456789:ABCdefGHIjklMNOpqrsTUVwxyz`
5. Nakili tokeni hii kwenye `config.toml` chini ya `telegram.bot_token`

### 2. Pixiv Refresh Token

**Njia Inayopendekezwa**: Tumia [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

Baada ya kuwa na tokeni ya refresh, inakili kwenye `config.toml` chini ya `pixiv.refresh_token`.

⚠️ **Muhimu**: Weka tokeni zako salama na usiziwasilishe kamwe kwenye udhibiti wa toleo!

## Usanidi

Chaguo za usanidi zinazosaidiwa katika `config.toml` au kupitia Vigeuzi vya Mazingira (kiambishi awali `PIX__` na mistari miwili ya chini):

| Ufunguo wa Usanidi | Kibadilishaji cha Mazingira | Maelezo | Chaguo-msingi |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | Kitambulisho cha Mtumiaji Mmiliki | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` au `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | URL ya Muunganisho wa Hifadhidata | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | Kiwango cha Kurekodi (info, debug, warn) | `"info"` |
| `scheduler.cache_retention_days` | - | Uhifadhi wa hifadhi (siku) | `7` |

## Amri

### Amri za Mtumiaji

- `/start` - Anzisha boti
- `/help` - Onyesha ujumbe wa msaada
- `/sub <id,...> [+tag1 -tag2]` - Jiandikishe kwa msanii
- `/subrank <mode>` - Jiandikishe kwa orodha (daily, weekly, monthly)
- `/unsub <id,...>` - Ondoa usajili kutoka kwa msanii
- `/unsubrank <mode>` - Ondoa usajili kutoka kwa orodha
- `/list` - Orodhesha usajili unaofanya kazi
- `/settings` - Onyesha na dhibiti mipangilio ya mazungumzo

### Amri za Msimamizi

- `/enablechat [chat_id]` - Washa boti katika mazungumzo (ikiwa ni katika modi ya kibinafsi)
- `/disablechat [chat_id]` - Zima boti katika mazungumzo

### Amri za Mmiliki

- `/setadmin <user_id>` - Pandisha mtumiaji kuwa Msimamizi
- `/unsetadmin <user_id>` - Shusha Msimamizi kuwa Mtumiaji
- `/info` - Onyesha hali ya mfumo wa boti

## Kuchangia

Tazama [CONTRIBUTING.md](CONTRIBUTING.md) kwa miongozo ya maendeleo.

## Leseni

[MIT](LICENSE)

## Shukrani

- **[PixivPy](https://github.com/upbit/pixivpy)**: Marejeleo muhimu kwa utekelezaji wa Pixiv API. Mradi huu haungewezekana bila kazi yao.
- **Msaada wa AI**: Shukrani maalum kwa GitHub Copilot, Claude, na Gemini kwa msaada wao wa kiufundi na uzalishaji wa msimbo wakati wa maendeleo.
