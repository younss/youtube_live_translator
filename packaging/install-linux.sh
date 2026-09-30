#!/bin/sh
# Installe l'application pour l'utilisateur courant (~/.local), sans droits administrateur.
set -e
cd "$(dirname "$0")"
dest="$HOME/.local/share/youtube-live-translator"
mkdir -p "$dest" "$HOME/.local/bin" "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/256x256/apps"
cp youtube-live-translator yt-dlp "$dest/"
[ -d models ] && cp -r models "$dest/"
ln -sf "$dest/youtube-live-translator" "$HOME/.local/bin/youtube-live-translator"
cp youtube-live-translator.png "$HOME/.local/share/icons/hicolor/256x256/apps/"
sed "s|^Exec=.*|Exec=$dest/youtube-live-translator|" youtube-live-translator.desktop > "$HOME/.local/share/applications/youtube-live-translator.desktop"
command -v ldconfig >/dev/null && ldconfig -p 2>/dev/null | grep -q libmpv || echo "Attention : libmpv introuvable — installez-la (Ubuntu : sudo apt install libmpv2)."
echo "Installé. Lancez « YouTube Live Translator » depuis le menu des applications."
