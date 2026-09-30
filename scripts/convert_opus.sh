#!/bin/sh
# Convertit OPUS-MT tc-big turc → anglais (Helsinki-NLP) au format CTranslate2 int8 (~230 Mo).
# Aucune version convertie n'existe à télécharger : on la fabrique une fois, dans un
# environnement Python temporaire supprimé ensuite.
# Usage : scripts/convert_opus.sh [dossier_de_sortie]
set -e
OUT="${1:-$HOME/Library/Caches/youtube-live-translator/models/opus-mt-tc-big-tr-en-int8}"
[ -f "$OUT/model.bin" ] && { echo "OPUS-MT turc déjà installé."; exit 0; }
command -v python3 >/dev/null || { echo "python3 introuvable : modèle turc OPUS ignoré (NLLB sera utilisé pour le turc)."; exit 0; }
VENV="$(mktemp -d)/venv"
echo "Conversion du modèle turc OPUS-MT (une seule fois, ~2 min)…"
python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --upgrade pip
"$VENV/bin/pip" install --quiet ctranslate2 transformers sentencepiece torch
"$VENV/bin/ct2-transformers-converter" --model Helsinki-NLP/opus-mt-tc-big-tr-en --output_dir "$OUT" \
  --quantization int8 --copy_files source.spm target.spm --force >/dev/null 2>&1
rm -rf "$(dirname "$VENV")"
echo "Modèle turc OPUS-MT prêt : $OUT"
