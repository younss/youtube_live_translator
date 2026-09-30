//! Moteur de l'application, sans interface : configuration, installation des modèles,
//! transcription partagée (YouTube / Whisper), traduction et export. L'interface Slint
//! l'appelle directement (pas de serveur HTTP) et reçoit l'avancement par des rappels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::subs::{self, Cue};
use crate::translate::{self, Engine, Progress};
use crate::{whisper, youtube};

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

/// Publie les sous-titres déjà prêts pendant que le travail continue (affichage progressif).
pub type Publish = Arc<dyn Fn(Vec<Cue>) + Send + Sync>;

/// État partagé du moteur (bon marché à cloner).
#[derive(Clone)]
pub struct AppState {
    config: Arc<Mutex<Config>>,
    http: reqwest::Client,
    /// Transcriptions partagées en cours, par clé (vidéo + mode + langue source).
    transcribing: Arc<Mutex<HashMap<String, Hub>>>,
    /// Position de lecture de chaque vidéo (ms), pour transcrire d'abord la zone regardée.
    playheads: Arc<Mutex<HashMap<String, whisper::Playhead>>>,
    /// Jobs en cours : (vidéo, clé de transcription, poignée pour l'annuler).
    running: Arc<Mutex<Vec<(String, String, tokio::task::AbortHandle)>>>,
    rt: tokio::runtime::Handle,
    pub cache_dir: PathBuf,
    config_path: PathBuf,
}

/// Ce que l'interface affiche dans la barre d'état et les réglages.
#[derive(Clone, Debug)]
pub struct Status {
    pub ytdlp: bool,
    pub claude_key: bool,
    pub whisper_model: String,
    pub nmt_model: String,
    pub nmt_ready: bool,
}

impl AppState {
    pub fn new(rt: tokio::runtime::Handle) -> Self {
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
        if migrated && config_path.is_file() {
            let _ = write_config(&config_path, &config);
        }
        // Fichiers temporaires d'une session précédente.
        let _ = std::fs::remove_dir_all(cache_dir.join("work"));
        Self {
            config: Arc::new(Mutex::new(config)),
            http: reqwest::Client::new(),
            transcribing: Default::default(),
            playheads: Default::default(),
            running: Default::default(),
            rt,
            cache_dir,
            config_path,
        }
    }

