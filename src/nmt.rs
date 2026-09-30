//! Traduction neuronale locale (NMT), exécutée par CTranslate2 compilé dans l'app. Hors ligne.
//!
//! - **NLLB-200** (Meta, 200 langues, traduction directe entre toutes) : 600M, 1.3B ou 3.3B,
//!   quantifiés int8.
//! - **OPUS-MT tc-big turc → anglais** (Helsinki, Marian, ~230 Mo) : comprend nettement mieux le
//!   turc (suffixes, subordonnées) que NLLB. Quand la source est le turc, on traduit tr → en avec
//!   OPUS, puis en → langue cible avec NLLB (inutile si la cible est l'anglais).
//!
//! Chaque ligne est découpée en phrases avant traduction : ces modèles ne traduisent souvent
//! que la première (ou la dernière) phrase d'une ligne qui en contient plusieurs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use ct2rs::{ComputeType, Config, TranslationOptions, Translator};
use sentencepiece::SentencePieceProcessor;
use tokio::io::AsyncWriteExt;

use crate::translate::Progress;

/// Modèles NLLB proposés : (identifiant, dossier local, dépôt Hugging Face).
pub const MODELS: [(&str, &str, &str); 3] = [
    ("600m", "nllb-200-600m-int8", "JustFrederik/nllb-200-distilled-600M-ct2-int8"),
    ("1.3b", "nllb-200-1.3b-int8", "JustFrederik/nllb-200-distilled-1.3B-ct2-int8"),
    ("3.3b", "nllb-200-3.3b-int8", "OpenNMT/nllb-200-3.3B-ct2-int8"),
];
pub const DEFAULT_MODEL: &str = "1.3b";

/// Tous les NLLB-200 partagent le même SentencePiece ; le dépôt 3.3B ne le fournit pas.
const NLLB_SPM_URL: &str = "https://huggingface.co/JustFrederik/nllb-200-distilled-1.3B-ct2-int8/resolve/main/sentencepiece.bpe.model";

/// Dossier du modèle OPUS-MT tc-big tr → en converti pour CTranslate2 (par l'installateur).
pub const OPUS_TR_EN: &str = "opus-mt-tc-big-tr-en-int8";

fn entry(name: &str) -> (&'static str, &'static str, &'static str) {
    MODELS.iter().copied().find(|(n, _, _)| *n == name).unwrap_or(MODELS[1])
}

/// Fichiers à télécharger pour un modèle NLLB : (nom local, URL).
fn nllb_files(name: &str) -> Vec<(&'static str, String)> {
    let repo = entry(name).2;
    let url = |f: &str| format!("https://huggingface.co/{repo}/resolve/main/{f}");
    if name == "3.3b" {
        vec![
            ("config.json", url("config.json")),
            ("shared_vocabulary.json", url("shared_vocabulary.json")),
            ("sentencepiece.bpe.model", NLLB_SPM_URL.to_string()),
            ("model.bin", url("model.bin")),
        ]
    } else {
        ["config.json", "shared_vocabulary.txt", "sentencepiece.bpe.model", "model.bin"].map(|f| (f, url(f))).to_vec()
    }
}

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
    let dir = model_dir(models_dir, name);
    ["config.json", "model.bin", "sentencepiece.bpe.model"].iter().all(|f| dir.join(f).is_file())
        && (dir.join("shared_vocabulary.txt").is_file() || dir.join("shared_vocabulary.json").is_file())
}

/// Modèle OPUS tr → en : dans le cache des modèles, ou livré à côté de l'exécutable
/// (versions Windows / Linux construites par GitHub).
pub fn opus_dir(models_dir: &Path) -> Option<PathBuf> {
    let beside_exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("models").join(OPUS_TR_EN)));
    [Some(models_dir.join(OPUS_TR_EN)), beside_exe]
        .into_iter()
        .flatten()
        .find(|d| ["config.json", "model.bin", "source.spm", "target.spm"].iter().all(|f| d.join(f).is_file()))
}

