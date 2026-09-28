# YouTube Live Translator

A small Rust browser (tao + wry) with a **VLC × Winamp** style interface. It plays a YouTube video from its URL and shows **generated and translated subtitles**, from and to **Arabic, French, English, German, Turkish and Spanish**.

## Install

```bash
brew install yt-dlp ffmpeg
./scripts/install.sh        # builds the app, copies it into /Applications and downloads the models
```

- `./scripts/dmg.sh` builds `target/YouTube-Live-Translator.dmg`: open it and drag the app onto Applications.
- `./scripts/bundle.sh` builds `target/YouTube Live Translator.app` only.
- Models (Whisper + NMT) are downloaded once into `~/Library/Caches/youtube-live-translator/models`: a reinstall doesn't download them again. From the DMG, the app downloads them on first launch (`ytlt --setup` does the same thing from the command line).
- The icon is drawn by `scripts/make_icon.swift` (→ `assets/icon.png`).

`cargo run -- --server` starts only the local server (http://127.0.0.1:47653) so you can test in a regular browser.

## How it works

| Step | Tool |
|---|---|
| Playback | **Native** player: yt-dlp resolves the H.264 + AAC streams, which play in synced `<video>`/`<audio>` elements. The YouTube iframe is kept as a fallback (the "YOUTUBE" menu). |
| Source subtitles | 1. YouTube subtitles (manual first, otherwise automatic) 2. otherwise **Whisper compiled into the app** (whisper.cpp + Metal). The audio is read as a stream by ffmpeg and transcribed in 30 s chunks as it arrives, with nothing downloaded to disk. |
| Translation | **Local NMT** by default: NLLB-200 int8, 1.3B (better quality) or 600M (lighter) to choose in ⚙ Settings, running in CTranslate2, compiled into the app. It's offline, needs no key, and uses about 630 MB on disk. Also available: **Google** (free, with MyMemory as a fallback when Google blocks), **Claude** (API key), or **YouTube's automatic translation**. |
| Progressive display | With the local NMT, each Whisper segment is translated and shown as soon as it's transcribed. Changing the target language doesn't re-run Whisper: the transcript is cached separately. |
| Gender and proper names | With Claude: the **VOIX** (who is speaking) and **À QUI** (who is addressed or talked about) choices set the grammatical agreement (Arabic أنتَ/أنتِ, French agreement…). Proper names are kept or transliterated. Google translates line by line and can't take this into account. |

Results are cached in `~/Library/Caches/youtube-live-translator/`. Settings are stored in `~/Library/Application Support/youtube-live-translator/config.json`.

## Keyboard shortcuts

| Key | Action |
|---|---|
| Space | Play / pause |
| ← / → | Seek back / forward 5 s |
| ↑ / ↓ | Volume up / down |
| F | Full screen (Esc to exit) |
| S | Show / hide subtitles |
| D | Two lines: original + translation |
| [ / ] | Shift subtitle sync by ±0.1 s |
| N / P | Next / previous playlist item |
| G | Generate subtitles |
| L | Focus the address bar |

Typing anything that isn't a URL in the address bar runs a YouTube search (RECHERCHE tab).

## Limitations

- Live streams: YouTube subtitles work if the stream provides them. Whisper needs the full video.
- For songs, Whisper works best with the `large-v3-turbo` model (the default). Setting the source language (e.g. Turkish) instead of "Auto" further improves recognition.
- The spectrum analyzer is decorative: the page cannot read the YouTube audio signal.

## Security

- The local server listens on `127.0.0.1` only, so it can't be reached from the network.
- Each launch generates a random 256-bit **session token**, given only to the app window. The window swaps it for an `HttpOnly` / `SameSite=Strict` cookie.
- Every request without that cookie is rejected (403): other local programs, and websites open in a browser (CSRF).
- The `Host` / `Origin` headers are checked, which blocks DNS rebinding.
- The Claude API key is stored in the **macOS Keychain**. `config.json` (mode 600) no longer holds any secret.
- The window only loads the app and the YouTube player. Any other navigation is refused, and pop-up windows open in the default browser.
- A strict Content-Security-Policy is in place: only the app's scripts, the YouTube player/streams, hls.js and the fonts are allowed.
