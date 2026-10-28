# Offside BOT

[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Serenity](https://img.shields.io/badge/serenity-0.12-blue.svg?style=flat-square)](https://github.com/serenity-rs/serenity)
[![Songbird](https://img.shields.io/badge/songbird-0.6-indigo.svg?style=flat-square)](https://github.com/serenity-rs/songbird)
[![yt-dlp](https://img.shields.io/badge/yt--dlp-powered-red.svg?style=flat-square)](https://github.com/yt-dlp/yt-dlp)
[![Docker](https://img.shields.io/badge/docker-ready-2496ED.svg?style=flat-square&logo=docker)](https://www.docker.com/)

A high-performance, asynchronous Discord music bot written in **Rust** powered by **Serenity**, **Songbird**, and **yt-dlp**. Designed for pristine audio quality, zero-wait caching, intelligent search fine-tuning, and zero-key multi-platform link resolution.

---

## Features

- **Smart Audio-Only YouTube Search:**
  Automatically prioritizes clean studio releases, official audio, lyric videos, and YouTube Music (`- Topic`) tracks while filtering out music videos with dialogue, skits, bloopers, movie scenes, and behind-the-scenes footage.
- **Zero-Wait Disk Caching:**
  Simultaneously streams audio directly to voice while teeing the stream into a local cache directory. Looped songs or replayed tracks start with **0ms buffering**. Automatically prunes files with an LRU strategy (5 GB limit, 7-day TTL).
- **Zero-Key Multi-Platform Resolvers:**
  Paste links directly from **Spotify**, **Apple Music**, **Deezer**, or **YouTube / YouTube Music**.
  - **Spotify:** Seamlessly resolves track info via public OpenGraph metadata without requiring a Spotify Developer account or API keys (falls back to official API if credentials are provided).
  - **Apple Music / iTunes:** Free public directory lookup via the official iTunes Search API.
  - **Deezer:** Free track resolution via Deezer's public REST API.
- **Flexible Voice & Stay Modes:**
  Customizable inactivity timeout (`!!stay 24`, `!!stay forever`, `!!stay off`). The bot stays connected for the specified duration and auto-leaves when idle to save bandwidth.
- **Seamless Dev Mode (`!!dev`):**
  Instantly pause the production container while testing changes on a local development instance with `!!dev on`, and resume production with `!!dev off`.
- **Modern Discord Markdown:**
  Clean, structured command menu and feedback formatted specifically for Discord's desktop, mobile, and web clients.

---

## Commands

All commands use the `!!` prefix:

### Playback
| Command | Description | Example |
|---|---|---|
| `!!play <song \| URL>` | Play a song or queue it (supports query, YouTube, Spotify, Apple Music, Deezer) | `!!play Bohemian Rhapsody`<br>`!!play https://open.spotify.com/track/...` |
| `!!pause` | Pause current playback | `!!pause` |
| `!!resume` | Resume paused playback | `!!resume` |
| `!!skip` | Skip to the next song in the queue | `!!skip` |
| `!!stop` | Stop playback and clear the queue | `!!stop` |
| `!!loop` | Toggle looping for the current track | `!!loop` |

### Queue & Voice Channel
| Command | Description | Example |
|---|---|---|
| `!!queue` | View the currently playing track and upcoming queue | `!!queue` |
| `!!clear` | Clear all waiting songs in the queue | `!!clear` |
| `!!stay <hours \| forever \| off>` | Keep bot connected in voice (default: 2 minutes inactivity timeout) | `!!stay 24`<br>`!!stay forever`<br>`!!stay off` |
| `!!disconnect` | Immediately disconnect the bot from voice | `!!disconnect` |

### System & Utilities
| Command | Description | Example |
|---|---|---|
| `!!cache` | View local audio cache usage (track count and size) | `!!cache` |
| `!!dev [on \| off]` | Toggle dev mode (pauses production bot during local testing) | `!!dev on`<br>`!!dev off` |
| `!!help` | Display the interactive Markdown command menu | `!!help` |

---

## Getting Started

### Prerequisites

1. **Rust:** Ensure you have Rust and Cargo installed (1.80+):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
2. **Audio Tools & Codecs:**
   - **`ffmpeg`** installed and available in system `PATH`.
   - **`yt-dlp`** installed and available in system `PATH` (or placed in the project directory as `yt-dlp` / `yt-dlp.exe`).

#### Ubuntu / Debian:
```bash
sudo apt update
sudo apt install -y build-essential cmake pkg-config libopus-dev ffmpeg
curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o /usr/local/bin/yt-dlp
sudo chmod a+rx /usr/local/bin/yt-dlp
```

#### Windows:
- Install `ffmpeg` and `yt-dlp` via [winget](https://learn.microsoft.com/en-us/windows/package-manager/winget/) or [Scoop](https://scoop.sh/):
  ```powershell
  winget install Gyan.FFmpeg
  winget install yt-dlp.yt-dlp
  ```

---

## Configuration

Create a `.env` file in the project root:

```env
# Required: Discord Bot Token from Discord Developer Portal
DISCORD_TOKEN=your_discord_bot_token_here

# Optional: Storage directory for cached audio streams (default: cache)
CACHE_DIR=cache

# Optional: Environment flag (production / dev)
BOT_ENV=development

# Optional: Spotify Developer API credentials
# NOTE: If left empty or omitted, OffsideBOT will automatically resolve Spotify
# links using its zero-key public OpenGraph resolver!
SPOTIPY_CLIENT_ID=
SPOTIPY_CLIENT_SECRET=
```

### Discord Bot Permissions
When creating your bot in the [Discord Developer Portal](https://discord.com/developers/applications):
1. Navigate to **Bot** &rarr; **Privileged Gateway Intents**.
2. Enable **Message Content Intent**.
3. Under OAuth2 URL Generator, assign:
   - Scope: `bot`
   - Permissions: `Connect`, `Speak`, `Send Messages`, `Read Message History`, `View Channels`.

---

## Running the Bot

### Local Development
```bash
# Run in debug mode (starts with local dev mode enabled)
cargo run

# Or build optimized release binary
cargo run --release
```

### Running Tests
The test suite verifies audio search fine-tuning, cache limits, direct URL resolution, and multi-platform resolvers:
```bash
cargo test -- --nocapture
```

---

## Docker Deployment

The project includes a multi-stage Docker build producing a minimal runtime image (~100 MB) with `yt-dlp` and `ffmpeg` pre-configured.

### Using Docker Compose (Recommended)

1. Make sure your `.env` file is populated.
2. Build and start the container in the background:
   ```bash
   docker compose up -d --build
   ```
3. View logs:
   ```bash
   docker compose logs -f
   ```
4. Stop the bot:
   ```bash
   docker compose down
   ```

---


## YouTube Cookies (Optional)

If YouTube imposes rate limits or bot verifications on your server IP:
1. Export cookies from your browser using an extension like **Get cookies.txt LOCALLY** ([Chrome Web Store](https://chromewebstore.google.com/detail/get-cookiestxt-locally/cclelndahbckbenkjhflpdbgdldlbecc) / [Firefox Add-ons](https://addons.mozilla.org/en-US/firefox/addon/get-cookies-txt-locally/)).
2. Save the file as `cookies.txt` in the bot's root directory (or mount it into `/app/cookies.txt` in Docker).
3. The bot will automatically detect and load `cookies.txt` for all `yt-dlp` operations.

---

## License

This project is licensed under the terms described in [LICENSE.txt](LICENSE.txt).

Developed with ❤️ by [@evangelion.xyz](https://github.com/evangelionxyz)