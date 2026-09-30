#!/bin/sh
# Installe l'app dans /Applications (ou ~/Applications si /Applications n'est pas accessible)
# et vérifie les outils nécessaires.
set -e
cd "$(dirname "$0")/.."
./scripts/bundle.sh
APP="target/YouTube Live Translator.app"
DEST="/Applications"
[ -w "$DEST" ] || { DEST="$HOME/Applications"; mkdir -p "$DEST"; }
rm -rf "$DEST/YouTube Live Translator.app"
cp -R "$APP" "$DEST/"
echo "Installée dans $DEST/YouTube Live Translator.app"

# Modèles (Whisper + traduction NMT) téléchargés maintenant, une seule fois, dans le cache
# partagé : l'app est prête dès le premier lancement et une réinstallation ne les retélécharge pas.
echo "Installation des modèles…"
"$DEST/YouTube Live Translator.app/Contents/MacOS/ytlt" --setup

missing=""
for tool in yt-dlp mpv; do
  command -v $tool >/dev/null 2>&1 || [ -x "/opt/homebrew/bin/$tool" ] || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "Outils manquants :$missing"
  echo "Installez-les avec : brew install yt-dlp mpv"
fi