    fn api_key(&self) -> Option<String> {
        secrets::get().or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()))
    }

    pub fn status(&self) -> Status {
        let cfg = self.config.lock().unwrap().clone();
        let models = self.cache_dir.join("models");
        Status {
            ytdlp: youtube::find_bin("yt-dlp").is_some(),
            claude_key: self.api_key().is_some(),
            nmt_ready: crate::nmt::is_ready(&models, &cfg.nmt_model),
            whisper_model: cfg.whisper_model,
            nmt_model: cfg.nmt_model,
        }
    }

    /// Enregistre les réglages. `api_key` : `None` = inchangée, `Some("")` = supprimée.
    pub fn save_settings(&self, api_key: Option<String>, whisper_model: String, nmt_model: String) -> Result<()> {
        if let Some(k) = api_key {
            let k = k.trim().to_string();
            secrets::set((!k.is_empty()).then_some(k.as_str())).map_err(|e| anyhow!("Trousseau : {e}"))?;
        }
        let cfg = {
            let mut cfg = self.config.lock().unwrap();
            if crate::nmt::MODELS.iter().any(|(n, _, _)| *n == nmt_model) {
                cfg.nmt_model = nmt_model;
            }
            if WHISPER_MODELS.contains(&whisper_model.as_str()) {
                cfg.whisper_model = whisper_model;
            }
            cfg.clone()
        };
        write_config(&self.config_path, &cfg)
    }

    pub fn playhead(&self, video: &str) -> whisper::Playhead {
        self.playheads
            .lock()
            .unwrap()
            .entry(video.to_string())
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)))
            .clone()
    }

    /// L'interface signale où en est la lecture : Whisper transcrit cette zone en priorité.
    pub fn set_playhead(&self, video: &str, t: f64) {
        if t.is_finite() && t >= 0.0 {
            self.playhead(video).store((t * 1000.0) as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Lance la génération des sous-titres. `publish` reçoit les sous-titres déjà prêts,
    /// `done` le résultat final. Les jobs devenus inutiles (autre vidéo, autre source)
    /// sont annulés, ainsi que leur transcription.
    pub fn start_job(
        &self,
        req: JobReq,
        progress: Progress,
        publish: Publish,
        done: impl FnOnce(std::result::Result<Value, String>) + Send + 'static,
    ) {
        let Some(video) = youtube::video_id(&req.url) else {
            done(Err("URL YouTube invalide".into()));
            return;
        };
        let tkey = transcript_key(&video, &req);
        {
            let mut running = self.running.lock().unwrap();
            running.retain(|(_, _, h)| !h.is_finished());
            for (v, k, h) in running.iter() {
                if *v != video || *k != tkey {
                    h.abort();
                }
            }
        }
        self.transcribing.lock().unwrap().retain(|k, h| {
            let keep = h.video == video && *k == tkey;
            if !keep {
                h.task.abort();
            }
            keep
        });
        let st = self.clone();
        let task = self.rt.spawn(async move {
            let outcome = run_pipeline(&st, &req, progress, publish).await;
            done(outcome.map_err(|e| format!("{e:#}")));
        });
        self.running.lock().unwrap().push((video, tkey, task.abort_handle()));
    }
}

/// Modèles Whisper proposés dans les réglages.
pub const WHISPER_MODELS: [&str; 5] = ["tiny", "base", "small", "medium", "large-v3-turbo-q5_0"];

/// Enregistre les sous-titres en .srt dans Téléchargements ; `which` : "text", "orig" ou "dual".
pub fn export_srt(title: &str, lang: &str, which: &str, cues: &[Cue]) -> Result<PathBuf> {
    let srt = match which {
        "orig" => subs::to_srt(cues, |c| &c.orig),
        "dual" => {
            let dual: Vec<Cue> = cues.iter().map(|c| Cue { text: format!("{}\n{}", c.text, c.orig), ..c.clone() }).collect();
            subs::to_srt(&dual, |c| &c.text)
        }
        _ => subs::to_srt(cues, |c| &c.text),
    };
    let safe: String = title.chars().map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '_' }).take(80).collect();
    let dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("{}.{}.srt", safe.trim(), lang));
    std::fs::write(&path, srt)?;
    Ok(path)
}

/// Télécharge ce qui manque (modèle Whisper + modèle de traduction NMT) dans le cache
/// partagé. Appelé par l'installateur (`ytlt --setup`) et au lancement de l'app.
pub async fn setup_models(progress: Progress) -> Result<()> {
    let st = AppState::new(tokio::runtime::Handle::current());
    run_setup(&st, progress).await
}

