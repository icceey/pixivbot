# PixivBot

[![CI](https://github.com/icceey/pixivbot/actions/workflows/ci.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/ci.yml)
[![Docker Build](https://github.com/icceey/pixivbot/actions/workflows/docker.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/docker.yml)
[![Release](https://github.com/icceey/pixivbot/actions/workflows/release.yml/badge.svg)](https://github.com/icceey/pixivbot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org/)
[![Docker Image](https://img.shields.io/badge/docker-ghcr.io-blue)](https://github.com/icceey/pixivbot/pkgs/container/pixivbot)

**Γλώσσα / Language:** [English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [文言文](README.lzh.md) | [Esperanto](README.eo.md) | [Ελληνικά](README.el.md) | [संस्कृतम्](README.sa.md) | [火星语](README.mars.md) | [Kiswahili](README.sw.md)

Ρομπότ Telegram για το Pixiv βασισμένο σε Rust.

## Χαρακτηριστικά

- **Συνδρομή Καλλιτέχνη**: Εγγραφείτε σε καλλιτέχνες του Pixiv και λαμβάνετε αυτόματες ενημερώσεις για νέες εικονογραφήσεις.
- **Συνδρομή Κατάταξης**: Εγγραφείτε σε ημερήσιες, εβδομαδιαίες ή μηνιαίες κατατάξεις του Pixiv.
- **Ανίχνευση Συνδέσμων Pixiv**: Ανιχνεύει αυτόματα συνδέσμους εικονογραφήσεων και χρηστών του Pixiv σε μηνύματα.
  - Στέλνει πλήρεις εικόνες για συνδέσμους εικονογραφήσεων.
  - Προσφέρει γρήγορη συνδρομή για συνδέσμους χρηστών.
- **Έξυπνος Χειρισμός Εικόνων**:
  - Ομαδοποιεί αυτόματα πολλαπλές εικόνες σε άλμπουμ.
  - Αποθηκεύει εικόνες στη μνήμη cache για να μειώσει το φορτίο του διακομιστή και τις κλήσεις API του Pixiv.
  - Υποστηρίζει θάμπωμα spoiler για ευαίσθητο περιεχόμενο (R-18, NSFW).
- **Ευέλικτος Προγραμματισμός**: Τυχαιοποιημένα διαστήματα ερωτημάτων για να συμπεριφέρεται πιο σαν ανθρώπινος χρήστης και να αποφεύγει τα όρια ρυθμού.
- **Έλεγχος Πρόσβασης**:
  - Ρόλοι Διαχειριστή/Ιδιοκτήτη για τη διαχείριση του ρομπότ σε ομαδικές συνομιλίες.
  - Διαμορφώσιμοι τρόποι λειτουργίας "Ιδιωτικός" (μόνο με πρόσκληση) ή "Δημόσιος".
- **Προσαρμόσιμο**:
  - Ρυθμίσεις ανά συνομιλία για ευαίσθητες ετικέτες.
  - Διαμορφώσιμη αποθήκευση cache και καταγραφή.

## Εγκατάσταση και Χρήση

Συνιστούμε τη χρήση Docker για την ανάπτυξη καθώς χειρίζεται αυτόματα τις εξαρτήσεις και τη ρύθμιση του περιβάλλοντος.

### Χρησιμοποιώντας Docker Compose (Συνιστάται)

1. Κλωνοποιήστε το αποθετήριο:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   ```

2. Διαμορφώστε το ρομπότ:

   ```bash
   cp config.toml.example config.toml
   # Επεξεργαστείτε το config.toml με τα tokens σας
   ```

3. Ξεκινήστε με Docker Compose:

   ```bash
   docker compose up -d
   ```

   (Ένα αρχείο `docker-compose.yml` περιλαμβάνεται στον ριζικό κατάλογο)

### Κατασκευή από τον Πηγαίο Κώδικα

Αν προτιμάτε να τρέξετε απευθείας στο μηχάνημά σας:

1. **Προαπαιτούμενα**:
    - Rust (τελευταία σταθερή έκδοση)
    - SQLite

2. **Κλωνοποίηση και Διαμόρφωση**:

   ```bash
   git clone https://github.com/icceey/pixivbot.git
   cd pixivbot
   cp config.toml.example config.toml
   # Επεξεργαστείτε το config.toml με τα διαπιστευτήριά σας
   ```

3. **Κατασκευή και Εκτέλεση**:

   ```bash
   cargo run --release
   ```

## Λήψη Απαιτούμενων Tokens

Πριν διαμορφώσετε το ρομπότ, πρέπει να αποκτήσετε δύο βασικά tokens:

### 1. Telegram Bot Token

1. Ανοίξτε το Telegram και αναζητήστε το [@BotFather](https://t.me/BotFather)
2. Στείλτε `/newbot` και ακολουθήστε τις οδηγίες
3. Επιλέξτε ένα όνομα και όνομα χρήστη για το ρομπότ σας
4. Θα λάβετε ένα token όπως `123456789:ABCdefGHIjklMNOpqrsTUVwxyz`
5. Αντιγράψτε αυτό το token στο `config.toml` κάτω από το `telegram.bot_token`

### 2. Pixiv Refresh Token

**Συνιστώμενη Μέθοδος**: Χρησιμοποιήστε το [get-pixivpy-token](https://github.com/eggplants/get-pixivpy-token)

Μόλις έχετε το refresh token, αντιγράψτε το στο `config.toml` κάτω από το `pixiv.refresh_token`.

⚠️ **Σημαντικό**: Διατηρήστε τα tokens σας ασφαλή και μην τα δημοσιεύσετε ποτέ στο version control!

## Διαμόρφωση

Υποστηριζόμενες επιλογές διαμόρφωσης στο `config.toml` ή μέσω Μεταβλητών Περιβάλλοντος (πρόθεμα `PIX__` με διπλές κάτω παύλες):

| Κλειδί Διαμόρφωσης | Μεταβλητή Περιβάλλοντος | Περιγραφή | Προεπιλογή |
|---|---|---|---|
| `telegram.bot_token` | `PIX__TELEGRAM__BOT_TOKEN` | Telegram Bot API Token | `""` |
| `telegram.owner_id` | `PIX__TELEGRAM__OWNER_ID` | ID Χρήστη Ιδιοκτήτη | `0` |
| `telegram.bot_mode` | `PIX__TELEGRAM__BOT_MODE` | `public` ή `private` | `"private"` |
| `pixiv.refresh_token` | `PIX__PIXIV__REFRESH_TOKEN` | Pixiv OAuth Refresh Token | `""` |
| `database.url` | `PIX__DATABASE__URL` | URL Σύνδεσης Βάσης Δεδομένων | `sqlite:./data/pixivbot.db?mode=rwc` |
| `logging.level` | `PIX__LOGGING__LEVEL` | Επίπεδο Καταγραφής (info, debug, warn) | `"info"` |
| `scheduler.cache_retention_days` | - | Διατήρηση cache (ημέρες) | `7` |

## Εντολές

### Εντολές Χρήστη

- `/start` - Εκκίνηση του ρομπότ
- `/help` - Εμφάνιση μηνύματος βοήθειας
- `/sub <id,...> [+tag1 -tag2]` - Εγγραφή σε καλλιτέχνη
- `/subrank <mode>` - Εγγραφή σε κατάταξη (daily, weekly, monthly)
- `/unsub <id,...>` - Διαγραφή εγγραφής από καλλιτέχνη
- `/unsubrank <mode>` - Διαγραφή εγγραφής από κατάταξη
- `/list` - Λίστα ενεργών εγγραφών
- `/settings` - Εμφάνιση τρεχουσών ρυθμίσεων συνομιλίας
- `/blursensitive <on|off>` - Ενεργοποίηση/απενεργοποίηση spoiler για ευαίσθητο περιεχόμενο
- `/sensitivetags <tag,...>` - Ορισμός προσαρμοσμένων ευαίσθητων ετικετών
- `/clearsensitivetags` - Εκκαθάριση ευαίσθητων ετικετών
- `/excludetags <tag,...>` - Ορισμός εξαιρούμενων ετικετών (εικόνες με αυτές τις ετικέτες δεν θα σταλούν)
- `/clearexcludedtags` - Εκκαθάριση εξαιρούμενων ετικετών

### Εντολές Διαχειριστή

- `/enablechat [chat_id]` - Ενεργοποίηση ρομπότ σε συνομιλία (αν είναι σε ιδιωτικό τρόπο λειτουργίας)
- `/disablechat [chat_id]` - Απενεργοποίηση ρομπότ σε συνομιλία

### Εντολές Ιδιοκτήτη

- `/setadmin <user_id>` - Προαγωγή χρήστη σε Διαχειριστή
- `/unsetadmin <user_id>` - Υποβιβασμός Διαχειριστή σε Χρήστη
- `/info` - Εμφάνιση κατάστασης συστήματος ρομπότ

## Συνεισφορά

Δείτε το [CONTRIBUTING.md](CONTRIBUTING.md) για οδηγίες ανάπτυξης.

## Άδεια

[MIT](LICENSE)

## Ευχαριστίες

- **[PixivPy](https://github.com/upbit/pixivpy)**: Κρίσιμη αναφορά για την υλοποίηση του Pixiv API. Αυτό το έργο δεν θα ήταν δυνατό χωρίς τη δουλειά τους.
- **Βοήθεια AI**: Ειδικές ευχαριστίες στο GitHub Copilot, Claude και Gemini για την τεχνική τους υποστήριξη και τη δημιουργία κώδικα κατά τη διάρκεια της ανάπτυξης.