/// Télécharge le modèle NLLB une seule fois dans le cache partagé (hors de l'app :
/// réinstaller l'app ne le retélécharge pas).
pub async fn ensure_model(models_dir: &Path, name: &str, progress: &Progress) -> Result<PathBuf> {
    let dir = model_dir(models_dir, name);
    tokio::fs::create_dir_all(&dir).await?;
    for (file, url) in nllb_files(name) {
        let path = dir.join(file);
        if path.is_file() {
            continue;
        }
        let mut resp = reqwest::get(&url).await?.error_for_status().context("téléchargement du modèle NMT")?;
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

/// Découpe une ligne en phrases, en gardant la ponctuation finale avec chaque phrase.
fn split_sentences(line: &str) -> Vec<String> {
    const ENDS: [char; 10] = ['.', '!', '?', '…', '؟', '۔', '。', '！', '？', '।'];
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = line.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let next = chars.get(i + 1).copied();
        // Fin de phrase : ponctuation suivie d'un espace (ou pleine chasse, qui n'en met pas),
        // hors suite de ponctuations (« ?! », « ... »).
        let full_width = matches!(c, '。' | '！' | '？');
        if ENDS.contains(&c) && next.is_some_and(|n| !ENDS.contains(&n) && (n.is_whitespace() || full_width)) {
            let s = cur.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
            cur.clear();
        }
    }
    let s = cur.trim().to_string();
    if !s.is_empty() {
        out.push(s);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
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
        let pieces: Vec<String> = tokens.into_iter().filter(|t| !is_special(t) && !is_lang_token(t)).collect();
        Ok(self.sp.decode_pieces(&pieces)?)
    }
}

/// Tokeniseur Marian (OPUS-MT) : SentencePiece source en entrée, cible en sortie.
struct MarianTokenizer {
    source: SentencePieceProcessor,
    target: SentencePieceProcessor,
}

impl ct2rs::Tokenizer for MarianTokenizer {
    fn encode(&self, input: &str) -> Result<Vec<String>> {
        let mut tokens: Vec<String> = self.source.encode(input)?.into_iter().map(|p| p.piece).collect();
        tokens.push("</s>".into());
        Ok(tokens)
    }

    fn decode(&self, tokens: Vec<String>) -> Result<String> {
        let pieces: Vec<String> = tokens.into_iter().filter(|t| !is_special(t)).collect();
        Ok(self.target.decode_pieces(&pieces)?)
    }
}

fn is_special(t: &str) -> bool {
    t.starts_with('<') && t.ends_with('>')
}

fn is_lang_token(t: &str) -> bool {
    // Forme « xxx_Yyyy » (ex. fra_Latn).
    t.len() == 8 && t.as_bytes()[3] == b'_' && t[..3].chars().all(|c| c.is_ascii_lowercase())
}

/// Les modèles sont chargés une fois et restent en mémoire entre deux vidéos.
type Nllb = (PathBuf, Translator<NllbTokenizer>, Arc<Mutex<&'static str>>);
static NLLB: Mutex<Option<Nllb>> = Mutex::new(None);
static OPUS: Mutex<Option<(PathBuf, Translator<MarianTokenizer>)>> = Mutex::new(None);
static LAST_USE: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Libère les modèles (0,2–3,5 Go) après 1 min sans traduction ; ils se rechargent en ~1–3 s.
fn start_idle_reaper() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(15));
            let idle = LAST_USE.lock().ok().and_then(|t| *t).is_some_and(|t| t.elapsed().as_secs() > 60);
            if !idle {
                continue;
            }
            // try_lock : on ne libère jamais un modèle en train de traduire.
            let (Ok(mut nllb), Ok(mut opus)) = (NLLB.try_lock(), OPUS.try_lock()) else { continue };
            if nllb.take().is_some() | opus.take().is_some() {
                *LAST_USE.lock().unwrap() = None;
                drop((nllb, opus));
                crate::release_memory();
            }
        });
    });
}

fn config() -> Config {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    Config { num_threads_per_replica: threads, compute_type: ComputeType::INT8, ..Config::default() }
}

/// Recherche en faisceau plus large = meilleures traductions pour un coût modeste sur des
/// lignes courtes. Pas de pénalité de répétition : dans une chanson, elle est voulue.
fn options() -> TranslationOptions<String, String> {
    TranslationOptions { beam_size: 4, max_decoding_length: 200, ..Default::default() }
}

const BATCH: usize = 16;

fn nllb_translate(
    dir: &Path,
    sentences: &[String],
    source: &str,
    target: &str,
    progress: &Progress,
    on_batch: &mut dyn FnMut(&[String]),
) -> Result<Vec<String>> {
    let src = nllb_code(source).ok_or_else(|| anyhow!("langue source inconnue : choisissez-la dans « DE »"))?;
    let tgt = nllb_code(target).ok_or_else(|| anyhow!("langue cible non supportée : {target}"))?;
    let mut guard = NLLB.lock().map_err(|_| anyhow!("moteur de traduction indisponible"))?;
    if guard.as_ref().is_none_or(|(d, _, _)| d != dir) {
        *guard = None; // libère l'ancien modèle avant d'en charger un autre
        progress(0.0, "Chargement du modèle de traduction…".into());
        let sp = SentencePieceProcessor::open(dir.join("sentencepiece.bpe.model"))?;
        let source = Arc::new(Mutex::new(src));
        let tokenizer = NllbTokenizer { sp, source: source.clone() };
        *guard = Some((dir.to_path_buf(), Translator::with_tokenizer(dir, tokenizer, &config())?, source));
    }
    let (_, engine, source) = guard.as_ref().unwrap();
    *source.lock().unwrap_or_else(|e| e.into_inner()) = src;
    let mut out = Vec::with_capacity(sentences.len());
    for chunk in sentences.chunks(BATCH) {
        let prefixes = vec![vec![tgt.to_string()]; chunk.len()];
        let res = engine.translate_batch_with_target_prefix(chunk, &prefixes, &options(), None)?;
        if res.len() != chunk.len() {
            bail!("le moteur a renvoyé {} lignes au lieu de {}", res.len(), chunk.len());
        }
        out.extend(res.into_iter().map(|(text, _)| fix_punctuation(text.trim(), target)));
        on_batch(&out);
    }
    Ok(out)
}

