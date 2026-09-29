# YouTube Live Translator

A small Rust browser (tao + wry) with a **VLC × Winamp** style interface. It plays a YouTube video from its URL and shows **generated and translated subtitles**, from and to **Arabic, French, English, German, Turkish and Spanish**.

## Build from source (macOS)

Prerequisites:
- macOS 12 or later. Apple Silicon is recommended: Whisper runs on the GPU through Metal.
- Xcode command line tools: `xcode-select --install`
- [Rust](https://rustup.rs) (stable, edition 2024)
- Homebrew: `brew install cmake yt-dlp`
  - `cmake` is used only at build time, to compile whisper.cpp and CTranslate2;
  - `yt-dlp` is the only external tool the app needs at runtime.

```bash
git clone https://github.com/younss/youtube_live_translator.git
cd youtube_live_translator
./scripts/install.sh
```

`install.sh` does three things:
- builds the app (the first build takes a few minutes, because of the C++ code for whisper.cpp and CTranslate2);
- copies it into `/Applications` (or `~/Applications`);
- downloads the models once, about 1.9 GB total (Whisper large-v3-turbo about 550 MB, NLLB 1.3B about 1.3 GB), into `~/Library/Caches/youtube-live-translator/models`.

Reinstalling doesn't download the models again.

Other scripts:
- `./scripts/bundle.sh` builds `target/YouTube Live Translator.app` without installing it.
- `./scripts/dmg.sh` builds a `.dmg`. Installed from the DMG, the app downloads the models on first launch.
- `cargo run --release -- --setup` downloads the models only.
- `cargo run --release -- --server` starts only the local server, to debug from a browser. It prints the URL to open, which includes the session token.

Keep `yt-dlp` up to date (`brew upgrade yt-dlp`): YouTube changes often, and an outdated yt-dlp is the most common cause of errors.

## How it works

| Step | Tool |
|---|---|
| Playback | **Native** player. yt-dlp resolves the stream: in the app, YouTube's HLS stream, which WebKit plays natively with audio and video together; in a regular browser, separate H.264 + AAC streams relayed by the local server. The YouTube iframe is kept as a fallback (the "YOUTUBE" menu). |
| Source subtitles | 1. YouTube subtitles (manual first, otherwise automatic) 2. otherwise **Whisper compiled into the app** (whisper.cpp + Metal). The audio is read as a stream and decoded in Rust (HLS/AAC segments via symphonia, resampled to 16 kHz, no ffmpeg), then transcribed in 30 s chunks as it arrives, with nothing downloaded to disk. |
| Translation | **Local NMT** by default: NLLB-200 int8, 1.3B (better quality) or 600M (lighter) to choose in ⚙ Settings, running in CTranslate2, compiled into the app. It's offline and needs no key. Also available: **Google** (free, with MyMemory as a fallback when Google blocks), **Claude** (API key), or **YouTube's automatic translation**. |
| Sync and context | Subtitles are kept short (at most ~2 lines, 5 s), with Whisper's word-level timestamps, so each line appears when it's spoken. For translation, lines are regrouped into whole sentences (split on punctuation and pauses), translated with that context, then the translation is spread back over the lines of the sentence. |
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

## Third-party licenses

- The code in this repository: [MIT](LICENSE).
- whisper.cpp and Whisper models: MIT.
- CTranslate2: MIT.
- NLLB-200 (translation models): **CC-BY-NC 4.0, non-commercial use only**.
- yt-dlp: Unlicense. symphonia (audio decoding): MPL-2.0.

## License and disclaimer

The code is distributed under the [MIT](LICENSE) license: free to use, modify and redistribute.

**The software is provided "as is", without warranty of any kind. The author cannot be held liable for any damage, claim or consequence resulting from its use.**

- This project is a personal, educational project. It is **not affiliated with, endorsed or sponsored by YouTube, Google, Meta, OpenAI, Anthropic** or any other company mentioned.
- **Each user is solely responsible for their use**, in particular:
  - compliance with the [YouTube Terms of Service](https://www.youtube.com/t/terms);
  - respect for the copyright of the content watched, transcribed or translated;
  - the laws of their country.
- The generated subtitles and translations are automatic and may be wrong. They must not be relied on for any important decision (legal, medical, etc.).
- The models used keep their own licenses (see above). NLLB-200 is restricted to non-commercial use.
