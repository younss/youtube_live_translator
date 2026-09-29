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

/// Modèles proposés : (identifiant, dossier local, dépôt Hugging Face).
pub const MODELS: [(&str, &str, &str); 2] = [
    ("600m", "nllb-200-600m-int8", "JustFrederik/nllb-200-distilled-600M-ct2-int8"),
    ("1.3b", "nllb-200-1.3b-int8", "JustFrederik/nllb-200-distilled-1.3B-ct2-int8"),
];
pub const DEFAULT_MODEL: &str = "1.3b";

fn entry(name: &str) -> (&'static str, &'static str, &'static str) {
    MODELS.iter().copied().find(|(n, _, _)| *n == name).unwrap_or(MODELS[1])
}

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
        "pt" => "por_Latn",
        "it" => "ita_Latn",
        "ru" => "rus_Cyrl",
        "pl" => "pol_Latn",
        "nl" => "nld_Latn",
        "fa" => "pes_Arab",
        "ur" => "urd_Arab",
        "hi" => "hin_Deva",
        "bn" => "ben_Beng",
        "ta" => "tam_Taml",
        "te" => "tel_Telu",
        "zh" => "zho_Hans",
        "ja" => "jpn_Jpan",
        "ko" => "kor_Hang",
        "th" => "tha_Thai",
        "vi" => "vie_Latn",
        "id" => "ind_Latn",
        "tl" => "tgl_Latn",
        _ => return None,
    })
}

pub fn model_dir(models_dir: &Path, name: &str) -> PathBuf {
    models_dir.join(entry(name).1)
}

pub fn is_ready(models_dir: &Path, name: &str) -> bool {
    FILES.iter().all(|f| model_dir(models_dir, name).join(f).is_file())
}

/// Télécharge le modèle une seule fois dans le cache partagé (hors de l'app :
/// réinstaller l'app ne le retélécharge pas).
pub async fn ensure_model(models_dir: &Path, name: &str, progress: &Progress) -> Result<PathBuf> {
    let dir = model_dir(models_dir, name);
    let repo = entry(name).2;
    tokio::fs::create_dir_all(&dir).await?;
    for file in FILES {
        let path = dir.join(file);
        if path.is_file() {
            continue;
        }
        let mut resp = reqwest::get(format!("https://huggingface.co/{repo}/resolve/main/{file}")).await?.error_for_status().context("téléchargement du modèle NMT")?;
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

/// NLLB termine souvent les phrases japonaises et chinoises par un point latin : on remet la
/// ponctuation pleine chasse attendue.
fn fix_punctuation(text: &str, target: &str) -> String {
    if !matches!(target, "ja" | "zh") {
        return text.to_string();
    }
    let t = text.trim_end();
    match t.strip_suffix('.').or_else(|| t.strip_suffix("...")) {
        Some(rest) if !rest.ends_with('.') => format!("{rest}。"),
        _ => t.replace("?", "？").replace("!", "！"),
    }
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
type Engine = (PathBuf, Translator<NllbTokenizer>, Arc<Mutex<&'static str>>);
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
static LAST_USE: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Libère le modèle (~0,7–1,4 Go) après 1 min sans traduction ; il se recharge en ~1 s.
fn start_idle_reaper() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(15));
            let idle = LAST_USE.lock().ok().and_then(|t| *t).is_some_and(|t| t.elapsed().as_secs() > 60);
            if idle {
                // try_lock : on ne libère jamais un modèle en train de traduire.
                if let Ok(mut guard) = ENGINE.try_lock() {
                    if guard.take().is_some() {
                        *LAST_USE.lock().unwrap() = None;
                        drop(guard);
                        crate::release_memory();
                    }
                }
            }
        });
    });
}

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

    start_idle_reaper();
    let mut guard = ENGINE.lock().map_err(|_| anyhow!("moteur de traduction indisponible"))?;
    if guard.as_ref().is_none_or(|(d, _, _)| d != dir) {
        *guard = None; // libère l'ancien modèle avant d'en charger un autre
        progress(0.0, "Chargement du modèle de traduction…".into());
        let sp = SentencePieceProcessor::open(dir.join("sentencepiece.bpe.model"))?;
        let source = Arc::new(Mutex::new(src));
        let tokenizer = NllbTokenizer { sp, source: source.clone() };
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
        let config = Config { num_threads_per_replica: threads, compute_type: ComputeType::INT8, ..Config::default() };
        *guard = Some((dir.to_path_buf(), Translator::with_tokenizer(dir, tokenizer, &config)?, source));
    }
    let (_, engine, source) = guard.as_ref().unwrap();
    *source.lock().unwrap_or_else(|e| e.into_inner()) = src;

    // Recherche en faisceau plus large = meilleures traductions pour un coût modeste sur des
    // lignes courtes. Pas de pénalité de répétition : dans une chanson, elle est voulue.
    let options = TranslationOptions {
        beam_size: 4,
        max_decoding_length: 200,
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
        out.extend(res.into_iter().map(|(text, _)| fix_punctuation(text.trim(), target)));
        on_chunk(&out);
        progress((i + 1) as f32 / chunks.len() as f32, format!("Traduction locale {}/{}", i + 1, chunks.len()));
    }
    *LAST_USE.lock().unwrap() = Some(std::time::Instant::now());
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn cjk_final_period() {
        assert_eq!(super::fix_punctuation("明天我会在车站等你.", "zh"), "明天我会在车站等你。");
        assert_eq!(super::fix_punctuation("Hello.", "fr"), "Hello.");
    }
}
