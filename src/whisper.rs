//! Transcription locale avec whisper.cpp **compilé dans l'app** (Metal) quand YouTube ne
//! fournit aucun sous-titre. Le modèle est chargé une seule fois et partagé par tous les jobs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncWriteExt;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::subs::Cue;
use crate::translate::Progress;

pub fn model_path(models_dir: &Path, size: &str) -> PathBuf {
    models_dir.join(format!("ggml-{size}.bin"))
}

/// Télécharge le modèle multilingue depuis Hugging Face s'il n'est pas déjà là.
pub async fn ensure_model(models_dir: &Path, size: &str, progress: &Progress) -> Result<PathBuf> {
    let path = model_path(models_dir, size);
    if path.is_file() {
        return Ok(path);
    }
    tokio::fs::create_dir_all(models_dir).await?;
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{size}.bin");
    let mut resp = reqwest::get(&url).await?.error_for_status().context("téléchargement du modèle Whisper")?;
    let total = resp.content_length().unwrap_or(0);
    let tmp = path.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut got = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        got += chunk.len() as u64;
        if total > 0 {
            progress(got as f32 / total as f32, format!("Modèle Whisper {size} : {} / {} Mo", got >> 20, total >> 20));
        }
    }
    file.flush().await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(path)
}

/// Rappels pour afficher la transcription au fil de l'eau (avant la fin de Whisper).
#[derive(Clone)]
pub struct Live {
    /// Chaque segment dès que Whisper l'écrit.
    pub on_segment: Arc<dyn Fn(Cue) + Send + Sync>,
    /// Langue détectée (si la source est sur « Auto »), annoncée avant la transcription.
    pub on_language: Arc<dyn Fn(String) + Send + Sync>,
}

/// Le modèle (~550 Mo) reste chargé entre deux vidéos au lieu d'être relu à chaque fois.
static MODEL: Mutex<Option<(PathBuf, Arc<WhisperContext>)>> = Mutex::new(None);

fn load(model: &Path) -> Result<Arc<WhisperContext>> {
    let mut guard = MODEL.lock().map_err(|_| anyhow!("Whisper indisponible"))?;
    if let Some((path, ctx)) = guard.as_ref() {
        if path == model {
            return Ok(ctx.clone());
        }
    }
    whisper_rs::install_logging_hooks(); // silencieux : whisper.cpp est très bavard sur stderr
    let params = WhisperContextParameters { use_gpu: true, flash_attn: true, ..Default::default() };
    let path = model.to_str().ok_or_else(|| anyhow!("chemin du modèle invalide"))?;
    let ctx = Arc::new(WhisperContext::new_with_params(path, params).map_err(|e| anyhow!("modèle Whisper : {e}"))?);
    *guard = Some((model.to_path_buf(), ctx.clone()));
    Ok(ctx)
}

fn threads() -> i32 {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8) as i32
}

/// Détecte la langue parlée sur un extrait (on évite le tout début, souvent instrumental).
fn detect_language(ctx: &WhisperContext, pcm: &[f32]) -> Option<String> {
    let mut state = ctx.create_state().ok()?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("auto"));
    params.set_detect_language(true);
    params.set_n_threads(threads());
    silence(&mut params);
    let secs = pcm.len() as f64 / 16000.0;
    params.set_offset_ms((secs / 3.0).min(30.0) as i32 * 1000);
    state.full(params, pcm).ok()?;
    whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(str::to_string)
}

fn silence(params: &mut FullParams) {
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
}

