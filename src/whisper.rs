//! Transcription locale avec whisper.cpp quand YouTube ne fournit aucun sous-titre.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::subs::Cue;
use crate::translate::Progress;
use crate::youtube::find_bin;

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

/// whisper-cli veut du WAV 16 kHz mono.
pub async fn to_wav(input: &Path) -> Result<PathBuf> {
    let ffmpeg = find_bin("ffmpeg").ok_or_else(|| anyhow!("ffmpeg introuvable — brew install ffmpeg"))?;
    let out = input.with_extension("wav");
    let status = Command::new(ffmpeg)
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(&out)
        .status()
        .await?;
    if !status.success() {
        bail!("ffmpeg n'a pas pu convertir l'audio");
    }
    Ok(out)
}

/// Transcrit `wav` depuis `offset` secondes (sur `duration` secondes si précisé).
/// Rappels pour afficher la transcription au fil de l'eau (avant la fin de Whisper).
#[derive(Clone)]
pub struct Live {
    /// Chaque segment dès que Whisper l'écrit.
    pub on_segment: Arc<dyn Fn(Cue) + Send + Sync>,
    /// Langue détectée (si la source est sur « Auto »), annoncée dès le début.
    pub on_language: Arc<dyn Fn(String) + Send + Sync>,
}

async fn run_whisper(
    model: &Path,
    wav: &Path,
    lang: &str,
    offset: f64,
    duration: Option<f64>,
    tag: &str,
    progress: Option<&Progress>,
    live: Option<&Live>,
) -> Result<(Vec<Cue>, Option<String>)> {
    let bin = find_bin("whisper-cli").ok_or_else(|| anyhow!("whisper-cli introuvable — brew install whisper-cpp"))?;
    let prefix = PathBuf::from(format!("{}{tag}", wav.with_extension("").display()));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let mut child = Command::new(bin)
        .arg("-m")
        .arg(model)
        .arg("-f")
        .arg(wav)
        // -mc 0 évite les boucles de répétition sur la musique, -sns retire les « ♪ ».
        .args(["-l", lang, "-oj", "-pp", "-mc", "0", "-sns", "-t", &threads.to_string()])
        .args(["-ot", &((offset * 1000.0) as u64).to_string()])
        .args(["-d", &duration.map_or(0, |d| (d * 1000.0) as u64).to_string(), "-of"])
        .arg(&prefix)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // Whisper écrit chaque segment sur stdout dès qu'il est décodé :
    // « [00:00:05.000 --> 00:00:09.000]  texte ».
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let live_out = live.cloned();
    let reader = tokio::spawn(async move {
        let mut lines = stdout.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let (Some(live), Some(cue)) = (&live_out, parse_segment_line(&line)) {
                (live.on_segment)(cue);
            }
        }
    });

    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut tail = Vec::new();
    while let Some(line) = lines.next_line().await? {
        // Format : "whisper_print_progress_callback: progress =  45%"
        if let Some(pct) = line.split("progress =").nth(1).and_then(|p| p.trim().trim_end_matches('%').parse::<f32>().ok()) {
            if let Some(progress) = progress {
                progress(pct / 100.0, format!("Transcription Whisper {pct:.0}%"));
            }
        } else if let Some(l) = line.split("auto-detected language:").nth(1) {
            if let (Some(live), Some(code)) = (live, l.split_whitespace().next()) {
                (live.on_language)(code.to_string());
            }
        } else {
            tail.push(line);
            if tail.len() > 5 {
                tail.remove(0);
            }
        }
    }
    let _ = reader.await;
    if !child.wait().await?.success() {
        bail!("whisper-cli a échoué : {}", tail.join(" | "));
    }

    #[derive(Deserialize)]
    struct Out {
        transcription: Vec<Seg>,
        #[serde(default)]
        result: Option<Detected>,
    }
    #[derive(Deserialize)]
    struct Detected {
        language: String,
    }
    #[derive(Deserialize)]
    struct Seg {
        offsets: Offsets,
        text: String,
    }
    #[derive(Deserialize)]
    struct Offsets {
        from: f64,
        to: f64,
    }
    let json_path = PathBuf::from(format!("{}.json", prefix.display()));
    let out: Out = serde_json::from_str(&tokio::fs::read_to_string(&json_path).await?)?;
    let _ = tokio::fs::remove_file(json_path).await;
    let detected = out.result.map(|r| r.language).filter(|l| !l.is_empty() && l != "auto");
    let cues = out
        .transcription
        .into_iter()
        .filter(|s| !s.text.trim().is_empty() && !is_hallucination(&s.text))
        .map(|s| {
            let text = s.text.trim().to_string();
            Cue { start: s.offsets.from / 1000.0, end: s.offsets.to / 1000.0, orig: text.clone(), text }
        })
        .collect();
    Ok((cues, detected))
}

