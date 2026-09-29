//! Serveur HTTP local : sert l'interface embarquée et l'API de génération de sous-titres.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
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
    /// Ancien emplacement de la clé (en clair) : lu une fois pour migration vers le Trousseau,
    /// jamais réécrit sur disque.
    #[serde(default, skip_serializing)]
    anthropic_key: Option<String>,
    #[serde(default = "default_model")]
    whisper_model: String,
    #[serde(default = "default_nmt")]
    nmt_model: String,
}

fn default_nmt() -> String {
    crate::nmt::DEFAULT_MODEL.into()
}

/// config.json ne contient plus de secret, mais on le garde lisible par l'utilisateur seul.
fn write_config(path: &std::path::Path, cfg: &Config) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Clé API Claude dans le Trousseau macOS (chiffré, lié à la session de l'utilisateur).
mod secrets {
    const SERVICE: &str = "com.younss.ytlt";
    const ACCOUNT: &str = "anthropic-api-key";

    #[cfg(target_os = "macos")]
    pub fn get() -> Option<String> {
        let bytes = security_framework::passwords::get_generic_password(SERVICE, ACCOUNT).ok()?;
        String::from_utf8(bytes).ok().filter(|k| !k.is_empty())
    }

    #[cfg(target_os = "macos")]
    pub fn set(key: Option<&str>) -> anyhow::Result<()> {
        match key {
            Some(k) => security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, k.as_bytes())?,
            None => {
                let _ = security_framework::passwords::delete_generic_password(SERVICE, ACCOUNT);
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn get() -> Option<String> {
        None
    }

    #[cfg(not(target_os = "macos"))]
    pub fn set(_: Option<&str>) -> anyhow::Result<()> {
        anyhow::bail!("stockage sécurisé indisponible sur ce système")
    }
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
    /// Sous-titres déjà prêts pendant que le travail continue (affichage progressif).
    partial: Option<Vec<Cue>>,
    partial_rev: u64,
}

/// Publie les sous-titres déjà prêts dans le job en cours.
type Publish = Arc<dyn Fn(Vec<Cue>) + Send + Sync>;

#[derive(Clone)]
struct AppState {
    jobs: Arc<Mutex<HashMap<u64, Job>>>,
    next_id: Arc<AtomicU64>,
    config: Arc<Mutex<Config>>,
    streams: Arc<Mutex<HashMap<String, (std::time::Instant, youtube::Streams)>>>,
    http: reqwest::Client,
    /// Un verrou par transcription (vidéo + mode + langue source) : un seul Whisper à la fois
    /// pour une même vidéo, les autres jobs attendent son résultat.
    transcribing: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Jobs en cours : (vidéo, clé de transcription, poignée pour l'annuler).
    running: Arc<Mutex<HashMap<u64, (String, String, tokio::task::AbortHandle)>>>,
    /// Installation des modèles au premier lancement : (en cours, progression, message).
    setup: Arc<Mutex<(bool, f32, String)>>,
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
        if config.nmt_model.is_empty() {
            config.nmt_model = default_nmt();
        }
        // Migration : une clé laissée en clair dans config.json part dans le Trousseau.
        let legacy_key = config.anthropic_key.take().filter(|k| !k.trim().is_empty());
        let migrated = legacy_key.is_none_or(|key| secrets::set(Some(&key)).is_ok());
        // Réécrit toujours le fichier : champ de clé retiré et droits 600.
        if migrated && config_path.is_file() {
            let _ = write_config(&config_path, &config);
        }
        Self {
            jobs: Default::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            config: Arc::new(Mutex::new(config)),
            streams: Default::default(),
            http: reqwest::Client::new(),
            setup: Arc::new(Mutex::new((false, 1.0, String::new()))),
            transcribing: Default::default(),
            running: Default::default(),
            cache_dir,
            config_path,
        }
    }

    fn api_key(&self) -> Option<String> {
        secrets::get().or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()))
    }

    fn update_job(&self, id: u64, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(&id) {
            f(job);
        }
    }
}

/// Télécharge ce qui manque (modèle Whisper + modèle de traduction NMT) dans le cache
/// partagé. Appelé par l'installateur (`ytlt --setup`) et au lancement de l'app.
pub async fn setup_models(progress: Progress) -> Result<()> {
    let st = AppState::new();
    run_setup(&st, progress).await
}

async fn run_setup(st: &AppState, progress: Progress) -> Result<()> {
    let models = st.cache_dir.join("models");
    let whisper_model = st.config.lock().unwrap().whisper_model.clone();
    let p = progress.clone();
    let wp: Progress = Arc::new(move |f, s| p(0.5 * f, s));
    whisper::ensure_model(&models, &whisper_model, &wp).await?;
    let p = progress.clone();
    let np: Progress = Arc::new(move |f, s| p(0.5 + 0.5 * f, s));
    let nmt_model = st.config.lock().unwrap().nmt_model.clone();
    crate::nmt::ensure_model(&models, &nmt_model, &np).await?;
    progress(1.0, "Modèles prêts".into());
    Ok(())
}

fn models_missing(st: &AppState) -> bool {
    let models = st.cache_dir.join("models");
    let whisper_model = st.config.lock().unwrap().whisper_model.clone();
    let nmt_model = st.config.lock().unwrap().nmt_model.clone();
    !whisper::model_path(&models, &whisper_model).is_file() || !crate::nmt::is_ready(&models, &nmt_model)
}

/// Seule la fenêtre de l'app peut parler au serveur : elle reçoit au lancement un jeton
/// secret, échangé contre un cookie HttpOnly / SameSite=Strict. Le contrôle de l'en-tête Host
/// bloque le « DNS rebinding » (un site web qui se ferait passer pour 127.0.0.1).
#[derive(Clone)]
struct Guard {
    token: Arc<String>,
    hosts: Arc<[String; 2]>,
}

const COOKIE: &str = "ytlt_session";

fn same_secret(a: &str, b: &str) -> bool {
    // Comparaison en temps constant.
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn guard(
    State(g): State<Guard>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let headers = req.headers();
    let host_ok = headers.get(header::HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| g.hosts.iter().any(|a| a == h));
    let origin_ok = headers
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .is_none_or(|o| g.hosts.iter().any(|a| o == format!("http://{a}")));
    if !host_ok || !origin_ok {
        return (StatusCode::FORBIDDEN, "accès refusé").into_response();
    }

    // Première ouverture par l'app : « /?t=<jeton> » -> cookie de session, puis « / ».
    if let Some(t) = req.uri().query().and_then(|q| q.split('&').find_map(|p| p.strip_prefix("t="))) {
        if same_secret(t, &g.token) {
            let cookie = format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict", g.token);
            return (StatusCode::SEE_OTHER, [(header::SET_COOKIE, cookie), (header::LOCATION, "/".to_string())]).into_response();
        }
        return (StatusCode::FORBIDDEN, "jeton invalide").into_response();
    }

    let authed = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")))
        .any(|v| same_secret(v, &g.token));
    if !authed {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<h1>Accès réservé</h1><p>Ouvrez YouTube Live Translator.</p>",
        )
            .into_response();
    }

    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        if let Ok(v) = header::HeaderValue::from_str(value) {
            h.insert(*name, v);
        }
    }
    resp
}

