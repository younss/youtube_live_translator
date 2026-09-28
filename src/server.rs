//! Serveur HTTP local : sert l'interface embarquée et l'API de génération de sous-titres.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::subs::{self, Cue};
use crate::translate::{self, Engine, Progress};
use crate::{whisper, youtube};

#[derive(Embed)]
#[folder = "ui/"]
struct Assets;

#[derive(Default, Clone, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    anthropic_key: Option<String>,
    #[serde(default = "default_model")]
    whisper_model: String,
}

fn default_model() -> String {
    "large-v3-turbo-q5_0".into()
}

#[derive(Clone, Serialize)]
struct Job {
    state: &'static str,
    stage: String,
    progress: f32,
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Clone)]
struct AppState {
    jobs: Arc<Mutex<HashMap<u64, Job>>>,
    next_id: Arc<AtomicU64>,
    config: Arc<Mutex<Config>>,
    streams: Arc<Mutex<HashMap<String, (std::time::Instant, youtube::Streams)>>>,
    cache_dir: PathBuf,
    config_path: PathBuf,
}

impl AppState {
    fn new() -> Self {
        let cache_dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("youtube-live-translator");
        let config_path = dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("youtube-live-translator")
            .join("config.json");
        let mut config: Config = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if config.whisper_model.is_empty() {
            config.whisper_model = default_model();
        }
        Self {
            jobs: Default::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            config: Arc::new(Mutex::new(config)),
            streams: Default::default(),
            cache_dir,
            config_path,
        }
    }

    fn api_key(&self) -> Option<String> {
        let from_cfg = self.config.lock().unwrap().anthropic_key.clone().filter(|k| !k.trim().is_empty());
        from_cfg.or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()))
    }

    fn update_job(&self, id: u64, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(&id) {
            f(job);
        }
    }
}

pub fn router() -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/settings", post(save_settings))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/search", get(search))
        .route("/api/stream/{id}", get(stream))
        .route("/api/export", post(export_srt))
        .fallback(static_file)
        .with_state(AppState::new())
}