fn parse_segment_line(line: &str) -> Option<Cue> {
    let rest = line.trim_start().strip_prefix('[')?;
    let (times, text) = rest.split_once(']')?;
    let (a, b) = times.split_once("-->")?;
    let ts = |t: &str| -> Option<f64> {
        let mut parts = t.trim().split(':').rev();
        let s: f64 = parts.next()?.parse().ok()?;
        let m: f64 = parts.next().unwrap_or("0").parse().ok()?;
        let h: f64 = parts.next().unwrap_or("0").parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    };
    let text = text.trim().to_string();
    if text.is_empty() || is_hallucination(&text) {
        return None;
    }
    Some(Cue { start: ts(a)?, end: ts(b)?, orig: text.clone(), text })
}

/// Whisper découpe l'audio en fenêtres de 30 s. Quand une fenêtre mêle surtout musique et
/// voix (intro, pont instrumental d'une chanson), il y invente du texte (filtré ensuite) et
/// perd les paroles de toute la fenêtre. On repère ces trous et on les retranscrit par de
/// courtes passes qui démarrent un peu plus loin, pour décaler la fenêtre.
pub async fn transcribe(
    model: &Path,
    wav: &Path,
    lang: &str,
    progress: &Progress,
    live: Option<&Live>,
) -> Result<(Vec<Cue>, Option<String>)> {
    let (mut cues, detected) = run_whisper(model, wav, lang, 0.0, None, "", Some(progress), live).await?;
    // La langue détectée sert aussi aux passes de réparation (plus fiables avec une langue fixe).
    let probe_lang = detected.clone().filter(|_| lang == "auto").unwrap_or_else(|| lang.to_string());

    const GAP: f64 = 10.0;
    let mut gaps: Vec<(f64, f64)> = Vec::new();
    let first = cues.first().map_or(0.0, |c| c.start);
    if first > 6.0 {
        gaps.push((0.0, first));
    }
    gaps.extend(cues.windows(2).filter(|w| w[1].start - w[0].end > GAP).map(|w| (w[0].end, w[1].start)));

    for (n, (from, to)) in gaps.iter().copied().enumerate() {
        progress(0.99, format!("Recherche des paroles manquantes ({}/{})…", n + 1, gaps.len()));
        let mut offset = from + 2.0;
        while offset < to - 1.0 {
            let (probe, _) = run_whisper(model, wav, &probe_lang, offset, Some(to - offset + 2.0), ".gap", None, None).await?;
            let found: Vec<Cue> = probe.into_iter().filter(|c| c.start >= from - 0.3 && c.start < to - 0.3).collect();
            if !found.is_empty() {
                for mut c in found {
                    c.end = c.end.min(to);
                    if let Some(live) = live {
                        (live.on_segment)(c.clone());
                    }
                    cues.push(c);
                }
                break;
            }
            offset += 3.0;
        }
    }
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok((cues, detected))
}

/// Sur la musique ou le silence, Whisper « invente » des crédits de sous-titrage
/// appris dans ses données d'entraînement. On les retire.
fn is_hallucination(text: &str) -> bool {
    const PATTERNS: [&str; 14] = [
        "ترجمة", "الترجمة", "اشترك", "sous-titr", "sous titr", "subtitles by", "subtitled by",
        "altyazı", "untertitel", "subtítulos", "amara.org", "thanks for watching", "merci d'avoir regardé",
        "abone ol",
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
mod segment_tests {
    #[test]
    fn parses_whisper_stdout_line() {
        let c = super::parse_segment_line("[00:01:05.500 --> 00:01:09.000]   Bonjour tout le monde").unwrap();
        assert!((c.start - 65.5).abs() < 1e-9 && (c.end - 69.0).abs() < 1e-9);
        assert_eq!(c.orig, "Bonjour tout le monde");
        assert!(super::parse_segment_line("whisper_init: loading model").is_none());
    }
}