/// La page ne peut charger que ses propres fichiers, le lecteur/flux YouTube, hls.js et les polices.
const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "content-security-policy",
        "default-src 'self'; \
         script-src 'self' https://www.youtube.com https://s.ytimg.com https://cdn.jsdelivr.net; \
         style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
         font-src https://fonts.gstatic.com; \
         img-src 'self' data: https://i.ytimg.com; \
         media-src 'self' blob: https://*.googlevideo.com; \
         connect-src 'self' https://*.googlevideo.com; \
         frame-src https://www.youtube.com https://www.youtube-nocookie.com; \
         worker-src blob:; frame-ancestors 'none'; object-src 'none'; base-uri 'none'; form-action 'self'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
    ("cross-origin-opener-policy", "same-origin"),
];

pub fn router(token: String, port: u16) -> Router {
    let g = Guard {
        token: Arc::new(token),
        hosts: Arc::new([format!("127.0.0.1:{port}"), format!("localhost:{port}")]),
    };
    let state = AppState::new();
    // Fichiers temporaires d'une session précédente (audio de repli, sous-titres bruts).
    let _ = std::fs::remove_dir_all(state.cache_dir.join("work"));
    if models_missing(&state) {
        let st = state.clone();
        *st.setup.lock().unwrap() = (true, 0.0, "Installation des modèles…".into());
        tokio::spawn(async move {
            let s2 = st.clone();
            let progress: Progress = Arc::new(move |f, msg| *s2.setup.lock().unwrap() = (true, f, msg));
            let result = run_setup(&st, progress).await;
            *st.setup.lock().unwrap() = match result {
                Ok(()) => (false, 1.0, "Modèles prêts".into()),
                Err(e) => (false, 0.0, format!("Échec de l'installation des modèles : {e:#}")),
            };
        });
    }
    Router::new()
        .route("/api/status", get(status))
        .route("/api/settings", post(save_settings))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/search", get(search))
        .route("/api/mix/{id}", get(mix))
        .route("/api/stream/{id}", get(stream))
        .route("/api/media/{key}/{kind}", get(media))
        .route("/api/export", post(export_srt))
        .route("/api/log", post(client_log))
        .route("/api/translate", post(translate_text))
        .fallback(static_file)
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(g, guard))
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
    let setup = st.setup.lock().unwrap().clone();
    let models = st.cache_dir.join("models");
    Json(json!({
        "ytdlp": youtube::find_bin("yt-dlp").is_some(),
        "whisper": true,
        "claude_key": st.api_key().is_some(),
        "whisper_model": cfg.whisper_model,
        "whisper_model_ready": whisper::model_path(&models, &cfg.whisper_model).is_file(),
        "nmt_ready": crate::nmt::is_ready(&models, &cfg.nmt_model),
        "nmt_model": cfg.nmt_model,
        "setup": { "running": setup.0, "progress": setup.1, "message": setup.2 },
        "langs": translate::LANGS.iter().map(|(c, n)| json!({"code": c, "name": n})).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct SettingsReq {
    anthropic_key: Option<String>,
    whisper_model: Option<String>,
    nmt_model: Option<String>,
}

async fn save_settings(State(st): State<AppState>, Json(req): Json<SettingsReq>) -> Response {
    let cfg = {
        let mut cfg = st.config.lock().unwrap();
        if let Some(k) = req.anthropic_key {
            let k = k.trim();
            if let Err(e) = secrets::set((!k.is_empty()).then_some(k)) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Trousseau : {e}"));
            }
        }
        if let Some(m) = req.nmt_model.filter(|m| crate::nmt::MODELS.iter().any(|(n, _, _)| n == m)) {
            cfg.nmt_model = m;
        }
        if let Some(m) = req.whisper_model.filter(|m| ["tiny", "base", "small", "medium", "large-v3-turbo-q5_0"].contains(&m.as_str())) {
            cfg.whisper_model = m;
        }
        cfg.clone()
    };
    match write_config(&st.config_path, &cfg) {
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
    /// Qui parle : "auto", "female" ou "male" (accords grammaticaux de la traduction).
    #[serde(default = "default_voice")]
    voice: String,
    /// À qui / de qui on parle, mêmes valeurs.
    #[serde(default = "default_voice")]
    addressee: String,
}

fn default_voice() -> String {
    "auto".into()
}

async fn create_job(State(st): State<AppState>, Json(req): Json<JobReq>) -> Response {
    if youtube::video_id(&req.url).is_none() {
        return err(StatusCode::BAD_REQUEST, "URL YouTube invalide");
    }
    let video = youtube::video_id(&req.url).unwrap_or_default();
    let tkey = transcript_key(&video, &req);
    // Un job pour une autre vidéo (ou une autre source) rend les précédents inutiles :
    // on les annule, ce qui arrête aussi leur whisper-cli. Même transcription : on la garde,
    // le nouveau job attendra son résultat au lieu d'en relancer une.
    for (jid, (v, k, handle)) in st.running.lock().unwrap().iter() {
        if *v != video || *k != tkey {
            handle.abort();
            st.update_job(*jid, |j| {
                j.state = "error";
                j.error = Some("annulé (nouvelle demande)".into());
            });
        }
    }
    let id = st.next_id.fetch_add(1, Ordering::Relaxed);
    st.jobs.lock().unwrap().insert(
        id,
        Job {
            state: "running",
            stage: "Démarrage…".into(),
            progress: 0.0,
            result: None,
            error: None,
            partial: None,
            partial_rev: 0,
        },
    );
    let st2 = st.clone();
    let task = tokio::spawn(async move {
        let progress: Progress = {
            let st = st2.clone();
            Arc::new(move |p, stage| st.update_job(id, |j| {
                j.progress = p;
                j.stage = stage;
            }))
        };
        let publish: Publish = {
            let st = st2.clone();
            Arc::new(move |cues| st.update_job(id, |j| {
                if j.state == "running" {
                    j.partial = Some(cues);
                    j.partial_rev += 1;
                }
            }))
        };
        let outcome = run_pipeline(&st2, &req, progress, publish).await;
        st2.update_job(id, |j| match outcome {
            Ok(v) => {
                j.state = "done";
                j.progress = 1.0;
                j.stage = "Prêt".into();
                j.result = Some(v);
                j.partial = None;
            }
            Err(e) => {
                j.state = "error";
                j.error = Some(format!("{e:#}"));
            }
        });
        st2.running.lock().unwrap().remove(&id);
    });
    let mut running = st.running.lock().unwrap();
    running.retain(|_, (_, _, h)| !h.is_finished());
    if !task.is_finished() {
        running.insert(id, (video, tkey, task.abort_handle()));
    }
    Json(json!({ "id": id })).into_response()
}

async fn get_job(State(st): State<AppState>, UrlPath(id): UrlPath<u64>) -> Response {
    match st.jobs.lock().unwrap().get(&id) {
        Some(job) => Json(job.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "job inconnu"),
    }
}

async fn run_pipeline(st: &AppState, req: &JobReq, progress: Progress, publish: Publish) -> Result<Value> {
    let id = youtube::video_id(&req.url).ok_or_else(|| anyhow!("URL YouTube invalide"))?;
    let target = req.target.as_str();
    if !translate::LANGS.iter().any(|(c, _)| *c == target) {
        bail!("langue cible non supportée : {target}");
    }
    let work = st.cache_dir.join("work");
    let cache_file = st.cache_dir.join("subs").join(format!(
        "{id}_{}_{}_{}_{}_{}_{}{}_v{CACHE_VERSION}.json",
        req.mode, req.source, target, req.translator, req.voice, req.addressee,
        if req.translator == "local" { format!("_{}", st.config.lock().unwrap().nmt_model) } else { String::new() }
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

    // La transcription ne dépend pas de la langue cible : on la garde à part pour que changer
    // de langue (arabe → français…) ne relance ni YouTube ni Whisper, seulement la traduction.
    let tkey = transcript_key(&id, req);
    let transcript_file = st.cache_dir.join("transcripts").join(format!("{tkey}.json"));
    let read_cached = || async {
        tokio::fs::read_to_string(&transcript_file).await.ok().and_then(|s| serde_json::from_str::<Transcript>(&s).ok())
    };
    let lock = st.transcribing.lock().unwrap().entry(tkey.clone()).or_default().clone();
    if lock.try_lock().is_err() {
        progress(0.05, "Transcription déjà en cours pour cette vidéo — en attente…".into());
    }
    let _transcribing = lock.lock().await;
    // Relu après l'attente : un autre job vient peut-être de terminer cette transcription.
    let cached: Option<Transcript> = if req.refresh { None } else { read_cached().await };
    let mut meta: Option<youtube::Meta> = None;
    let transcript = match cached {
        Some(t) => {
            progress(0.55, "Transcription déjà faite — traduction seule…".into());
            t
        }
        None => {
            // Avec le NMT local, chaque segment Whisper est traduit et affiché dès qu'il sort.
            let models = st.cache_dir.join("models");
            let nmt_model = st.config.lock().unwrap().nmt_model.clone();
            let live = (req.translator == "local" && crate::nmt::is_ready(&models, &nmt_model))
                .then(|| start_live_nmt(crate::nmt::model_dir(&models, &nmt_model), &req.source, target, publish.clone()));
            let result = transcribe_video(st, &id, req, &work, &progress, live.as_ref().map(|(l, _)| l)).await;
            if let Some((_, task)) = live {
                task.abort();
            }
            let (t, m) = result?;
            if let Some(dir) = transcript_file.parent() {
                tokio::fs::create_dir_all(dir).await?;
                tokio::fs::write(&transcript_file, serde_json::to_vec(&t)?).await?;
            }
            meta = Some(m);
            t
        }
    };
    let Transcript { title, is_live, source_lang, origin, mut cues } = transcript;

    let base = |s: &str| s.split('-').next().unwrap_or(s).to_string();
    let needs_translation = base(&source_lang) != target;
    let mut translator = "aucun (même langue)".to_string();
    if needs_translation {
        match req.translator.as_str() {
            "youtube" => {
                progress(0.6, "Traduction automatique YouTube…".into());
                let meta = match meta.take() {
                    Some(m) => m,
                    None => youtube::metadata(&id).await?,
                };
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
            "local" => {
                let p = progress.clone();
                let dl: Progress = Arc::new(move |f, s| p(0.6 + 0.1 * f, s));
                let nmt_model = st.config.lock().unwrap().nmt_model.clone();
                let dir = crate::nmt::ensure_model(&st.cache_dir.join("models"), &nmt_model, &dl).await?;
                let p = progress.clone();
                let sub: Progress = Arc::new(move |f, s| p(0.7 + 0.29 * f, s));
                // NLLB traduit phrase par phrase : on lui donne des phrases entières, pas des
                // morceaux de sous-titre, puis on répartit la traduction sur les cues.
                let groups = subs::translation_groups(&cues);
                let lines: Vec<String> = groups.iter().map(|g| subs::group_text(&cues[g.clone()])).collect();
                let (src, tgt) = (base(&source_lang), target.to_string());
                let snapshot = cues.clone();
                let publish = publish.clone();
                let groups2 = groups.clone();
                let out = tokio::task::spawn_blocking(move || {
                    // Chaque lot traduit est publié tout de suite : l'affichage commence en ~1 s.
                    let on_chunk = |done: &[String]| {
                        let mut ready = snapshot.clone();
                        for (g, t) in groups2.iter().zip(done) {
                            subs::distribute(t, &mut ready[g.clone()]);
                        }
                        let translated = done.len().checked_sub(1).and_then(|i| groups2.get(i)).map_or(0, |g| g.end);
                        ready.truncate(translated);
                        publish(ready);
                    };
                    crate::nmt::translate_blocking(&dir, &lines, &src, &tgt, &sub, &on_chunk)
                })
                .await??;
                for (g, t) in groups.iter().zip(&out) {
                    subs::distribute(t, &mut cues[g.clone()]);
                }
                translator = format!("NMT local (NLLB {nmt_model})");
            }
            engine => {
                let engine = if engine == "claude" { Engine::Claude } else { Engine::Google };
                let label = if engine == Engine::Claude { "Claude" } else { "Google" };
                let p = progress.clone();
                let sub: Progress = Arc::new(move |f, s| p(0.6 + 0.39 * f, s));
                sub(0.0, format!("Traduction via {label}…"));
                let src = if source_lang == "auto" { "auto".to_string() } else { base(&source_lang) };
                let context = translate::TranslationContext {
                    title: title.clone(),
                    voice: req.voice.clone(),
                    addressee: req.addressee.clone(),
                };
                translator = translate::translate_cues(&mut cues, &src, target, engine, st.api_key(), context, sub).await?;
            }
        }
    }

    let result = json!({
        "video_id": id,
        "title": title,
        "is_live": is_live,
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

/// À incrémenter quand le découpage des cues change : les caches écrits avant sont ignorés.
const CACHE_VERSION: u32 = 2;

fn transcript_key(id: &str, req: &JobReq) -> String {
    format!("{id}_{}_{}_v{CACHE_VERSION}", req.mode, req.source)
}

#[derive(Serialize, Deserialize)]
struct Transcript {
    title: String,
    is_live: bool,
    source_lang: String,
    origin: String,
    cues: Vec<Cue>,
}

/// Obtient le texte original : sous-titres YouTube si possible, sinon Whisper.
async fn transcribe_video(
    st: &AppState,
    id: &str,
    req: &JobReq,
    work: &std::path::Path,
    progress: &Progress,
    live: Option<&whisper::Live>,
) -> Result<(Transcript, youtube::Meta)> {
    progress(0.02, "Lecture des infos YouTube…".into());
    let meta = youtube::metadata(id).await?;
    let mut origin = String::new();
    let mut source_lang = req.source.clone();
    let mut cues: Vec<Cue> = Vec::new();

    if req.mode != "whisper" {
        if let Some(track) = youtube::pick_source_track(&meta, &req.source) {
            progress(0.1, format!("Téléchargement des sous-titres YouTube ({})…", track.key));
            cues = subs::merge_short(youtube::download_track(id, &track, work).await?);
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
        let (whisper_cues, detected) = whisper_pipeline(st, id, &req.source, work, progress, live, meta.duration).await?;
        cues = whisper_cues;
        if source_lang == "auto" {
            if let Some(lang) = detected {
                source_lang = lang;
            }
        }
        origin = "Whisper (local)".into();
    }
    if cues.is_empty() {
        bail!("Aucune parole détectée");
    }
    let transcript = Transcript {
        title: meta.title.clone(),
        is_live: meta.is_live.unwrap_or(false),
        source_lang,
        origin,
        cues,
    };
    Ok((transcript, meta))
}

async fn whisper_pipeline(
    st: &AppState,
    id: &str,
    source: &str,
    work: &std::path::Path,
    progress: &Progress,
    live: Option<&whisper::Live>,
    duration: Option<f64>,
) -> Result<(Vec<Cue>, Option<String>)> {
    let size = st.config.lock().unwrap().whisper_model.clone();
    let p = progress.clone();
    let model_progress: Progress = Arc::new(move |f, s| p(0.05 + 0.15 * f, s));
    let model = whisper::ensure_model(&st.cache_dir.join("models"), &size, &model_progress).await?;

    // L'audio est lu en flux, décodé en Rust et transcrit morceau par morceau pendant qu'il arrive.
    progress(0.2, "Connexion au flux audio…".into());
    let stream = Arc::new(whisper::PcmStream::default());
    let feeder = start_audio_feed(id.to_string(), work.to_path_buf(), stream.clone(), st.http.clone());

    let p = progress.clone();
    let tr_progress: Progress = Arc::new(move |f, s| p(0.2 + 0.4 * f, s));
    let (source, live, s2) = (source.to_string(), live.cloned(), stream.clone());
    let result = tokio::task::spawn_blocking(move || {
        whisper::transcribe_stream(&model, &s2, &source, duration, &tr_progress, live.as_ref())
    })
    .await;
    feeder.abort();
    let (cues, lang) = result??;
    Ok((subs::merge_short(cues), lang))
}

/// Alimente `stream` en PCM 16 kHz, entièrement en Rust (sans ffmpeg) : les segments HLS audio
/// de YouTube sont téléchargés et décodés au fil de l'eau. Si le flux direct échoue, on retombe
/// sur le téléchargement complet de l'audio par yt-dlp, décodé ensuite de la même manière.
fn start_audio_feed(id: String, work: std::path::PathBuf, stream: Arc<whisper::PcmStream>, http: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let direct = async {
            let url = youtube::audio_stream_url(&id).await?;
            crate::audio::stream_hls(&http, &url, |pcm| stream.push(pcm)).await
        };
        match direct.await {
            Ok(()) => stream.finish(None),
            Err(_) if stream.len() == 0 => {
                let fallback = async {
                    let path = youtube::download_audio(&id, &work).await?;
                    let pcm = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || crate::audio::decode_file(&path)
                    })
                    .await?;
                    let _ = tokio::fs::remove_file(&path).await;
                    pcm
                };
                match fallback.await {
                    Ok(pcm) if !pcm.is_empty() => {
                        stream.push(&pcm);
                        stream.finish(None);
                    }
                    Ok(_) => stream.finish(Some("audio vide ou format non pris en charge".into())),
                    Err(e) => stream.finish(Some(format!("{e:#}"))),
                }
            }
            Err(e) => stream.finish(Some(format!("{e:#}"))),
        }
    })
}

/// Traducteur « au fil de l'eau » : reçoit les segments de Whisper, les traduit par petits
/// lots avec le NMT local et publie la liste à jour. La langue source est fixée par
/// l'utilisateur ou annoncée par Whisper (« auto-detected language »).
fn start_live_nmt(dir: std::path::PathBuf, source: &str, target: &str, publish: Publish) -> (whisper::Live, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cue>();
    let lang = Arc::new(Mutex::new((source != "auto").then(|| source.to_string())));
    let target = target.to_string();
    let lang_task = lang.clone();
    let task = tokio::spawn(async move {
        let mut pending: Vec<Cue> = Vec::new();
        let mut shown: Vec<Cue> = Vec::new();
        while let Some(first) = rx.recv().await {
            pending.push(first);
            while let Ok(c) = rx.try_recv() {
                pending.push(c);
            }
            let Some(src) = lang_task.lock().unwrap().clone() else { continue };
            let mut batch = std::mem::take(&mut pending);
            batch.sort_by(|a, b| a.start.total_cmp(&b.start));
            if src == target {
                batch.iter_mut().for_each(|c| c.text = c.orig.clone());
            } else {
                // Phrases entières au traducteur, puis répartition sur les cues (comme la passe finale).
                let groups = subs::translation_groups(&batch);
                let lines: Vec<String> = groups.iter().map(|g| subs::group_text(&batch[g.clone()])).collect();
                let (dir, tgt) = (dir.clone(), target.clone());
                let quiet: Progress = Arc::new(|_, _| {});
                let Ok(Ok(texts)) = tokio::task::spawn_blocking(move || crate::nmt::translate_blocking(&dir, &lines, &src, &tgt, &quiet, &|_| {})).await
                else {
                    continue;
                };
                for (g, t) in groups.iter().zip(&texts) {
                    subs::distribute(t, &mut batch[g.clone()]);
                }
            }
            shown.extend(batch);
            shown.sort_by(|a, b| a.start.total_cmp(&b.start));
            publish(shown.clone());
        }
    });
    let live = whisper::Live {
        on_segment: Arc::new(move |c| {
            let _ = tx.send(c);
        }),
        on_language: Arc::new(move |l| {
            let mut guard = lang.lock().unwrap();
            if guard.is_none() {
                *guard = Some(l);
            }
        }),
    };
    (live, task)
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
    /// Le client sait lire le HLS nativement (WebKit / Safari).
    #[serde(default)]
    hls: bool,
}

fn default_height() -> u32 {
    720
}

/// Les URL googlevideo expirent au bout de ~6 h ; on les garde 1 h en mémoire.
/// Le navigateur reçoit des URL locales (`/api/media/…`) servies par [`media`].
async fn stream(State(st): State<AppState>, UrlPath(id): UrlPath<String>, Query(q): Query<StreamReq>) -> Response {
    let Some(id) = youtube::video_id(&id) else { return err(StatusCode::BAD_REQUEST, "identifiant invalide") };
    if q.hls {
        let key = format!("{id}-hls");
        let cached = st.streams.lock().unwrap().get(&key).filter(|(at, _)| at.elapsed().as_secs() < 3600).cloned();
        if let Some((_, s)) = cached {
            return Json(s).into_response();
        }
        match youtube::hls_master(&id).await {
            Ok(s) => {
                st.streams.lock().unwrap().insert(key, (std::time::Instant::now(), s.clone()));
                return Json(s).into_response();
            }
            // Pas de HLS : on retombe sur les flux séparés via le proxy.
            Err(e) => eprintln!("HLS indisponible pour {id} : {e:#}"),
        }
    }
    let key = format!("{id}-{}", q.q);
    let cached = st.streams.lock().unwrap().get(&key).filter(|(at, _)| at.elapsed().as_secs() < 3600).cloned();
    let s = match cached {
        Some((_, s)) => s,
        None => match youtube::streams(&id, q.q).await {
            Ok(s) => {
                st.streams.lock().unwrap().insert(key.clone(), (std::time::Instant::now(), s.clone()));
                s
            }
            Err(e) => return err(StatusCode::BAD_GATEWAY, e),
        },
    };
    // Les manifestes HLS (directs) sont lus tels quels : WebKit les gère nativement.
    if s.hls {
        return Json(s).into_response();
    }
    let local = youtube::Streams {
        video: format!("/api/media/{key}/v"),
        audio: s.audio.as_ref().map(|_| format!("/api/media/{key}/a")),
        ..s
    };
    Json(local).into_response()
}

/// Taille des requêtes envoyées à googlevideo : au-delà de ~10 Mo par requête,
/// YouTube bride ou coupe la connexion (c'est ce que fait aussi yt-dlp).
const CHUNK: u64 = 8 << 20;

/// Proxy HTTP avec prise en charge des requêtes `Range` pour les flux googlevideo.
/// WebKit (AVFoundation) demande de très grandes plages que YouTube coupe ;
/// on les découpe en morceaux de [`CHUNK`] octets enchaînés dans une seule réponse.
async fn media(State(st): State<AppState>, UrlPath((key, kind)): UrlPath<(String, String)>, headers: HeaderMap) -> Response {
    let upstream = {
        let map = st.streams.lock().unwrap();
        map.get(&key).and_then(|(_, s)| if kind == "a" { s.audio.clone() } else { Some(s.video.clone()) })
    };
    let Some(url) = upstream else { return err(StatusCode::NOT_FOUND, "flux expiré — rouvrez la vidéo") };

    let param = |name: &str| {
        url.split(['?', '&'])
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .map(|v| urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default())
    };
    let mime = param("mime").unwrap_or_else(|| if kind == "a" { "audio/mp4".into() } else { "video/mp4".into() });
    let total = match param("clen").and_then(|c| c.parse::<u64>().ok()) {
        Some(n) => n,
        None => match probe_length(&st.http, &url).await {
            Ok(n) => n,
            Err(e) => return err(StatusCode::BAD_GATEWAY, e),
        },
    };

    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(|r| parse_range(r, total));
    let (start, end) = range.unwrap_or((0, total.saturating_sub(1)));
    if start >= total || start > end {
        return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{total}"))]).into_response();
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    let http = st.http.clone();
    tokio::spawn(async move {
        let mut pos = start;
        while pos <= end {
            let to = (pos + CHUNK - 1).min(end);
            let resp = http.get(&url).header(header::RANGE, format!("bytes={pos}-{to}")).send().await;
            let mut resp = match resp.and_then(|r| r.error_for_status()) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(std::io::Error::other(e))).await;
                    return;
                }
            };
            loop {
                match resp.chunk().await {
                    Ok(Some(bytes)) => {
                        pos += bytes.len() as u64;
                        if tx.send(Ok(bytes)).await.is_err() {
                            return; // le lecteur a fermé la connexion (seek, pause…)
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(std::io::Error::other(e))).await;
                        return;
                    }
                }
            }
            if pos <= to {
                // Réponse plus courte que prévu : on repart de là où on en est.
                continue;
            }
        }
    });

    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    let status = if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    let mut resp = (status, body).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, mime.parse().unwrap_or(header::HeaderValue::from_static("video/mp4")));
    h.insert(header::ACCEPT_RANGES, header::HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, (end - start + 1).into());
    if range.is_some() {
        if let Ok(v) = format!("bytes {start}-{end}/{total}").parse() {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    resp
}

/// `bytes=a-b`, `bytes=a-` ou `bytes=-n` → plage inclusive bornée à la taille du fichier.
fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let last = total.checked_sub(1)?;
    match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            Some((total.saturating_sub(n), last))
        }
        (a, "") => Some((a.parse().ok()?, last)),
        (a, b) => Some((a.parse().ok()?, b.parse::<u64>().ok()?.min(last))),
    }
}

async fn probe_length(http: &reqwest::Client, url: &str) -> Result<u64> {
    let resp = http.get(url).header(header::RANGE, "bytes=0-0").send().await?.error_for_status()?;
    resp.headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| anyhow!("taille du flux inconnue"))
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
struct TranslateReq {
    lines: Vec<String>,
    source: String,
    target: String,
    /// Modèle NMT à utiliser ; par défaut celui des réglages.
    model: Option<String>,
}

/// Traduit du texte libre avec le NMT local (tests et comparaisons de modèles).
async fn translate_text(State(st): State<AppState>, Json(req): Json<TranslateReq>) -> Response {
    let name = req.model.unwrap_or_else(|| st.config.lock().unwrap().nmt_model.clone());
    let models = st.cache_dir.join("models");
    if !crate::nmt::is_ready(&models, &name) {
        return err(StatusCode::CONFLICT, format!("modèle NMT {name} non installé"));
    }
    let dir = crate::nmt::model_dir(&models, &name);
    let quiet: Progress = Arc::new(|_, _| {});
    let started = std::time::Instant::now();
    let res = tokio::task::spawn_blocking(move || {
        crate::nmt::translate_blocking(&dir, &req.lines, &req.source, &req.target, &quiet, &|_| {})
    })
    .await;
    match res {
        Ok(Ok(out)) => Json(json!({ "model": name, "ms": started.elapsed().as_millis() as u64, "lines": out })).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Les erreurs JavaScript de la webview sont renvoyées ici pour apparaître dans le terminal.
async fn client_log(body: String) -> StatusCode {
    eprintln!("[ui] {}", body.chars().take(2000).collect::<String>());
    StatusCode::NO_CONTENT
}

async fn mix(UrlPath(id): UrlPath<String>) -> Response {
    let Some(id) = youtube::video_id(&id) else { return err(StatusCode::BAD_REQUEST, "identifiant invalide") };
    match youtube::mix(&id, 10).await {
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

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-1", 100), Some((0, 1)));
        assert_eq!(parse_range("bytes=10-", 100), Some((10, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=50-500", 100), Some((50, 99)));
        assert_eq!(parse_range("items=0-1", 100), None);
    }
}
