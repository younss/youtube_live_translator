#!/bin/sh
# Crée target/YouTube-Live-Translator.dmg : glisser l'app sur le raccourci Applications.
set -e
cd "$(dirname "$0")/.."
./scripts/bundle.sh
STAGE="target/dmg"
rm -rf "$STAGE" && mkdir -p "$STAGE"
cp -R "target/YouTube Live Translator.app" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f target/YouTube-Live-Translator.dmg
hdiutil create -volname "YouTube Live Translator" -srcfolder "$STAGE" -ov -format UDZO target/YouTube-Live-Translator.dmg >/dev/null
echo "OK : target/YouTube-Live-Translator.dmg"