async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref().to_string()), (header::CACHE_CONTROL, "no-cache".into())], file.data)
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn err(status: StatusCode, e: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

async fn status(State(st): State<AppState>) -> Json<Value> {
    let cfg = st.config.lock().unwrap().clone();
    let models = st.cache_dir.join("models");
    Json(json!({
        "ytdlp": youtube::find_bin("yt-dlp").is_some(),
        "ffmpeg": youtube::find_bin("ffmpeg").is_some(),
        "whisper": youtube::find_bin("whisper-cli").is_some(),
        "claude_key": st.api_key().is_some(),
        "whisper_model": cfg.whisper_model,
        "whisper_model_ready": whisper::model_path(&models, &cfg.whisper_model).is_file(),
        "langs": translate::LANGS.iter().map(|(c, n)| json!({"code": c, "name": n})).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct SettingsReq {
    anthropic_key: Option<String>,
    whisper_model: Option<String>,
}

async fn save_settings(State(st): State<AppState>, Json(req): Json<SettingsReq>) -> Response {
    let cfg = {
        let mut cfg = st.config.lock().unwrap();
        if let Some(k) = req.anthropic_key {
            cfg.anthropic_key = Some(k.trim().to_string()).filter(|k| !k.is_empty());
        }
        if let Some(m) = req.whisper_model.filter(|m| ["tiny", "base", "small", "medium", "large-v3-turbo-q5_0"].contains(&m.as_str())) {
            cfg.whisper_model = m;
        }
        cfg.clone()
    };
    let write = async {
        if let Some(dir) = st.config_path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&st.config_path, serde_json::to_vec_pretty(&cfg)?).await?;
        anyhow::Ok(())
    };
    match write.await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize, Clone)]
struct JobReq {
    url: String,
    /// "auto" ou un code langue.
    source: String,
    target: String,
    /// "auto" (YouTube puis Whisper), "youtube" ou "whisper".
    mode: String,
    /// "google", "claude" ou "youtube".
    translator: String,
    #[serde(default)]
    refresh: bool,
}

async fn create_job(State(st): State<AppState>, Json(req): Json<JobReq>) -> Response {
    if youtube::video_id(&req.url).is_none() {
        return err(StatusCode::BAD_REQUEST, "URL YouTube invalide");
    }
    let id = st.next_id.fetch_add(1, Ordering::Relaxed);
    st.jobs.lock().unwrap().insert(
        id,
        Job { state: "running", stage: "Démarrage…".into(), progress: 0.0, result: None, error: None },
    );
    let st2 = st.clone();
    tokio::spawn(async move {
        let progress: Progress = {
            let st = st2.clone();
            Arc::new(move |p, stage| st.update_job(id, |j| {
                j.progress = p;
                j.stage = stage;
            }))
        };
        let outcome = run_pipeline(&st2, &req, progress).await;
        st2.update_job(id, |j| match outcome {
            Ok(v) => {
                j.state = "done";
                j.progress = 1.0;
                j.stage = "Prêt".into();
                j.result = Some(v);
            }
            Err(e) => {
                j.state = "error";
                j.error = Some(format!("{e:#}"));
            }
        });
    });
    Json(json!({ "id": id })).into_response()
}

async fn get_job(State(st): State<AppState>, UrlPath(id): UrlPath<u64>) -> Response {
    match st.jobs.lock().unwrap().get(&id) {
        Some(job) => Json(job.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "job inconnu"),
    }
}

async fn run_pipeline(st: &AppState, req: &JobReq, progress: Progress) -> Result<Value> {
    let id = youtube::video_id(&req.url).ok_or_else(|| anyhow!("URL YouTube invalide"))?;
    let target = req.target.as_str();
    if !translate::LANGS.iter().any(|(c, _)| *c == target) {
        bail!("langue cible non supportée : {target}");
    }
    let work = st.cache_dir.join("work");
    let cache_file = st.cache_dir.join("subs").join(format!(
        "{id}_{}_{}_{}_{}.json",
        req.mode, req.source, target, req.translator
    ));
    if !req.refresh {
        if let Ok(s) = tokio::fs::read_to_string(&cache_file).await {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                progress(1.0, "Chargé depuis le cache".into());
                return Ok(v);
            }
        }
    }
    tokio::fs::create_dir_all(&work).await?;

    progress(0.02, "Lecture des infos YouTube…".into());
    let meta = youtube::metadata(&id).await?;

    let mut origin = String::new();
    let mut source_lang = req.source.clone();
    let mut cues: Vec<Cue> = Vec::new();

    if req.mode != "whisper" {
        if let Some(track) = youtube::pick_source_track(&meta, &req.source) {
            progress(0.1, format!("Téléchargement des sous-titres YouTube ({})…", track.key));
            cues = subs::merge_short(youtube::download_track(&id, &track, &work).await?);
            source_lang = track.lang.clone();
            origin = format!("YouTube {} ({})", if track.auto { "auto" } else { "manuel" }, track.key);
        } else if req.mode == "youtube" {
            bail!("Aucun sous-titre YouTube disponible pour cette vidéo — essayez le mode Whisper");
        }
    }

    if cues.is_empty() {
        if meta.is_live.unwrap_or(false) {
            bail!("Direct en cours sans sous-titres YouTube : Whisper a besoin de la vidéo complète (réessayez à la fin du live)");
        }
        cues = whisper_pipeline(st, &id, &req.source, &work, &progress).await?;
        origin = "Whisper (local)".into();
    }
    if cues.is_empty() {
        bail!("Aucune parole détectée");
    }

    let base = |s: &str| s.split('-').next().unwrap_or(s).to_string();
    let needs_translation = base(&source_lang) != target;
    let mut translator = "aucun (même langue)".to_string();
    if needs_translation {
        match req.translator.as_str() {
            "youtube" => {
                progress(0.6, "Traduction automatique YouTube…".into());
                let track = youtube::Track { key: target.to_string(), lang: target.to_string(), auto: true };
                let manual = meta.subtitles.contains_key(target);
                if !manual && !meta.automatic_captions.contains_key(target) {
                    bail!("YouTube ne propose pas de traduction automatique vers « {target} » pour cette vidéo");
                }
                let track = youtube::Track { auto: !manual, ..track };
                let translated = subs::merge_short(youtube::download_track(&id, &track, &work).await?);
                cues = align(translated, &cues);
                translator = "YouTube".into();
            }
            engine => {
                let engine = if engine == "claude" { Engine::Claude } else { Engine::Google };
                translator = if engine == Engine::Claude { "Claude".into() } else { "Google".into() };
                let p = progress.clone();
                let sub: Progress = Arc::new(move |f, s| p(0.6 + 0.39 * f, s));
                sub(0.0, format!("Traduction via {translator}…"));
                let src = if source_lang == "auto" { "auto".to_string() } else { base(&source_lang) };
                translate::translate_cues(&mut cues, &src, target, engine, st.api_key(), sub).await?;
            }
        }
    }

    let result = json!({
        "video_id": id,
        "title": meta.title,
        "is_live": meta.is_live.unwrap_or(false),
        "source_lang": source_lang,
        "target_lang": target,
        "origin": origin,
        "translator": translator,
        "cues": cues,
    });
    if let Some(dir) = cache_file.parent() {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(&cache_file, serde_json::to_vec(&result)?).await?;
    }
    Ok(result)
}

async fn whisper_pipeline(st: &AppState, id: &str, source: &str, work: &std::path::Path, progress: &Progress) -> Result<Vec<Cue>> {
    let size = st.config.lock().unwrap().whisper_model.clone();
    let p = progress.clone();
    let model_progress: Progress = Arc::new(move |f, s| p(0.05 + 0.15 * f, s));
    let model = whisper::ensure_model(&st.cache_dir.join("models"), &size, &model_progress).await?;

    progress(0.2, "Téléchargement de l'audio…".into());
    let audio = youtube::download_audio(id, work).await?;
    progress(0.25, "Conversion audio…".into());
    let wav = whisper::to_wav(&audio).await?;
    let _ = tokio::fs::remove_file(&audio).await;

    let p = progress.clone();
    let tr_progress: Progress = Arc::new(move |f, s| p(0.25 + 0.35 * f, s));
    let cues = whisper::transcribe(&model, &wav, source, &tr_progress).await;
    let _ = tokio::fs::remove_file(&wav).await;
    Ok(subs::merge_short(cues?))
}

/// Associe à chaque cue traduite le texte original qui la recouvre dans le temps.
fn align(mut translated: Vec<Cue>, original: &[Cue]) -> Vec<Cue> {
    for t in &mut translated {
        let orig: Vec<&str> = original
            .iter()
            .filter(|o| o.start < t.end && o.end > t.start)
            .map(|o| o.orig.as_str())
            .collect();
        let text = std::mem::take(&mut t.orig);
        t.orig = orig.join(" ");
        t.text = text;
    }
    translated
}

#[derive(Deserialize)]
struct StreamReq {
    #[serde(default = "default_height")]
    q: u32,
}

fn default_height() -> u32 {
    720
}

/// Les URL googlevideo expirent au bout de ~6 h ; on les garde 1 h en mémoire.
async fn stream(State(st): State<AppState>, UrlPath(id): UrlPath<String>, Query(q): Query<StreamReq>) -> Response {
    let Some(id) = youtube::video_id(&id) else { return err(StatusCode::BAD_REQUEST, "identifiant invalide") };
    let key = format!("{id}@{}", q.q);
    if let Some((at, s)) = st.streams.lock().unwrap().get(&key) {
        if at.elapsed().as_secs() < 3600 {
            return Json(s.clone()).into_response();
        }
    }
    match youtube::streams(&id, q.q).await {
        Ok(s) => {
            st.streams.lock().unwrap().insert(key, (std::time::Instant::now(), s.clone()));
            Json(s).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct SearchReq {
    q: String,
}

async fn search(Query(q): Query<SearchReq>) -> Response {
    match youtube::search(&q.q, 15).await {
        Ok(hits) => Json(hits).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Deserialize)]
struct ExportReq {
    title: String,
    lang: String,
    /// "text", "orig" ou "dual".
    which: String,
    cues: Vec<Cue>,
}

async fn export_srt(Json(req): Json<ExportReq>) -> Response {
    let srt = match req.which.as_str() {
        "orig" => subs::to_srt(&req.cues, |c| &c.orig),
        "dual" => {
            let dual: Vec<Cue> =
                req.cues.iter().map(|c| Cue { text: format!("{}\n{}", c.text, c.orig), ..c.clone() }).collect();
            subs::to_srt(&dual, |c| &c.text)
        }
        _ => subs::to_srt(&req.cues, |c| &c.text),
    };
    let safe: String = req
        .title
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '_' })
        .take(80)
        .collect();
    let dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("{}.{}.srt", safe.trim(), req.lang));
    match tokio::fs::write(&path, srt).await {
        Ok(()) => Json(json!({ "path": path })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
