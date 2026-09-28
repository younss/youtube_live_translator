//! Tout ce qui passe par `yt-dlp` : métadonnées, pistes de sous-titres, audio, recherche.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::subs::{self, Cue};

/// Les apps macOS lancées depuis le Finder n'héritent pas du PATH du shell :
/// on regarde aussi les emplacements Homebrew habituels.
pub fn find_bin(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .map(PathBuf::from)
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn ytdlp() -> Result<Command> {
    let bin = find_bin("yt-dlp").ok_or_else(|| anyhow!("yt-dlp introuvable — installez-le : brew install yt-dlp"))?;
    let mut cmd = Command::new(bin);
    // yt-dlp a besoin de ffmpeg/deno : on lui donne le même PATH élargi.
    let path = format!("/opt/homebrew/bin:/usr/local/bin:{}", std::env::var("PATH").unwrap_or_default());
    cmd.env("PATH", path).kill_on_drop(true);
    Ok(cmd)
}

async fn run(mut cmd: Command) -> Result<String> {
    let out = cmd.output().await.context("échec du lancement de yt-dlp")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().filter(|l| l.contains("ERROR")).last().unwrap_or(err.trim());
        bail!("yt-dlp : {last}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Extrait l'identifiant vidéo de toutes les formes d'URL YouTube courantes.
pub fn video_id(input: &str) -> Option<String> {
    let s = input.trim();
    let is_id = |id: &str| id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if is_id(s) {
        return Some(s.to_string());
    }
    let s = s.split('#').next()?;
    if let Some((_, query)) = s.split_once('?') {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("v=") {
                if is_id(v) {
                    return Some(v.to_string());
                }
            }
        }
    }
    let path = s.split('?').next()?;
    for marker in ["youtu.be/", "/shorts/", "/embed/", "/live/", "/v/"] {
        if let Some((_, rest)) = path.split_once(marker) {
            let id = rest.split('/').next().unwrap_or("");
            if is_id(id) {
                return Some(id.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub subtitles: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub automatic_captions: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub is_live: Option<bool>,
    #[serde(default)]
    pub duration: Option<f64>,
}

pub async fn metadata(id: &str) -> Result<Meta> {
    let mut cmd = ytdlp()?;
    cmd.args(["-J", "--skip-download", "--no-warnings", "--no-playlist"]).arg(watch_url(id));
    let json = run(cmd).await?;
    serde_json::from_str(&json).context("réponse yt-dlp illisible")
}

pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// Piste YouTube choisie : clé de langue + manuelle ou automatique.
#[derive(Debug, Clone)]
pub struct Track {
    pub key: String,
    pub lang: String,
    pub auto: bool,
}

/// Choisit la meilleure piste dans la langue source (ou la langue de la vidéo si "auto").
pub fn pick_source_track(meta: &Meta, source: &str) -> Option<Track> {
    let base = |k: &str| k.split(['-', '_']).next().unwrap_or(k).to_lowercase();
    let wanted: Option<String> = match source {
        "auto" => meta.language.as_deref().map(base),
        s => Some(s.to_string()),
    };

    // 1. Sous-titres manuels (meilleure qualité) dans la langue voulue.
    if let Some(lang) = &wanted {
        if let Some(k) = meta.subtitles.keys().filter(|k| *k != "live_chat").find(|k| base(k) == *lang) {
            return Some(Track { key: k.clone(), lang: lang.clone(), auto: false });
        }
    }
    // 2. Auto-captions d'origine (la piste "xx-orig" est la reconnaissance vocale réelle).
    if let Some(k) = meta.automatic_captions.keys().find(|k| k.ends_with("-orig")) {
        let lang = base(k);
        if wanted.as_deref().is_none_or(|w| w == lang) {
            return Some(Track { key: k.clone(), lang, auto: true });
        }
    }
    if let Some(lang) = &wanted {
        if let Some(k) = meta.automatic_captions.keys().find(|k| *k == lang) {
            return Some(Track { key: k.clone(), lang: lang.clone(), auto: true });
        }
    }
    // 3. En "auto" sans langue connue : n'importe quels sous-titres manuels.
    if source == "auto" {
        if let Some(k) = meta.subtitles.keys().find(|k| *k != "live_chat") {
            return Some(Track { key: k.clone(), lang: base(k), auto: false });
        }
    }
    None
}

/// Télécharge une piste de sous-titres et la convertit en cues.
pub async fn download_track(id: &str, track: &Track, dir: &Path) -> Result<Vec<Cue>> {
    let stem = format!("{id}.{}", if track.auto { "auto" } else { "manual" });
    let mut cmd = ytdlp()?;
    cmd.args(["--skip-download", "--no-warnings", "--no-playlist"])
        .arg(if track.auto { "--write-auto-subs" } else { "--write-subs" })
        .args(["--sub-langs", &track.key, "--sub-format", "json3/vtt/best"])
        .arg("-o")
        .arg(dir.join(format!("{stem}.%(ext)s")))
        .arg(watch_url(id));
    run(cmd).await?;

    for ext in ["json3", "vtt"] {
        let file = dir.join(format!("{stem}.{}.{ext}", track.key));
        if let Ok(content) = tokio::fs::read_to_string(&file).await {
            let _ = tokio::fs::remove_file(&file).await;
            let cues = if ext == "json3" { subs::parse_json3(&content)? } else { subs::parse_vtt(&content) };
            return Ok(cues);
        }
    }
    bail!("YouTube n'a renvoyé aucun fichier pour la piste « {} »", track.key)
}

/// URL du flux audio seul (HLS en priorité : c'est celui que YouTube sert sans 403),
/// que ffmpeg peut lire directement pendant que Whisper travaille.
pub async fn audio_stream_url(id: &str) -> Result<String> {
    let mut cmd = ytdlp()?;
    cmd.args(["-g", "--no-warnings", "--no-playlist", "-f", "ba[protocol=m3u8_native]/ba/b"]).arg(watch_url(id));
    let out = run(cmd).await?;
    out.lines().map(str::trim).find(|l| l.starts_with("http")).map(String::from).ok_or_else(|| anyhow!("aucun flux audio"))
}

/// Télécharge la piste audio (pour Whisper). Renvoie le chemin du fichier.
/// YouTube renvoie de plus en plus souvent 403 sur les fichiers audio directs alors que
/// le HLS (celui du lecteur) passe : on essaie donc le HLS d'abord.
pub async fn download_audio(id: &str, dir: &Path) -> Result<PathBuf> {
    let mut last_err = anyhow!("aucun format audio");
    for format in ["ba[protocol=m3u8_native]/b[protocol=m3u8_native]", "bestaudio/best"] {
        let mut cmd = ytdlp()?;
        cmd.args(["-f", format, "--no-warnings", "--no-playlist", "--no-part", "--force-overwrites"])
            .arg("-o")
            .arg(dir.join(format!("{id}.audio.%(ext)s")))
            .args(["--print", "after_move:filepath"])
            .arg(watch_url(id));
        match run(cmd).await {
            Ok(out) => {
                let path = out.lines().last().map(str::trim).unwrap_or_default();
                if !path.is_empty() && Path::new(path).is_file() {
                    return Ok(PathBuf::from(path));
                }
                last_err = anyhow!("yt-dlp n'a pas indiqué le fichier audio");
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Flux directs lisibles par une balise `<video>` : vidéo H.264 + audio AAC séparés
/// (YouTube ne sert presque plus de flux combinés), ou un manifeste HLS pour les directs.
#[derive(Debug, Clone, Serialize)]
pub struct Streams {
    pub title: String,
    pub is_live: bool,
    pub video: String,
    pub audio: Option<String>,
    pub hls: bool,
}

pub async fn streams(id: &str, max_height: u32) -> Result<Streams> {
    let h = max_height;
    let selector = format!(
        "bv*[vcodec^=avc1][height<={h}][protocol=https]+ba[ext=m4a][protocol=https]\
         /b[vcodec^=avc1][height<={h}][protocol=https]\
         /bv*[height<={h}][protocol=https]+ba[protocol=https]\
         /b[protocol*=m3u8]/b"
    );
    let mut cmd = ytdlp()?;
    cmd.args(["--no-warnings", "--no-playlist", "-f", &selector])
        .args(["-O", "%(title)s", "-O", "%(is_live)s", "-O", "%(urls)s"])
        .arg(watch_url(id));
    let out = run(cmd).await?;
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let title = lines.next().unwrap_or_default().to_string();
    let is_live = lines.next() == Some("True");
    let urls: Vec<String> = lines.filter(|l| l.starts_with("http")).map(String::from).collect();
    let video = urls.first().cloned().ok_or_else(|| anyhow!("aucun flux lisible pour cette vidéo"))?;
    let hls = video.contains(".m3u8") || video.contains("/manifest/hls");
    Ok(Streams { title, is_live, audio: urls.get(1).cloned(), video, hls })
}

/// Manifeste HLS « maître » (audio + vidéo, qualité adaptative). WebKit le lit nativement,
/// sans les coupures qu'il a sur les MP4 fragmentés de YouTube.
pub async fn hls_master(id: &str) -> Result<Streams> {
    let mut cmd = ytdlp()?;
    cmd.args(["-J", "--no-warnings", "--no-playlist"]).arg(watch_url(id));
    let json: serde_json::Value = serde_json::from_str(&run(cmd).await?)?;
    let manifest = json["formats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["manifest_url"].as_str())
        .find(|u| u.contains("m3u8") || u.contains("/manifest/hls"))
        .ok_or_else(|| anyhow!("pas de flux HLS pour cette vidéo"))?;
    Ok(Streams {
        title: json["title"].as_str().unwrap_or_default().to_string(),
        is_live: json["is_live"].as_bool().unwrap_or(false),
        video: manifest.to_string(),
        audio: None,
        hls: true,
    })
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: Option<f64>,
}

/// Vidéos suivantes du « Mix » YouTube de cette vidéo (la playlist radio `list=RD<id>`).
pub async fn mix(id: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let mut cmd = ytdlp()?;
    cmd.args(["--flat-playlist", "-J", "--no-warnings", "--playlist-end", &(limit + 1).to_string()])
        .arg(format!("https://www.youtube.com/watch?v={id}&list=RD{id}"));
    Ok(parse_entries(&run(cmd).await?)?.into_iter().filter(|h| h.id != id).take(limit).collect())
}

fn parse_entries(json: &str) -> Result<Vec<SearchHit>> {
    let json: serde_json::Value = serde_json::from_str(json)?;
    let entries = json["entries"].as_array().cloned().unwrap_or_default();
    Ok(entries
        .into_iter()
        .filter_map(|e| {
            Some(SearchHit {
                id: e["id"].as_str()?.to_string(),
                title: e["title"].as_str().unwrap_or("?").to_string(),
                channel: e["channel"].as_str().or(e["uploader"].as_str()).unwrap_or("").to_string(),
                duration: e["duration"].as_f64(),
            })
        })
        .collect())
}

pub async fn search(query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let mut cmd = ytdlp()?;
    cmd.args(["--flat-playlist", "-J", "--no-warnings"]).arg(format!("ytsearch{limit}:{query}"));
    parse_entries(&run(cmd).await?)
}

#[cfg(test)]
mod tests {
    use super::video_id;

    #[test]
    fn parses_common_urls() {
        for url in [
            "https://www.youtube.com/watch?v=arj7oStGLkU",
            "https://youtube.com/watch?feature=share&v=arj7oStGLkU&t=10",
            "https://youtu.be/arj7oStGLkU?si=abc",
            "https://www.youtube.com/shorts/arj7oStGLkU",
            "https://www.youtube.com/live/arj7oStGLkU",
            "https://www.youtube.com/embed/arj7oStGLkU",
            "arj7oStGLkU",
        ] {
            assert_eq!(video_id(url).as_deref(), Some("arj7oStGLkU"), "{url}");
        }
        assert_eq!(video_id("https://example.com"), None);
    }
}
