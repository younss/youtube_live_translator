//! Transcription locale avec whisper.cpp quand YouTube ne fournit aucun sous-titre.

use std::path::{Path, PathBuf};

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

pub async fn transcribe(model: &Path, wav: &Path, lang: &str, progress: &Progress) -> Result<Vec<Cue>> {
    let bin = find_bin("whisper-cli").ok_or_else(|| anyhow!("whisper-cli introuvable — brew install whisper-cpp"))?;
    let prefix = wav.with_extension("");
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let mut child = Command::new(bin)
        .arg("-m")
        .arg(model)
        .arg("-f")
        .arg(wav)
        // -mc 0 évite les boucles de répétition sur la musique, -sns retire les « ♪ ».
        .args(["-l", lang, "-oj", "-pp", "-mc", "0", "-sns", "-t", &threads.to_string(), "-of"])
        .arg(&prefix)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut tail = Vec::new();
    while let Some(line) = lines.next_line().await? {
        // Format : "whisper_print_progress_callback: progress =  45%"
        if let Some(pct) = line.split("progress =").nth(1).and_then(|p| p.trim().trim_end_matches('%').parse::<f32>().ok()) {
            progress(pct / 100.0, format!("Transcription Whisper {pct:.0}%"));
        } else {
            tail.push(line);
            if tail.len() > 5 {
                tail.remove(0);
            }
        }
    }
    if !child.wait().await?.success() {
        bail!("whisper-cli a échoué : {}", tail.join(" | "));
    }

    #[derive(Deserialize)]
    struct Out {
        transcription: Vec<Seg>,
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
    Ok(out
        .transcription
        .into_iter()
        .filter(|s| !s.text.trim().is_empty() && !is_hallucination(&s.text))
        .map(|s| {
            let text = s.text.trim().to_string();
            Cue { start: s.offsets.from / 1000.0, end: s.offsets.to / 1000.0, orig: text.clone(), text }
        })
        .collect())
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
