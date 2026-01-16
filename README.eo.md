# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**Lingvo / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

Rust-bazita Pixiv Telegram roboto.

## Trajtoj

- **Abono de Artisto**: Abonu Pixiv-artistojn kaj ricevi aŭtomatajn ĝisdatigojn por novaj ilustraĵoj.
- **Abono de Rangaro**: Abonu ĉiutagajn, ĉiusemajnajn, aŭ ĉiumonatajn Pixiv-rangarojn.
- **Detekto de Pixiv-Ligilo**: Aŭtomate detektas Pixiv-ilustraĵajn kaj uzantajn ligilojn en mesaĝoj.
  - Sendas kompletajn bildojn por ilustraĵaj ligoj.
  - Proponas rapidan abonon por uzantaj ligoj.
- **Inteligenta Bildotraktado**:
  - Aŭtomate grupas plurajn bildojn en albumojn.
  - Kaŝmemoras bildojn por redukti servilan ŝarĝon kaj Pixiv API-vokojn.
  - Subtenas malklarigilon por delikata enhavo (R-18, NSFW).
- **Fleksebla Planado**: Hazardigitaj enketaj intervaloj por konduti pli kiel homa uzanto kaj eviti rapidlimojn.
- **Alirkontrolado**:
  - Administrantaj/Posedantaj roloj por administri la roboton en grupaj babiloj.
  - Agordebla "Privata" (nur-invita) aŭ "Publika" reĝimoj.
- **Personecebla**:
  - Per-babila agordoj por delikataj etikedoj.
  - Agordeblaj kaŝmemorado kaj protokolado.

## Instalado kaj Uzado

Ni rekomendas uzi Docker por disponigo ĉar ĝi aŭtomate traktas dependecojn kaj mediajn agordojn.

### Uzante Docker Compose (Rekomendita)

1. Klonu la deponejon:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. Agordu la roboton:

   ```bash
   cp config.toml.example config.toml
   # Redaktu config.toml kun viaj ĵetonoj
   ```

3. Starigu per Docker Compose:

   ```bash
   docker compose up -d
   ```

   (Dosiero `docker-compose.yml` estas inkluzivita en la radika dosierujo)

### Konstrui el Fontkodo

Se vi preferas ruli rekte sur via maŝino:

1. **Antaŭkondiĉoj**:
    - Rust (plej nova stabila)
    - SQLite

2. **Klonu kaj Agordu**:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # Redaktu config.toml kun viaj legitimaĵoj
   ```

3. **Konstruu kaj Rulu**:

   ```bash
   cargo run --release
   ```

## Akiri Necesajn Ĵetonojn

Antaŭ ol agordi la roboton, vi bezonas akiri du esencajn ĵetonojn:

### 1. Telegram Bot Token

1. Malfermu Telegram kaj serĉu [@BotFather](https://t.me/BotFather)
2. Sendu `/newbot` kaj sekvu la instrukciojn
3. Elektu nomon kaj uzantnomon por via roboto
4. Vi ricevos ĵetonon kiel `123456789:ABCdefGHIjklMNOpqrsTUVwxyz`
5. Kopiu ĉi tiun ĵetonon al `config.toml` sub `telegram.bot_token`

### 2. Pixiv Refresh Token

**Rekomendita Metodo**: Uzu [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

Post kiam vi havas la refresh token, kopiu ĝin al `config.toml` sub `pixiv.refresh_token`.

⚠️ **Grave**: Gardu viajn ĵetonojn sekuraj kaj neniam komisiu ilin al versiokontrolado!

## Agordado

Subtenataj agordaj opcioj en `config.toml` aŭ per Mediaj Variabloj (prefikso `PIX__` kun duoblaj substrekoj):

| Agorda Ŝlosilo | Media Variablo | Priskribo | Defaŭlto |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | Posedanta Uzanto-ID | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` aŭ `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | Datumbaza Konekta URL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | Protokola Nivelo (info, debug, warn) | `"info"` |
| `scheduler.cache_retention_days` | - | Kaŝmemora reteno (tagoj) | `7` |

## Komandoj

### Uzantaj Komandoj

- `/start` - Starigi la roboton
- `/help` - Montri helpmesaĝon
- `/sub <id,...> [+tag1 -tag2]` - Aboni artiston
- `/subrank <mode>` - Aboni rangaron (daily, weekly, monthly)
- `/unsub <id,...>` - Malaboni artiston
- `/unsubrank <mode>` - Malaboni rangaron
- `/list` - Listigi aktivajn abonojn
- `/settings` - Montri kaj administri babilajn agordojn

### Administrantaj Komandoj

- `/enablechat [chat_id]` - Ebligi roboton en babilo (se en privata reĝimo)
- `/disablechat [chat_id]` - Malebligi roboton en babilo

### Posedantaj Komandoj

- `/setadmin <user_id>` - Promocii uzanton al Administranto
- `/unsetadmin <user_id>` - Malpromocii Administranton al Uzanto
- `/info` - Montri robotan sisteman statuson

## Kontribuado

Vidu [CONTRIBUTING.md](CONTRIBUTING.md) por programadaj gvidlinioj.

## Permesilo

[MIT](LICENSE)

## Dankoj

- **[PixivPy](https://github.com/upbit/pixivpy)**: Decida referenco por Pixiv API-realigo. Ĉi tiu projekto ne estus ebla sen ilia laboro.
- **AI Asisto**: Specialaj dankoj al GitHub Copilot, Claude, kaj Gemini pro ilia teknika subteno kaj kodgenero dum programado.