/// Une passe de transcription sur `pcm`, depuis `offset` secondes (sur `duration` si précisé).
fn run_pass(
    ctx: &WhisperContext,
    pcm: &[f32],
    lang: &str,
    offset: f64,
    duration: Option<f64>,
    progress: Option<Progress>,
    live: Option<Live>,
) -> Result<Vec<Cue>> {
    let mut state = ctx.create_state().map_err(|e| anyhow!("Whisper : {e}"))?;
    let mut params = FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 });
    params.set_language(Some(lang));
    params.set_n_threads(threads());
    // Pas de contexte entre fenêtres : évite les boucles de répétition sur la musique ;
    // suppress_nst retire les « ♪ » et autres jetons non verbaux.
    params.set_n_max_text_ctx(0);
    params.set_suppress_nst(true);
    // Horodatage par jeton et segments courts coupés entre deux mots : sans cela un segment
    // peut couvrir 10 s de parole (et commencer pendant le silence qui précède), et le
    // sous-titre apparaît bien avant la voix. `max_len` est en octets UTF-8.
    params.set_token_timestamps(true);
    params.set_split_on_word(true);
    params.set_max_len(70);
    params.set_offset_ms((offset * 1000.0) as i32);
    params.set_duration_ms(duration.map_or(0, |d| (d * 1000.0) as i32));
    silence(&mut params);
    if let Some(progress) = progress {
        params.set_progress_callback_safe(move |pct: i32| progress(pct as f32 / 100.0, format!("Transcription Whisper {pct}%")));
    }
    if let Some(live) = live {
        params.set_segment_callback_safe_lossy(move |seg: whisper_rs::SegmentCallbackData| {
            if let Some(cue) = to_cue(seg.start_timestamp, seg.end_timestamp, &seg.text) {
                (live.on_segment)(cue);
            }
        });
    }
    state.full(params, pcm).map_err(|e| anyhow!("Whisper : {e}"))?;
    Ok(state
        .as_iter()
        .filter_map(|seg| to_cue(seg.start_timestamp(), seg.end_timestamp(), &seg.to_str_lossy().ok()?))
        .collect())
}

/// Les horodatages de whisper.cpp sont en centièmes de seconde.
fn to_cue(t0: i64, t1: i64, text: &str) -> Option<Cue> {
    let text = text.trim().to_string();
    if text.is_empty() || is_hallucination(&text) {
        return None;
    }
    Some(Cue { start: t0 as f64 / 100.0, end: t1 as f64 / 100.0, orig: text.clone(), text })
}

/// Tampon audio alimenté au fil de l'eau (flux HLS de YouTube décodé en Rust) et consommé par
/// Whisper morceau par morceau : pas besoin d'attendre tout l'audio pour commencer.
#[derive(Default)]
pub struct PcmStream {
    state: Mutex<PcmState>,
    ready: Condvar,
}

#[derive(Default)]
struct PcmState {
    /// Stocké en 16 bits (largement suffisant pour la parole) : une vidéo de 2 h tient en
    /// ~230 Mo au lieu de ~460 Mo en f32. Seule la fenêtre en cours est convertie en f32.
    samples: Vec<i16>,
    done: bool,
    error: Option<String>,
}

