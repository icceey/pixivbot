# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**भाषा / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

Rust-आधारितः Pixiv Telegram यन्त्रमानवः।

## विशेषताः

- **कलाकारस्य सदस्यता**: Pixiv कलाकारान् सदस्यं कुर्वन्तु, नवचित्राणां स्वचालितं ज्ञापनं प्राप्नुवन्तु।
- **श्रेणीसदस्यता**: Pixiv दैनिकश्रेणीं, साप्ताहिकश्रेणीं, मासिकश्रेणीं वा सदस्यं कुर्वन्तु।
- **Pixiv सङ्केतपरीक्षणम्**: सन्देशेषु Pixiv चित्राणां उपयोक्तॄणां च सङ्केतान् स्वचालितं परीक्षते।
  - चित्रसङ्केतेभ्यः पूर्णचित्राणि प्रेषयति।
  - उपयोक्तृसङ्केतेभ्यः द्रुतसदस्यताम् प्रदाति।
- **बुद्धिमन्चित्रप्रक्रिया**:
  - बहूनि चित्राणि स्वचालितं चित्रसङ्ग्रहे संयोजयति।
  - सेवकभारं Pixiv API आह्वानं च न्यूनीकर्तुं चित्राणि संगृहीतानि भवन्ति।
  - संवेदनशीलविषयस्य (R-18, NSFW) अस्पष्टीकरणं समर्थयति।
- **लचीलं समयनिर्धारणम्**: यादृच्छिकपरीक्षाकालाः मानवोपयोक्तृव्यवहारस्य अनुकरणाय दरसीमानां परिहाराय च।
- **प्रवेशनियन्त्रणम्**:
  - समूहवार्तासु यन्त्रमानवस्य प्रबन्धनाय प्रशासक/स्वामिभूमिकाः।
  - "निजी" (आमन्त्रणमात्रम्) "सार्वजनिकः" वा रीतिः विन्यास्यः।
- **अनुकूलनीयम्**:
  - प्रत्येकवार्तायै संवेदनशीलचिह्नानां सेटिङ्गाः।
  - विन्यास्यसंग्रहणं प्रवेशनञ्च।

## स्थापनं प्रयोगश्च

वयं Docker उपयोगस्य अनुशंसां कुर्मः यतः तत् निर्भरतां पर्यावरणविन्यासं च स्वचालितं साधयति।

### Docker Compose उपयोगः (अनुशंस्यते)

1. भाण्डागारं प्रतिलिप्यताम्:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. यन्त्रमानवं विन्यस्यताम्:

   ```bash
   cp config.toml.example config.toml
   # config.toml भवतः टोकनैः सह सम्पाद्यताम्
   ```

3. Docker Compose येन प्रारभ्यताम्:

   ```bash
   docker compose up -d
   ```

   (मूलनिर्देशिकायां `docker-compose.yml` सञ्चिका अन्तर्भूता अस्ति)

### स्रोतात् निर्माणम्

यदि भवान् स्वयन्त्रे सीधे चालयितुम् इच्छति:

1. **पूर्वापेक्षाः**:
    - Rust (नवीनतमः स्थिरः)
    - SQLite

2. **प्रतिलिप्यताम् विन्यस्यताम् च**:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # config.toml भवतः प्रमाणपत्रैः सह सम्पाद्यताम्
   ```

3. **निर्मीयताम् चाल्यताम् च**:

   ```bash
   cargo run --release
   ```

## आवश्यकटोकनानि प्राप्यन्ताम्

यन्त्रमानवं विन्यस्य पूर्वं द्वे आवश्यके टोकने प्राप्तव्ये:

### 1. Telegram Bot Token

1. Telegram उद्घाट्य [@BotFather](https://t.me/BotFather) अन्विष्यताम्
2. `/newbot` प्रेष्यताम् निर्देशान् अनुसर्यताम् च
3. भवतः यन्त्रमानवस्य नाम उपयोक्तृनाम च चिनुताम्
4. `123456789:ABCdefGHIjklMNOpqrsTUVwxyz` इत्यादिरूपं टोकनं प्राप्स्यति
5. एतत् टोकनं `config.toml` मध्ये `telegram.bot_token` इत्यस्मिन् प्रतिलिप्यताम्

### 2. Pixiv Refresh Token

**अनुशंसितः उपायः**: [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token) उपयुज्यताम्

refresh token प्राप्य, तत् `config.toml` मध्ये `pixiv.refresh_token` इत्यस्मिन् प्रतिलिप्यताम्।

⚠️ **महत्त्वपूर्णम्**: भवतः टोकनानि सुरक्षितानि रक्ष्यन्ताम्, संस्करणनियन्त्रणे कदापि न समर्प्यन्ताम्!

## विन्यासः

`config.toml` मध्ये वा पर्यावरणचलकैः (उपसर्गः `PIX__` द्विगुणाधःरेखाभिः सह) समर्थिताः विन्यासविकल्पाः:

| विन्यासकुञ्जी | पर्यावरणचलकः | विवरणम् | पूर्वनिर्धारितम् |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | स्वामिउपयोक्तृपरिचयः | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` अथवा `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | दत्तांशसञ्चयसम्बन्धURL | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | प्रवेशस्तरः (info, debug, warn) | `"info"` |
| `scheduler.cache_retention_days` | - | संग्रहसंरक्षणदिनानि | `7` |

## आदेशाः

### उपयोक्तृआदेशाः

- `/start` - यन्त्रमानवं प्रारभ्यताम्
- `/help` - साहाय्यसन्देशं दर्श्यताम्
- `/sub <id,...> [+tag1 -tag2]` - कलाकारं सदस्यं क्रियताम्
- `/subrank <mode>` - श्रेणीं सदस्यं क्रियताम् (daily, weekly, monthly)
- `/unsub <id,...>` - कलाकारात् सदस्यता त्यज्यताम्
- `/unsubrank <mode>` - श्रेण्याः सदस्यता त्यज्यताम्
- `/list` - सक्रियसदस्यतानां सूची दर्श्यताम्
- `/settings` - वर्तमानवार्तासेटिङ्गः दर्श्यताम् तथा प्रबन्ध्यताम्

### प्रशासकआदेशाः

- `/enablechat [chat_id]` - वार्तायां यन्त्रमानवं सक्रियं क्रियताम् (यदि निजीरीत्यां)
- `/disablechat [chat_id]` - वार्तायां यन्त्रमानवं निष्क्रियं क्रियताम्

### स्वामिआदेशाः

- `/setadmin <user_id>` - उपयोक्तारं प्रशासके उन्नीयताम्
- `/unsetadmin <user_id>` - प्रशासकं उपयोक्तृत्वे अवनीयताम्
- `/info` - यन्त्रमानवस्य प्रणालीस्थितिः दर्श्यताम्

## योगदानम्

विकासमार्गदर्शनार्थं [CONTRIBUTING.md](CONTRIBUTING.md) पश्यताम्।

## अनुज्ञापत्रम्

[MIT](LICENSE)

## कृतज्ञता

- **[PixivPy](https://github.com/upbit/pixivpy)**: Pixiv API कार्यान्वयनस्य महत्त्वपूर्णः सन्दर्भः। तेषां कार्यं विना इदं प्रकल्पं सम्भवं नासीत्।
- **AI साहाय्यम्**: विकासे तकनीकीसहाय्यस्य कोडजननस्य च कृते GitHub Copilot, Claude, Gemini एभ्यः विशेषधन्यवादाः।