pub async fn run_setup(st: &AppState, progress: Progress) -> Result<()> {
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

pub fn models_missing(st: &AppState) -> bool {
    let models = st.cache_dir.join("models");
    let whisper_model = st.config.lock().unwrap().whisper_model.clone();
    let nmt_model = st.config.lock().unwrap().nmt_model.clone();
    !whisper::model_path(&models, &whisper_model).is_file() || !crate::nmt::is_ready(&models, &nmt_model)
}

/// Demande de sous-titres.
#[derive(Deserialize, Clone, Default)]
pub struct JobReq {
    pub url: String,
    /// "auto" ou un code langue.
    pub source: String,
    pub target: String,
    /// "auto" (YouTube puis Whisper), "youtube" ou "whisper".
    pub mode: String,
    /// "google", "claude" ou "youtube".
    pub translator: String,
    #[serde(default)]
    pub refresh: bool,
    /// Qui parle : "auto", "female" ou "male" (accords grammaticaux de la traduction).
    #[serde(default = "default_voice")]
    pub voice: String,
    /// À qui / de qui on parle, mêmes valeurs.
    #[serde(default = "default_voice")]
    pub addressee: String,
}

fn default_voice() -> String {
    "auto".into()
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
    let cached: Option<Transcript> = if req.refresh { None } else { read_cached().await };
    let mut meta: Option<youtube::Meta> = None;
    let transcript = match cached {
        Some(t) => {
            progress(0.55, "Transcription déjà faite — traduction seule…".into());
            t
        }
        None => {
            let models = st.cache_dir.join("models");
            let nmt_model = st.config.lock().unwrap().nmt_model.clone();
            let live_dir = (req.translator == "local" && crate::nmt::is_ready(&models, &nmt_model))
                .then(|| crate::nmt::model_dir(&models, &nmt_model));
            let rx = join_or_start_hub(st, &id, req, &tkey, work.clone());
            follow_hub(rx, &req.source, target, live_dir, st.playhead(&id), &progress, &publish).await?
        }
    };
    let Transcript { title, is_live, source_lang, origin, mut cues } = transcript;

    let base = |s: &str| youtube::normalize_lang(s);
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
                let key = youtube::youtube_key(target);
                let track = youtube::Track { key: key.to_string(), lang: target.to_string(), auto: true };
                let manual = meta.subtitles.contains_key(key);
                if !manual && !meta.automatic_captions.contains_key(key) {
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

/// Transcription partagée d'une vidéo : lancée une seule fois, suivie par tous les jobs qui en
/// ont besoin (changement de langue en cours de route, regénération…).
struct Hub {
    rx: tokio::sync::watch::Receiver<HubState>,
    task: tokio::task::AbortHandle,
    video: String,
}

#[derive(Default)]
struct HubState {
    /// Segments bruts dans l'ordre d'arrivée (pas forcément chronologique : on commence par
    /// la zone regardée). Liste en ajout seul : un index suffit pour suivre ce qui est nouveau.
    cues: Vec<Cue>,
    lang: Option<String>,
    progress: f32,
    stage: String,
    done: Option<std::result::Result<Arc<Transcript>, String>>,
}

/// Rejoint la transcription en cours pour cette clé, ou en lance une.
fn join_or_start_hub(st: &AppState, id: &str, req: &JobReq, tkey: &str, work: std::path::PathBuf) -> tokio::sync::watch::Receiver<HubState> {
    let mut hubs = st.transcribing.lock().unwrap();
    if let Some(h) = hubs.get(tkey) {
        if !h.task.is_finished() && (!req.refresh || h.rx.borrow().done.is_none()) {
            return h.rx.clone();
        }
    }
    let (tx, rx) = tokio::sync::watch::channel(HubState::default());
    let tx = Arc::new(tx);
    let (st2, id2, req2, tkey2) = (st.clone(), id.to_string(), req.clone(), tkey.to_string());
    let task = tokio::spawn(async move {
        let live = {
            let (t1, t2) = (tx.clone(), tx.clone());
            whisper::Live {
                on_segment: Arc::new(move |c| t1.send_modify(|s| s.cues.push(c))),
                on_language: Arc::new(move |l| {
                    t2.send_modify(|s| {
                        if s.lang.is_none() {
                            s.lang = Some(l);
                        }
                    })
                }),
            }
        };
        let progress: Progress = {
            let t = tx.clone();
            Arc::new(move |f, stage| t.send_modify(|s| {
                s.progress = f;
                s.stage = stage;
            }))
        };
        let result = transcribe_video(&st2, &id2, &req2, &work, &progress, Some(&live)).await;
        let done = match result {
            Ok((t, _meta)) => {
                let file = st2.cache_dir.join("transcripts").join(format!("{tkey2}.json"));
                if let Some(dir) = file.parent() {
                    let _ = tokio::fs::create_dir_all(dir).await;
                }
                let _ = tokio::fs::write(&file, serde_json::to_vec(&t).unwrap_or_default()).await;
                Ok(Arc::new(t))
            }
            Err(e) => Err(format!("{e:#}")),
        };
        tx.send_modify(|s| s.done = Some(done));
    });
    hubs.insert(tkey.to_string(), Hub { rx: rx.clone(), task: task.abort_handle(), video: id.to_string() });
    rx
}

#[derive(Clone, Serialize, Deserialize)]
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
    let (source, live, s2, head) = (source.to_string(), live.cloned(), stream.clone(), st.playhead(id));
    let result = tokio::task::spawn_blocking(move || {
        whisper::transcribe_stream(&model, &s2, &source, duration, &tr_progress, live.as_ref(), &head)
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

/// Suit une transcription partagée jusqu'à sa fin. Avec le NMT local, les segments sont traduits
/// au fil de l'eau (ceux proches de la tête de lecture d'abord) et publiés dans le job.
async fn follow_hub(
    mut rx: tokio::sync::watch::Receiver<HubState>,
    source: &str,
    target: &str,
    live_dir: Option<std::path::PathBuf>,
    playhead: whisper::Playhead,
    progress: &Progress,
    publish: &Publish,
) -> Result<Transcript> {
    const CHUNK: usize = 24;
    let mut seen = 0usize;
    let mut pending: Vec<Cue> = Vec::new();
    let mut shown: Vec<Cue> = Vec::new();
    loop {
        let (new, lang, done) = {
            let s = rx.borrow_and_update();
            progress(0.2 + 0.4 * s.progress, s.stage.clone());
            (s.cues[seen..].to_vec(), s.lang.clone(), s.done.clone())
        };
        seen += new.len();
        if let Some(done) = done {
            return done.map(|t| (*t).clone()).map_err(|e| anyhow!(e));
        }
        if let Some(dir) = &live_dir {
            pending.extend(new);
            let src = if source == "auto" { lang } else { Some(source.to_string()) };
            if let (Some(src), false) = (src, pending.is_empty()) {
                // Les segments les plus proches de ce que l'utilisateur regarde passent d'abord.
                let head = match playhead.load(std::sync::atomic::Ordering::Relaxed) {
                    u64::MAX => 0.0,
                    ms => ms as f64 / 1000.0,
                };
                pending.sort_by(|a, b| (a.start - head).abs().total_cmp(&(b.start - head).abs()));
                let take = pending.len().min(CHUNK);
                let mut batch: Vec<Cue> = pending.drain(..take).collect();
                batch.sort_by(|a, b| a.start.total_cmp(&b.start));
                let base = |s: &str| youtube::normalize_lang(s);
                if base(&src) == target {
                    batch.iter_mut().for_each(|c| c.text = c.orig.clone());
                } else {
                    let groups = subs::translation_groups(&batch);
                    let lines: Vec<String> = groups.iter().map(|g| subs::group_text(&batch[g.clone()])).collect();
                    let (dir, src, tgt) = (dir.clone(), base(&src), target.to_string());
                    let quiet: Progress = Arc::new(|_, _| {});
                    if let Ok(Ok(texts)) =
                        tokio::task::spawn_blocking(move || crate::nmt::translate_blocking(&dir, &lines, &src, &tgt, &quiet, &|_| {})).await
                    {
                        for (g, t) in groups.iter().zip(&texts) {
                            subs::distribute(t, &mut batch[g.clone()]);
                        }
                    }
                }
                shown.extend(batch);
                shown.sort_by(|a, b| a.start.total_cmp(&b.start));
                publish(shown.clone());
                if !pending.is_empty() {
                    continue; // encore du travail : on ne bloque pas en attendant Whisper
                }
            }
        }
        if rx.changed().await.is_err() {
            bail!("transcription interrompue");
        }
    }
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