impl PcmStream {
    pub fn push(&self, samples: &[f32]) {
        let quantized = samples.iter().map(|&x| (x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        self.state.lock().unwrap().samples.extend(quantized);
        self.ready.notify_all();
    }

    pub fn finish(&self, error: Option<String>) {
        let mut st = self.state.lock().unwrap();
        st.done = true;
        st.error = error;
        self.ready.notify_all();
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().samples.len()
    }

    fn is_done(&self) -> bool {
        self.state.lock().unwrap().done
    }

    /// Attend qu'au moins `n` échantillons soient là (ou la fin du flux) et renvoie une copie
    /// de `from..min(n, dispo)` ainsi que « le flux est-il terminé ».
    fn wait_slice(&self, from: usize, n: usize) -> Result<(Vec<f32>, bool)> {
        let mut st = self.state.lock().unwrap();
        while st.samples.len() < n && !st.done {
            st = self.ready.wait(st).unwrap();
        }
        if let Some(e) = &st.error {
            if st.samples.len() <= from {
                bail!("flux audio : {e}");
            }
        }
        let end = n.min(st.samples.len());
        let at_end = st.done && end == st.samples.len();
        let window = st.samples[from.min(end)..end].iter().map(|&x| x as f32 / i16::MAX as f32).collect();
        Ok((window, at_end))
    }
}

const RATE: usize = 16_000;
const WINDOW: usize = 30 * RATE;

/// Transcrit le flux audio morceau par morceau (bloquant : à appeler depuis `spawn_blocking`).
/// Chaque segment est transmis à `live` dès qu'il est sûr (pas coupé en fin de morceau).
/// Renvoie aussi la langue détectée quand la source est sur « Auto ».
///
/// Quand une fenêtre de 30 s est surtout instrumentale (intro, pont d'une chanson), Whisper y
/// invente du texte (filtré) et perd les paroles de la fenêtre : on la relance alors décalée
/// de quelques secondes, ce qui suffit en général à retrouver la voix.
/// Position de lecture (en ms) communiquée par l'interface ; `u64::MAX` = inconnue.
pub type Playhead = Arc<AtomicU64>;

/// Zones déjà transcrites, en échantillons, triées et fusionnées.
#[derive(Default)]
struct Covered(Vec<(usize, usize)>);

impl Covered {
    fn add(&mut self, a: usize, b: usize) {
        if b <= a {
            return;
        }
        self.0.push((a, b));
        self.0.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(self.0.len());
        for (a, b) in self.0.drain(..) {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        self.0 = merged;
    }

    /// Premier échantillon non transcrit à partir de `x`.
    fn first_gap(&self, mut x: usize) -> usize {
        for &(a, b) in &self.0 {
            if x >= a && x < b {
                x = b;
            }
        }
        x
    }

    /// Début de la prochaine zone déjà faite après `x` (pour ne pas la retranscrire).
    fn next_start(&self, x: usize) -> Option<usize> {
        self.0.iter().map(|&(a, _)| a).find(|&a| a > x)
    }

    fn total(&self) -> usize {
        self.0.iter().map(|(a, b)| b - a).sum()
    }
}

pub fn transcribe_stream(
    model: &Path,
    stream: &PcmStream,
    lang: &str,
    duration: Option<f64>,
    progress: &Progress,
    live: Option<&Live>,
    playhead: &Playhead,
) -> Result<(Vec<Cue>, Option<String>)> {
    progress(0.0, "Chargement de Whisper…".into());
    let ctx = load(model)?;
    let mut detected = None;
    let lang = if lang == "auto" {
        progress(0.0, "Détection de la langue…".into());
        // 40 s suffisent ; on évite les 10 premières, souvent instrumentales.
        let (pcm, _) = stream.wait_slice(0, 40 * RATE)?;
        let skip = if pcm.len() > 20 * RATE { 10 * RATE } else { 0 };
        detected = detect_language(&ctx, &pcm[skip..]);
        if let (Some(live), Some(l)) = (live, &detected) {
            (live.on_language)(l.clone());
        }
        detected.clone().unwrap_or_else(|| "auto".into())
    } else {
        lang.to_string()
    };

    let total = duration.map(|d| (d * RATE as f64) as usize);
    let mut cues: Vec<Cue> = Vec::new();
    let mut covered = Covered::default();
    let mut misses = 0;
    let mut last_pos = usize::MAX;
    loop {
        // Priorité à ce que l'utilisateur regarde : la zone autour de la tête de lecture
        // (3 min devant elle), puis le reste de la vidéo depuis le début.
        let avail = stream.len();
        let done = stream.is_done();
        let head = match playhead.load(Ordering::Relaxed) {
            u64::MAX => None,
            ms => Some((ms as usize * RATE / 1000).saturating_sub(2 * RATE)),
        };
        let fill = covered.first_gap(0);
        // Terminé seulement quand TOUT l'audio est couvert, du début à la fin (et pas dès que
        // la zone de lecture l'est : sinon, tête de lecture en fin de vidéo = rien de transcrit).
        if done && fill >= avail {
            break;
        }
        let pos = match head.map(|h| (h, covered.first_gap(h))) {
            // Zone de lecture pas encore faite et son audio est arrivé : on y va.
            Some((h, g)) if g < h + 180 * RATE && g + RATE / 2 <= avail => g,
            // Son audio n'est pas encore arrivé et rien d'autre à faire : on l'attend.
            Some((h, g)) if g < h + 180 * RATE && !done && fill >= avail => g,
            // Sinon (zone faite, au-delà de la fin…) : on complète depuis le début.
            _ => fill,
        };
        // Saut vers une autre zone : le compteur d'échecs repart de zéro.
        if last_pos == usize::MAX || pos.abs_diff(last_pos) > WINDOW {
            misses = 0;
        }
        last_pos = pos;

        // La fenêtre s'arrête à la prochaine zone déjà transcrite.
        let limit = covered.next_start(pos).map_or(pos + WINDOW, |n| n.min(pos + WINDOW));
        let (pcm, at_end) = stream.wait_slice(pos, limit)?;
        if pcm.len() < RATE / 2 {
            // Bout d'audio trop court pour Whisper (fin de flux ou petit trou entre deux zones).
            covered.add(pos, pos + pcm.len().max(1));
            continue;
        }
        let touches_done = pos + pcm.len() >= limit && limit < pos + WINDOW;
        let base = pos as f64 / RATE as f64;
        let segs = run_pass(&ctx, &pcm, &lang, 0.0, None, None, None)?;
        let slice_end = pcm.len() as f64 / RATE as f64;
        // Hors fin de flux (ou bord d'une zone déjà faite), on garde la fin pour la fenêtre
        // suivante : un segment qui touche la limite est peut-être coupé au milieu d'un mot.
        let keep: Vec<Cue> = segs
            .into_iter()
            .filter(|c| at_end || touches_done || c.end < slice_end - 1.0)
            .map(|c| Cue { start: c.start + base, end: c.end + base, ..c })
            .collect();

        let next = if touches_done || at_end {
            misses = 0;
            pos + pcm.len()
        } else if let Some(last) = keep.last() {
            misses = 0;
            ((last.end * RATE as f64) as usize).max(pos + RATE)
        } else if misses < 8 {
            // Rien d'exploitable : même zone, fenêtre décalée de 4 s (jusqu'à ~30 s plus loin).
            misses += 1;
            pos + 4 * RATE
        } else {
            misses = 0;
            pos + WINDOW - RATE
        };
        covered.add(pos, next);
        for c in keep {
            if let Some(live) = live {
                (live.on_segment)(c.clone());
            }
            cues.push(c);
        }
        let known = total.unwrap_or_else(|| stream.len()).max(1);
        let pct = (covered.total() as f32 / known as f32).min(1.0);
        progress(pct, format!("Transcription Whisper {:.0}%", pct * 100.0));
    }
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    cues.dedup_by(|b, a| (b.start - a.start).abs() < 0.2 && b.orig == a.orig);
    // Whisper ne sert qu'une fois par vidéo (la transcription est ensuite en cache) :
    // on libère le modèle tout de suite. Un job concurrent garde sa copie `Arc` jusqu'à sa fin.
    drop(ctx);
    if let Ok(mut guard) = MODEL.lock() {
        *guard = None;
    }
    crate::release_memory();
    Ok((cues, detected))
}

/// Sur la musique ou le silence, Whisper « invente » des crédits de sous-titrage
/// appris dans ses données d'entraînement. On les retire.
fn is_hallucination(text: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "ترجمة", "الترجمة", "اشترك", "sous-titr", "sous titr", "subtitles by", "subtitled by",
        "altyazı", "untertitel", "subtítulos", "amara.org", "thanks for watching", "merci d'avoir regardé",
        "abone ol",
        // Crédits inventés fréquents en japonais, coréen, chinois, hindi, russe, portugais.
        "ご視聴ありがとう", "チャンネル登録", "시청해 주셔서", "구독과 좋아요", "字幕", "请不吝点赞", "订阅",
        "सब्सक्राइब", "субтитры", "legendas pela",
    ];
    let t = text.to_lowercase();
    t.chars().filter(|c| c.is_alphabetic()).count() < 2 || PATTERNS.iter().any(|p| t.contains(p))
}

#[cfg(test)]
mod tests {
    #[test]
    fn filters_credit_hallucinations() {
        assert!(super::is_hallucination("Sous-titres réalisés par la communauté d'Amara.org"));
        assert!(super::is_hallucination("♪"));
        assert!(!super::is_hallucination("Bonjour à tous"));
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::Covered;

    #[test]
    fn covered_merges_and_finds_gaps() {
        let mut c = Covered::default();
        c.add(100, 200);
        c.add(0, 50);
        c.add(40, 120);
        assert_eq!(c.0, vec![(0, 200)]);
        c.add(500, 600);
        assert_eq!(c.first_gap(0), 200);
        assert_eq!(c.first_gap(550), 600);
        assert_eq!(c.next_start(250), Some(500));
        assert_eq!(c.total(), 300);
    }
}
