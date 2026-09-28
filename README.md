# YouTube Live Translator

A small Rust browser (tao + wry) with a **VLC × Winamp** style interface. It plays a YouTube video from its URL and shows **generated and translated subtitles**, from and to **Arabic, French, English, German, Turkish and Spanish**.

## Install

```bash
brew install yt-dlp ffmpeg whisper-cpp
./scripts/install.sh        # builds the app and copies it into /Applications
```

- `./scripts/dmg.sh` builds `target/YouTube-Live-Translator.dmg`: open it and drag the app onto Applications.
- `./scripts/bundle.sh` builds `target/YouTube Live Translator.app` only.
- The icon is drawn by `scripts/make_icon.swift` (→ `assets/icon.png`).

`cargo run -- --server` starts only the local server (http://127.0.0.1:47653) so you can test in a regular browser.

## How it works

| Step | Tool |
|---|---|
| Playback | **Native** player: yt-dlp resolves the H.264 + AAC streams, which play in synced `<video>`/`<audio>` elements. The YouTube iframe is kept as a fallback (the "YOUTUBE" menu). |
| Source subtitles | 1. YouTube subtitles (manual first, otherwise automatic) 2. otherwise a **local Whisper** transcription (whisper.cpp; the model downloads automatically on first use) |
| Translation | **Google** (free, no key), **Claude** (Anthropic API key in ⚙ Settings or `ANTHROPIC_API_KEY`), or **YouTube's automatic translation** |
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
