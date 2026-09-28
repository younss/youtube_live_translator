//! Traduction neuronale locale (NMT) : NLLB-200 distillé 600M, quantifié int8, exécuté par
//! CTranslate2 compilé dans l'app. Hors ligne, ~630 Mo sur disque, ~1 Go de RAM, quelques
//! dizaines de ms par ligne : assez rapide pour suivre Whisper en quasi temps réel.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use ct2rs::{ComputeType, Config, TranslationOptions, Translator};
use sentencepiece::SentencePieceProcessor;
use tokio::io::AsyncWriteExt;

use crate::translate::Progress;

const MODEL_DIR: &str = "nllb-200-600m-int8";
const REPO: &str = "https://huggingface.co/JustFrederik/nllb-200-distilled-600M-ct2-int8/resolve/main";
const FILES: [&str; 4] = ["config.json", "model.bin", "sentencepiece.bpe.model", "shared_vocabulary.txt"];

/// Codes de langue NLLB (FLORES-200).
fn nllb_code(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "ar" => "arb_Arab",
        "fr" => "fra_Latn",
        "en" => "eng_Latn",
        "de" => "deu_Latn",
        "tr" => "tur_Latn",
        "es" => "spa_Latn",
        _ => return None,
    })
}

pub fn model_dir(models_dir: &Path) -> PathBuf {
    models_dir.join(MODEL_DIR)
}

pub fn is_ready(models_dir: &Path) -> bool {
    FILES.iter().all(|f| model_dir(models_dir).join(f).is_file())
}

/// Télécharge le modèle une seule fois dans le cache partagé (hors de l'app :
/// réinstaller l'app ne le retélécharge pas).
pub async fn ensure_model(models_dir: &Path, progress: &Progress) -> Result<PathBuf> {
    let dir = model_dir(models_dir);
    tokio::fs::create_dir_all(&dir).await?;
    for file in FILES {
        let path = dir.join(file);
        if path.is_file() {
            continue;
        }
        let mut resp = reqwest::get(format!("{REPO}/{file}")).await?.error_for_status().context("téléchargement du modèle NMT")?;
        let total = resp.content_length().unwrap_or(0);
        let tmp = path.with_extension("part");
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut got = 0u64;
        while let Some(chunk) = resp.chunk().await? {
            out.write_all(&chunk).await?;
            got += chunk.len() as u64;
            if total > 1 << 20 {
                progress(got as f32 / total as f32, format!("Modèle de traduction : {} / {} Mo", got >> 20, total >> 20));
            }
        }
        out.flush().await?;
        tokio::fs::rename(&tmp, &path).await?;
    }
    Ok(dir)
}

/// Tokeniseur NLLB : `<langue source> pièces… </s>` en entrée ; à la sortie on retire
/// les jetons spéciaux et de langue avant de recoller les pièces SentencePiece.
struct NllbTokenizer {
    sp: SentencePieceProcessor,
    source: Arc<Mutex<&'static str>>,
}

impl ct2rs::Tokenizer for NllbTokenizer {
    fn encode(&self, input: &str) -> Result<Vec<String>> {
        let src = *self.source.lock().map_err(|_| anyhow!("tokeniseur verrouillé"))?;
        let mut tokens = vec![src.to_string()];
        tokens.extend(self.sp.encode(input)?.into_iter().map(|p| p.piece));
        tokens.push("</s>".into());
        Ok(tokens)
    }

    fn decode(&self, tokens: Vec<String>) -> Result<String> {
        let pieces: Vec<String> = tokens
            .into_iter()
            .filter(|t| !(t.starts_with('<') && t.ends_with('>')) && !is_lang_token(t))
            .collect();
        Ok(self.sp.decode_pieces(&pieces)?)
    }
}

fn is_lang_token(t: &str) -> bool {
    // Forme « xxx_Yyyy » (ex. fra_Latn).
    t.len() == 8 && t.as_bytes()[3] == b'_' && t[..3].chars().all(|c| c.is_ascii_lowercase())
}

/// Le modèle est chargé une fois et reste en mémoire entre deux vidéos.
type Engine = (Translator<NllbTokenizer>, Arc<Mutex<&'static str>>);
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

/// Traduit les lignes (bloquant : à appeler depuis `spawn_blocking`).
/// `on_chunk(n)` est appelé avec toutes les traductions déjà prêtes après chaque lot,
/// pour afficher les sous-titres sans attendre la fin.
pub fn translate_blocking(
    dir: &Path,
    lines: &[String],
    source: &str,
    target: &str,
    progress: &Progress,
    on_chunk: &dyn Fn(&[String]),
) -> Result<Vec<String>> {
    let src = nllb_code(source).ok_or_else(|| anyhow!("langue source inconnue : choisissez-la dans « DE »"))?;
    let tgt = nllb_code(target).ok_or_else(|| anyhow!("langue cible non supportée : {target}"))?;

    let mut guard = ENGINE.lock().map_err(|_| anyhow!("moteur de traduction indisponible"))?;
    if guard.is_none() {
        progress(0.0, "Chargement du modèle de traduction…".into());
        let sp = SentencePieceProcessor::open(dir.join("sentencepiece.bpe.model"))?;
        let source = Arc::new(Mutex::new(src));
        let tokenizer = NllbTokenizer { sp, source: source.clone() };
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
        let config = Config { num_threads_per_replica: threads, compute_type: ComputeType::INT8, ..Config::default() };
        *guard = Some((Translator::with_tokenizer(dir, tokenizer, &config)?, source));
    }
    let (engine, source) = guard.as_ref().unwrap();
    *source.lock().unwrap_or_else(|e| e.into_inner()) = src;

    let options = TranslationOptions {
        beam_size: 2,
        max_decoding_length: 200,
        repetition_penalty: 1.1,
        ..Default::default()
    };
    const BATCH: usize = 16;
    let mut out = Vec::with_capacity(lines.len());
    let chunks: Vec<&[String]> = lines.chunks(BATCH).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let prefixes = vec![vec![tgt.to_string()]; chunk.len()];
        let res = engine.translate_batch_with_target_prefix(chunk, &prefixes, &options, None)?;
        if res.len() != chunk.len() {
            bail!("le moteur a renvoyé {} lignes au lieu de {}", res.len(), chunk.len());
        }
        out.extend(res.into_iter().map(|(text, _)| text.trim().to_string()));
        on_chunk(&out);
        progress((i + 1) as f32 / chunks.len() as f32, format!("Traduction locale {}/{}", i + 1, chunks.len()));
    }
    Ok(out)
}