fn opus_translate(dir: &Path, sentences: &[String], progress: &Progress, on_batch: &mut dyn FnMut(&[String])) -> Result<Vec<String>> {
    let mut guard = OPUS.lock().map_err(|_| anyhow!("moteur OPUS indisponible"))?;
    if guard.as_ref().is_none_or(|(d, _)| d != dir) {
        progress(0.0, "Chargement du modèle turc (OPUS-MT)…".into());
        let tokenizer = MarianTokenizer {
            source: SentencePieceProcessor::open(dir.join("source.spm"))?,
            target: SentencePieceProcessor::open(dir.join("target.spm"))?,
        };
        *guard = Some((dir.to_path_buf(), Translator::with_tokenizer(dir, tokenizer, &config())?));
    }
    let (_, engine) = guard.as_ref().unwrap();
    let mut out = Vec::with_capacity(sentences.len());
    for chunk in sentences.chunks(BATCH) {
        let res = engine.translate_batch(chunk, &options(), None)?;
        if res.len() != chunk.len() {
            bail!("le moteur OPUS a renvoyé {} lignes au lieu de {}", res.len(), chunk.len());
        }
        out.extend(res.into_iter().map(|(text, _)| text.trim().to_string()));
        on_batch(&out);
    }
    Ok(out)
}

/// Traduit les lignes (bloquant : à appeler depuis `spawn_blocking`) avec le NLLB `nllb`
/// (et OPUS quand la source est le turc). `on_chunk` reçoit les lignes déjà traduites après
/// chaque lot, pour afficher les sous-titres sans attendre la fin.
pub fn translate_blocking(
    models_dir: &Path,
    nllb: &str,
    lines: &[String],
    source: &str,
    target: &str,
    progress: &Progress,
    on_chunk: &dyn Fn(&[String]),
) -> Result<Vec<String>> {
    start_idle_reaper();
    // Phrases à plat + nombre de phrases par ligne, pour recoller ensuite.
    let split: Vec<Vec<String>> = lines.iter().map(|l| split_sentences(l)).collect();
    let flat: Vec<String> = split.iter().flatten().cloned().collect();
    let joiner = if matches!(target, "zh" | "ja" | "th") { "" } else { " " };
    let rejoin = |done: &[String]| -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        for parts in &split {
            if i + parts.len() > done.len() {
                break;
            }
            out.push(done[i..i + parts.len()].join(joiner).trim().to_string());
            i += parts.len();
        }
        out
    };
    let total = flat.len().max(1);
    let nllb_dir = model_dir(models_dir, nllb);
    let opus = (source == "tr").then(|| opus_dir(models_dir)).flatten();

    let translated = match opus {
        // Turc : OPUS tr → en, puis NLLB en → cible.
        Some(dir) => {
            let mut step1 = |done: &[String]| {
                progress(0.5 * done.len() as f32 / total as f32, format!("Traduction turc → anglais (OPUS) {}/{}", done.len(), total));
                if target == "en" {
                    on_chunk(&rejoin(done));
                }
            };
            let english = opus_translate(&dir, &flat, progress, &mut step1)?;
            if target == "en" {
                english
            } else {
                let mut step2 = |done: &[String]| {
                    progress(0.5 + 0.5 * done.len() as f32 / total as f32, format!("Traduction anglais → {target} (NLLB) {}/{}", done.len(), total));
                    on_chunk(&rejoin(done));
                };
                nllb_translate(&nllb_dir, &english, "en", target, progress, &mut step2)?
            }
        }
        None => {
            let mut step = |done: &[String]| {
                progress(done.len() as f32 / total as f32, format!("Traduction locale {}/{}", done.len(), total));
                on_chunk(&rejoin(done));
            };
            nllb_translate(&nllb_dir, &flat, source, target, progress, &mut step)?
        }
    };
    *LAST_USE.lock().unwrap() = Some(std::time::Instant::now());
    Ok(rejoin(&translated))
}

#[cfg(test)]
mod tests {
    use super::split_sentences;

    #[test]
    fn cjk_final_period() {
        assert_eq!(super::fix_punctuation("明天我会在车站等你.", "zh"), "明天我会在车站等你。");
        assert_eq!(super::fix_punctuation("Hello.", "fr"), "Hello.");
    }

    #[test]
    fn splits_multi_sentence_lines() {
        assert_eq!(split_sentences("Bunu sana kim söyledi? Ben hiçbir şey bilmiyorum."), ["Bunu sana kim söyledi?", "Ben hiçbir şey bilmiyorum."]);
        assert_eq!(split_sentences("Ne?! Hayır... Gel buraya."), ["Ne?!", "Hayır...", "Gel buraya."]);
        assert_eq!(split_sentences("一个。两个。"), ["一个。", "两个。"]);
        assert_eq!(split_sentences("Pas de ponctuation"), ["Pas de ponctuation"]);
        assert_eq!(split_sentences("3.5 kilo aldım."), ["3.5 kilo aldım."]);
    }
}
