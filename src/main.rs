//! YouTube Live Translator — interface Slint, vidéo mpv, sous-titres générés et traduits en local
//! (Whisper + NLLB) dans 24 langues.
//!
//! `ytlt`          ouvre la fenêtre
//! `ytlt --setup`  télécharge les modèles (utilisé par l'installateur)

// Sous Windows : application graphique, sans fenêtre de console.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod core;
mod nmt;
mod subs;
mod translate;
mod whisper;
mod youtube;

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_void};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use libmpv2::Mpv;
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::subs::Cue;
use crate::translate::{LANGS, Progress};

slint::include_modules!();

/// Rend au système la mémoire libérée (sinon l'allocateur de macOS la garde en réserve
/// et elle reste comptée dans l'app après le déchargement d'un modèle).
pub fn release_memory() {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// Libellés affichés (langue d'origine — nom français), dans l'ordre de `LANGS`.
fn lang_label(code: &str) -> &'static str {
    match code {
        "ar" => "العربية — Arabe",
        "fr" => "Français",
        "en" => "English — Anglais",
        "de" => "Deutsch — Allemand",
        "tr" => "Türkçe — Turc",
        "es" => "Español — Espagnol",
        "pt" => "Português — Portugais",
        "it" => "Italiano — Italien",
        "ru" => "Русский — Russe",
        "pl" => "Polski — Polonais",
        "nl" => "Nederlands — Néerlandais",
        "fa" => "فارسی — Persan",
        "ur" => "اردو — Ourdou",
        "hi" => "हिन्दी — Hindi",
        "bn" => "বাংলা — Bengali",
        "ta" => "தமிழ் — Tamoul",
        "te" => "తెలుగు — Télougou",
        "zh" => "中文 — Chinois",
        "ja" => "日本語 — Japonais",
        "ko" => "한국어 — Coréen",
        "th" => "ไทย — Thaï",
        "vi" => "Tiếng Việt — Vietnamien",
        "id" => "Bahasa Indonesia — Indonésien",
        "tl" => "Filipino — Philippin",
        _ => "?",
    }
}

const MODES: [&str; 3] = ["auto", "youtube", "whisper"];
const TRANSLATORS: [&str; 4] = ["local", "google", "claude", "youtube"];
const HEIGHTS: [u32; 4] = [360, 480, 720, 1080];
const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
/// Ordre du menu « Traduction locale » dans les réglages.
const NMT_CHOICES: [&str; 2] = ["1.3b", "600m"];

fn fmt_time(t: f64) -> String {
    let t = if t.is_finite() && t > 0.0 { t as u64 } else { 0 };
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

// ------------------------------------------------------------------ préférences

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Prefs {
    src: i32,
    dst: i32,
    mode: i32,
    translator: i32,
    quality: i32,
    speed: i32,
    volume: f32,
    font_size: f32,
    sub_pos: f32,
    sub_bg: f32,
    dual: bool,
    subs_on: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { src: 0, dst: 1, mode: 0, translator: 0, quality: 2, speed: 2, volume: 80.0, font_size: 28.0, sub_pos: 8.0, sub_bg: 55.0, dual: false, subs_on: true }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    id: String,
    title: String,
}

fn data_file(name: &str) -> std::path::PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("youtube-live-translator").join(name)
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    std::fs::read_to_string(data_file(name)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_json<T: Serialize>(name: &str, value: &T) {
    let path = data_file(name);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(value) {
        let _ = std::fs::write(path, bytes);
    }
}

// ------------------------------------------------------------------ état de l'interface

#[derive(Default)]
struct App {
    video: Option<String>,
    title: String,
    cues: Vec<Cue>,
    result: Option<(String, String, String)>, // (source, cible, traducteur)
    history: Vec<String>,
    history_pos: usize,
    playlist: Vec<Entry>,
    selected: Option<usize>,
    results: Vec<youtube::SearchHit>,
    /// Indices des cues affichées dans l'onglet transcription (après filtre).
    shown: Vec<usize>,
    now: Option<usize>,
    job_token: u64,
    fetching_mix: bool,
    margins: [f64; 4],
    last_title: String,
    was_eof: bool,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

// ------------------------------------------------------------------ lecteur mpv

/// Pointeur vers la fonction de résolution OpenGL fournie par Slint. Elle n'est valable que
/// pendant l'appel « RenderingSetup » : mpv ne l'utilise que là, dans `create_render_context`,
/// pour charger ses fonctions GL une fois pour toutes.
struct GlCtx(*const dyn Fn(&CStr) -> *const c_void);

fn get_proc(ctx: &GlCtx, name: &str) -> *mut c_void {
    let name = CString::new(name).unwrap_or_default();
    unsafe { (*ctx.0)(&name) as *mut c_void }
}

fn new_mpv(prefs: &Prefs) -> Result<&'static Mpv> {
    let ytdl = youtube::find_bin("yt-dlp");
    let mpv = Mpv::with_initializer(|init| {
        init.set_property("vo", "libmpv")?;
        init.set_property("ytdl", "yes")?;
        if let Some(path) = &ytdl {
            // Les apps lancées depuis le Finder n'ont pas le PATH de Homebrew.
            init.set_property("script-opts", format!("ytdl_hook-ytdl_path={}", path.display()))?;
        }
        init.set_property("ytdl-format", ytdl_format(HEIGHTS[prefs.quality.clamp(0, 3) as usize]))?;
        init.set_property("keep-open", "yes")?;
        init.set_property("hwdec", "auto-safe")?;
        init.set_property("sid", "no")?; // nos sous-titres, pas ceux de YouTube
        init.set_property("osd-level", 0i64)?;
        init.set_property("input-default-bindings", "no")?;
        init.set_property("volume", prefs.volume as f64)?;
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("mpv : {e}"))?;
    Ok(Box::leak(Box::new(mpv)))
}

fn ytdl_format(h: u32) -> String {
    format!("bv*[height<={h}]+ba/b[height<={h}]/b")
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// Branche le rendu vidéo de mpv sous l'interface Slint et place l'image sur la zone « écran ».
fn attach_renderer(ui: &AppWindow, mpv: &'static Mpv) -> Result<()> {
    let render: Rc<RefCell<Option<RenderContext<'static>>>> = Rc::new(RefCell::new(None));
    let weak = ui.as_weak();
    ui.window()
        .set_rendering_notifier(move |state, api| match (state, api) {
            (slint::RenderingState::RenderingSetup, slint::GraphicsAPI::NativeOpenGL { get_proc_address }) => {
                let short: *const (dyn Fn(&CStr) -> *const c_void + '_) = *get_proc_address;
                // SAFETY : utilisé seulement pendant create_render_context ci-dessous (voir GlCtx).
                let ctx = GlCtx(unsafe { std::mem::transmute::<*const (dyn Fn(&CStr) -> *const c_void + '_), *const (dyn Fn(&CStr) -> *const c_void + 'static)>(short) });
                match mpv.create_render_context(vec![
                    RenderParam::ApiType(RenderParamApiType::OpenGl),
                    RenderParam::InitParams(OpenGLInitParams { get_proc_address: get_proc, ctx }),
                ]) {
                    Ok(mut rc) => {
                        let w = weak.clone();
                        rc.set_update_callback(move || {
                            let w = w.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() {
                                    ui.window().request_redraw();
                                }
                            });
                        });
                        *render.borrow_mut() = Some(rc);
                    }
                    Err(e) => eprintln!("rendu mpv indisponible : {e}"),
                }
            }
            (slint::RenderingState::BeforeRendering, _) => {
                let (Some(rc), Some(ui)) = (render.borrow().as_ref().map(|_| ()), weak.upgrade()) else { return };
                let _ = rc;
                place_video(&ui, mpv);
                let size = ui.window().size();
                if let Some(rc) = render.borrow().as_ref() {
                    let _ = rc.render::<GlCtx>(0, size.width as i32, size.height as i32, true);
                }
            }
            (slint::RenderingState::RenderingTeardown, _) => {
                render.borrow_mut().take();
            }
            _ => {}
        })
        .map_err(|e| anyhow::anyhow!("rendu OpenGL : {e:?}"))
}

/// mpv dessine sur toute la fenêtre : on lui réserve des marges pour que l'image tombe
/// exactement sur la zone « écran » de l'interface (pas sous les panneaux).
fn place_video(ui: &AppWindow, mpv: &Mpv) {
    let scale = ui.window().scale_factor() as f64;
    let size = ui.window().size();
    let (w, h) = (size.width as f64 / scale, size.height as f64 / scale);
    if w < 1.0 || h < 1.0 {
        return;
    }
    let (x, y, sw, sh) = (ui.get_stage_x() as f64, ui.get_stage_y() as f64, ui.get_stage_w() as f64, ui.get_stage_h() as f64);
    let m = [x / w, (w - x - sw) / w, y / h, (h - y - sh) / h].map(|v| v.clamp(0.0, 0.99));
    let changed = with_app(|a| {
        let changed = a.margins.iter().zip(&m).any(|(a, b)| (a - b).abs() > 0.001);
        a.margins = m;
        changed
    });
    if changed {
        for (name, v) in ["video-margin-ratio-left", "video-margin-ratio-right", "video-margin-ratio-top", "video-margin-ratio-bottom"].iter().zip(m) {
            let _ = mpv.set_property(name, v);
        }
    }
}

// ------------------------------------------------------------------ outils d'interface

fn toast(ui: &AppWindow, msg: &str) {
    ui.set_toast_text(msg.into());
    ui.set_toast_visible(true);
    let w = ui.as_weak();
    let msg = msg.to_string();
    slint::Timer::single_shot(Duration::from_millis(1500), move || {
        if let Some(ui) = w.upgrade() {
            if ui.get_toast_text() == msg.as_str() {
                ui.set_toast_visible(false);
            }
        }
    });
}

fn set_progress(ui: &AppWindow, p: f32, text: &str, kind: i32) {
    ui.set_job_progress(p.clamp(0.0, 1.0));
    ui.set_job_text(text.to_uppercase().into());
    ui.set_job_kind(kind);
}

/// Met la playlist à jour. Si le nombre d'éléments n'a pas changé, les lignes sont modifiées
/// sur place : reconstruire le modèle détruirait l'élément sous la souris et ferait perdre le clic.
fn refresh_playlist(ui: &AppWindow) {
    let items: Vec<PlItem> = with_app(|a| {
        a.playlist
            .iter()
            .enumerate()
            .map(|(i, e)| PlItem { title: e.title.clone().into(), sel: a.selected == Some(i), playing: a.video.as_deref() == Some(&e.id) })
            .collect()
    });
    let model = ui.get_playlist();
    if model.row_count() == items.len() && items.len() > 0 {
        for (i, item) in items.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&item) {
                model.set_row_data(i, item);
            }
        }
    } else {
        ui.set_playlist(ModelRc::new(VecModel::from(items)));
    }
}

fn refresh_transcript(ui: &AppWindow) {
    let filter = ui.get_filter_text().to_lowercase();
    let rows: Vec<TrItem> = with_app(|a| {
        a.shown = a
            .cues
            .iter()
            .enumerate()
            .filter(|(_, c)| filter.is_empty() || c.text.to_lowercase().contains(&filter) || c.orig.to_lowercase().contains(&filter))
            .map(|(i, _)| i)
            .collect();
        a.now = None;
        a.shown
            .iter()
            .map(|&i| {
                let c = &a.cues[i];
                TrItem { time: fmt_time(c.start).into(), text: c.text.clone().into(), orig: c.orig.clone().into(), now: false }
            })
            .collect()
    });
    ui.set_transcript(ModelRc::new(VecModel::from(rows)));
}

fn set_tags(ui: &AppWindow) {
    with_app(|a| {
        let (src, dst, eng) = a.result.clone().unwrap_or(("--".into(), "--".into(), "---".into()));
        ui.set_tag_src(format!("SRC {}", src.to_uppercase()).into());
        ui.set_tag_dst(format!("DST {}", dst.to_uppercase()).into());
        ui.set_tag_eng(eng.to_uppercase().chars().take(12).collect::<String>().into());
        ui.set_tag_cues(format!("{} CUES", a.cues.len()).into());
    });
}

fn save_prefs(ui: &AppWindow) {
    save_json(
        "prefs.json",
        &Prefs {
            src: ui.get_src_lang(),
            dst: ui.get_dst_lang(),
            mode: ui.get_mode(),
            translator: ui.get_translator(),
            quality: ui.get_quality(),
            speed: ui.get_speed(),
            volume: ui.get_volume(),
            font_size: ui.get_font_size(),
            sub_pos: ui.get_sub_pos(),
            sub_bg: ui.get_sub_bg(),
            dual: ui.get_dual(),
            subs_on: ui.get_subs_on(),
        },
    );
}

fn save_playlist() {
    with_app(|a| save_json("playlist.json", &a.playlist));
}

fn refresh_status(ui: &AppWindow, core: &core::AppState) {
    let s = core.status();
    let dot = |ok: bool, name: &str| format!("{} {name}", if ok { "●" } else { "○" });
    ui.set_deps_text(
        [dot(s.ytdlp, "yt-dlp"), dot(true, "mpv"), dot(true, "whisper"), dot(s.nmt_ready, "NMT local"), dot(s.claude_key, "clé Claude")]
            .join("   ")
            .into(),
    );
    ui.set_key_state(if s.claude_key { "Une clé est configurée (Trousseau macOS)." } else { "Aucune clé : le traducteur Claude est indisponible." }.into());
    ui.set_whisper_model(core::WHISPER_MODELS.iter().position(|m| *m == s.whisper_model).unwrap_or(4) as i32);
    ui.set_nmt_model(NMT_CHOICES.iter().position(|m| *m == s.nmt_model).unwrap_or(0) as i32);
}

// ------------------------------------------------------------------ actions

struct Ctx {
    ui: slint::Weak<AppWindow>,
    core: core::AppState,
    mpv: &'static Mpv,
    rt: tokio::runtime::Handle,
}

impl Ctx {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("fenêtre")
    }

    fn open_video(&self, id: &str, from_history: bool) {
        let ui = self.ui();
        ui.set_url_text(watch_url(id).into());
        with_app(|a| {
            if !from_history {
                a.history.truncate(a.history_pos + usize::from(!a.history.is_empty()));
                if a.history.last().map(String::as_str) != Some(id) {
                    a.history.push(id.to_string());
                }
                a.history_pos = a.history.len().saturating_sub(1);
            }
            a.video = Some(id.to_string());
            a.cues.clear();
            a.result = None;
            a.was_eof = false;
            if !a.playlist.iter().any(|e| e.id == id) {
                a.playlist.push(Entry { id: id.to_string(), title: id.to_string() });
            }
        });
        save_playlist();
        refresh_playlist(&ui);
        refresh_transcript(&ui);
        set_tags(&ui);
        ui.set_sub_main("".into());
        ui.set_sub_orig("".into());
        ui.set_has_video(true);
        let _ = self.mpv.set_property("pause", false);
        let _ = self.mpv.command("loadfile", &[&watch_url(id)]);
        // Génération automatique, sauf avec Claude (payant) où l'on attend un clic.
        if TRANSLATORS[ui.get_translator().clamp(0, 3) as usize] != "claude" {
            self.generate(false);
        } else {
            set_progress(&ui, 0.0, "Cliquez sur GÉNÉRER (Claude)", 0);
        }
    }

    fn generate(&self, refresh: bool) {
        let ui = self.ui();
        let Some(video) = with_app(|a| a.video.clone()) else {
            toast(&ui, "Ouvrez d'abord une vidéo");
            return;
        };
        let src = ui.get_src_lang();
        let req = core::JobReq {
            url: video.clone(),
            source: if src <= 0 { "auto".into() } else { LANGS[(src - 1) as usize].0.into() },
            target: LANGS[ui.get_dst_lang().clamp(0, LANGS.len() as i32 - 1) as usize].0.into(),
            mode: MODES[ui.get_mode().clamp(0, 2) as usize].into(),
            translator: TRANSLATORS[ui.get_translator().clamp(0, 3) as usize].into(),
            refresh,
            voice: "auto".into(),
            addressee: "auto".into(),
        };
        let token = with_app(|a| {
            a.job_token += 1;
            a.job_token
        });
        ui.set_generating(true);
        set_progress(&ui, 0.01, "Démarrage…", 1);

        let video: Arc<str> = video.into();
        let (w1, w2, w3) = (self.ui.clone(), self.ui.clone(), self.ui.clone());
        let progress: Progress = Arc::new(move |p, stage| {
            let w = w1.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    if with_app(|a| a.job_token == token) {
                        set_progress(&ui, p, &stage, 1);
                    }
                }
            });
        });
        let publish: core::Publish = Arc::new(move |cues| {
            let (w, video) = (w2.clone(), video.clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    if with_app(|a| {
                        let ok = a.job_token == token && a.video.as_deref() == Some(&*video);
                        if ok {
                            a.cues = cues;
                        }
                        ok
                    }) {
                        refresh_transcript(&ui);
                        ui.set_tag_cues(format!("{} CUES…", with_app(|a| a.cues.len())).into());
                    }
                }
            });
        });
        self.core.start_job(req, progress, publish, move |outcome| {
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = w3.upgrade() else { return };
                if !with_app(|a| a.job_token == token) {
                    return;
                }
                ui.set_generating(false);
                match outcome {
                    Ok(v) => {
                        let cues: Vec<Cue> = serde_json::from_value(v["cues"].clone()).unwrap_or_default();
                        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
                        let summary = format!("{} sous-titres · {} → {}", cues.len(), s("origin"), s("translator"));
                        with_app(|a| {
                            a.cues = cues;
                            a.result = Some((s("source_lang"), s("target_lang"), s("translator")));
                            if a.title.is_empty() || a.title == a.video.clone().unwrap_or_default() {
                                a.title = s("title");
                            }
                        });
                        refresh_transcript(&ui);
                        set_tags(&ui);
                        set_progress(&ui, 1.0, &summary, 0);
                        ui.set_status_text(s("title").into());
                        toast(&ui, "Sous-titres prêts");
                    }
                    Err(e) => set_progress(&ui, 0.0, &format!("Erreur : {e}"), 2),
                }
            });
        });
    }

    fn play_offset(&self, dir: i32, auto: bool) {
        let ui = self.ui();
        let next = with_app(|a| {
            let i = a.video.as_ref().and_then(|v| a.playlist.iter().position(|e| &e.id == v))?;
            let j = i as i64 + dir as i64;
            (j >= 0).then(|| a.playlist.get(j as usize).map(|e| e.id.clone())).flatten()
        });
        if let Some(id) = next {
            self.open_video(&id, false);
            return;
        }
        if dir < 0 {
            toast(&ui, "Début de la playlist");
            return;
        }
        let Some(video) = with_app(|a| (!a.fetching_mix).then(|| a.video.clone()).flatten()) else { return };
        // Fin de la playlist : on enchaîne sur le Mix YouTube de la vidéo en cours.
        with_app(|a| a.fetching_mix = true);
        if !auto {
            toast(&ui, "Recherche des vidéos suivantes (Mix YouTube)…");
        }
        let w = self.ui.clone();
        self.rt.spawn(async move {
            let hits = youtube::mix(&video, 10).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = w.upgrade() else { return };
                with_app(|a| a.fetching_mix = false);
                match hits {
                    Ok(hits) => {
                        let first = with_app(|a| {
                            let fresh: Vec<_> = hits.into_iter().filter(|h| !a.playlist.iter().any(|e| e.id == h.id)).collect();
                            let first = fresh.first().map(|h| h.id.clone());
                            a.playlist.extend(fresh.into_iter().map(|h| Entry { id: h.id, title: h.title }));
                            first
                        });
                        save_playlist();
                        refresh_playlist(&ui);
                        match first {
                            Some(id) => CTX.with(|c| c.borrow().as_ref().map(|c| c.open_video(&id, false))).unwrap_or(()),
                            None => toast(&ui, "Pas de vidéo suivante trouvée"),
                        }
                    }
                    Err(e) => ui.set_status_text(format!("Mix YouTube : {e:#}").into()),
                }
            });
        });
    }

    fn search(&self, query: String) {
        let ui = self.ui();
        ui.set_tab(2);
        ui.set_results(ModelRc::new(VecModel::from(Vec::<ResItem>::new())));
        ui.set_results_empty("Recherche…".into());
        let w = self.ui.clone();
        self.rt.spawn(async move {
            let hits = youtube::search(&query, 15).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = w.upgrade() else { return };
                match hits {
                    Ok(hits) => {
                        let rows: Vec<ResItem> = hits
                            .iter()
                            .map(|h| ResItem {
                                title: h.title.clone().into(),
                                channel: h.channel.clone().into(),
                                duration: h.duration.map(fmt_time).unwrap_or_else(|| "LIVE".into()).into(),
                            })
                            .collect();
                        ui.set_results_empty(if rows.is_empty() { "Aucun résultat." } else { "" }.into());
                        ui.set_results(ModelRc::new(VecModel::from(rows)));
                        with_app(|a| a.results = hits);
                    }
                    Err(e) => ui.set_results_empty(format!("Erreur : {e:#}").into()),
                }
            });
        });
    }
}

thread_local! {
    static CTX: RefCell<Option<Rc<Ctx>>> = const { RefCell::new(None) };
}

// ------------------------------------------------------------------ boucle de lecture

/// Toutes les 100 ms : temps, barre de progression, sous-titre courant, tête de lecture
/// (pour Whisper), fin de vidéo.
fn tick(ctx: &Ctx) {
    let ui = ctx.ui();
    let mpv = ctx.mpv;
    let t: f64 = mpv.get_property("time-pos").unwrap_or(0.0);
    let d: f64 = mpv.get_property("duration").unwrap_or(0.0);
    let paused: bool = mpv.get_property("pause").unwrap_or(true);
    let eof: bool = mpv.get_property("eof-reached").unwrap_or(false);
    let has_video = with_app(|a| a.video.is_some());
    ui.set_time_text(fmt_time(t).into());
    ui.set_dur_text(fmt_time(d).into());
    ui.set_position(if d > 0.0 { (t / d) as f32 } else { 0.0 });
    ui.set_playing(has_video && !paused && !eof);

    // Titre (fourni par yt-dlp via mpv).
    if let Ok(title) = mpv.get_property::<String>("media-title") {
        let changed = with_app(|a| {
            if a.video.is_some() && title != a.last_title && !title.starts_with("watch?v=") && !title.is_empty() {
                a.last_title = title.clone();
                a.title = title.clone();
                if let Some(v) = a.video.clone() {
                    if let Some(e) = a.playlist.iter_mut().find(|e| e.id == v) {
                        e.title = title.clone();
                    }
                }
                true
            } else {
                false
            }
        });
        if changed {
            ui.set_marquee(format!("*** {} ***", title.to_uppercase()).into());
            save_playlist();
            refresh_playlist(&ui);
        }
    }

    // Whisper transcrit en priorité la zone regardée.
    if let Some(v) = with_app(|a| a.video.clone()) {
        ctx.core.set_playhead(&v, t);
    }

    // Sous-titre courant (recherche dichotomique ; les cues sont triées).
    let at = t - ui.get_offset() as f64 / 10.0;
    let (main, orig, idx) = with_app(|a| {
        let i = a.cues.partition_point(|c| c.start <= at);
        match i.checked_sub(1).map(|i| (i, &a.cues[i])) {
            Some((i, c)) if at <= c.end + 0.35 => (c.text.clone(), if c.orig != c.text { c.orig.clone() } else { String::new() }, Some(i)),
            _ => (String::new(), String::new(), None),
        }
    });
    if ui.get_sub_main().as_str() != main {
        ui.set_sub_main(main.into());
    }
    if ui.get_sub_orig().as_str() != orig {
        ui.set_sub_orig(orig.into());
    }
    // Ligne courante de la transcription.
    let (old, new) = with_app(|a| {
        let new = idx.and_then(|i| a.shown.iter().position(|&s| s == i));
        let old = std::mem::replace(&mut a.now, new);
        (old, new)
    });
    if old != new {
        let model = ui.get_transcript();
        for (row, flag) in [(old, false), (new, true)] {
            if let Some(r) = row {
                if let Some(mut item) = model.row_data(r) {
                    item.now = flag;
                    model.set_row_data(r, item);
                }
            }
        }
    }

    // Fin de vidéo : boucle ou élément suivant.
    let fire = with_app(|a| {
        let fire = eof && !a.was_eof && a.video.is_some();
        a.was_eof = eof;
        fire
    });
    if fire {
        if ui.get_loop() {
            let _ = mpv.command("seek", &["0", "absolute"]);
            let _ = mpv.set_property("pause", false);
        } else {
            ctx.play_offset(1, true);
        }
    }
}

/// Analyseur de spectre décoratif (l'app ne lit pas le signal audio de mpv).
fn animate_bars(ui: &AppWindow, phase: &mut f32) {
    let playing = ui.get_playing();
    let model = ui.get_bars();
    if !playing && (0..model.row_count()).all(|i| model.row_data(i).unwrap_or(0.0) < 0.01) {
        return;
    }
    *phase += 0.05;
    let bars: Vec<f32> = (0..19)
        .map(|i| {
            let old = model.row_data(i).unwrap_or(0.0);
            let fi = i as f32;
            let target = if playing {
                (0.25 + 0.75 * ((*phase * (1.3 + fi * 0.37) + fi).sin() * (*phase * 0.7 + fi * 1.7).sin()).abs()) * (1.0 - fi / 40.0)
            } else {
                0.0
            };
            old + (target - old) * if target > old { 0.5 } else { 0.12 }
        })
        .collect();
    ui.set_bars(ModelRc::new(VecModel::from(bars)));
}

// ------------------------------------------------------------------ démarrage

fn run_setup_cli() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let last = std::sync::Mutex::new(String::new());
    let progress: Progress = Arc::new(move |f, msg: String| {
        let line = format!("[{:>3.0}%] {msg}", f * 100.0);
        let mut last = last.lock().unwrap();
        if *last != line {
            println!("{line}");
            *last = line;
        }
    });
    rt.block_on(core::setup_models(progress))?;
    println!("Modèles installés.");
    Ok(())
}

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--setup") {
        return run_setup_cli();
    }

    let rt = tokio::runtime::Runtime::new()?;
    let core = core::AppState::new(rt.handle().clone());
    slint::BackendSelector::new().require_opengl().select().map_err(|e| anyhow::anyhow!("OpenGL : {e}"))?;
    let ui = AppWindow::new()?;
    // Polices présentes sur chaque système (Menlo / Helvetica n'existent pas sous Windows).
    let theme = ui.global::<Theme>();
    if cfg!(windows) {
        theme.set_mono("Consolas".into());
        theme.set_ui_font("Segoe UI".into());
    }

    // Préférences et playlist enregistrées.
    let prefs: Prefs = load_json("prefs.json");
    let mut sources = vec![SharedString::from("Auto (langue de la vidéo)")];
    sources.extend(LANGS.iter().map(|(c, _)| SharedString::from(lang_label(c))));
    let targets: Vec<SharedString> = LANGS.iter().map(|(c, _)| SharedString::from(lang_label(c))).collect();
    ui.set_languages(ModelRc::new(VecModel::from(sources)));
    ui.set_target_languages(ModelRc::new(VecModel::from(targets)));
    ui.set_src_lang(prefs.src.clamp(0, LANGS.len() as i32));
    ui.set_dst_lang(prefs.dst.clamp(0, LANGS.len() as i32 - 1));
    ui.set_mode(prefs.mode.clamp(0, 2));
    ui.set_translator(prefs.translator.clamp(0, 3));
    ui.set_quality(prefs.quality.clamp(0, 3));
    ui.set_speed(prefs.speed.clamp(0, 5));
    ui.set_volume(prefs.volume.clamp(0.0, 100.0));
    ui.set_font_size(prefs.font_size);
    ui.set_sub_pos(prefs.sub_pos);
    ui.set_sub_bg(prefs.sub_bg);
    ui.set_dual(prefs.dual);
    ui.set_subs_on(prefs.subs_on);
    with_app(|a| a.playlist = load_json("playlist.json"));
    refresh_playlist(&ui);
    refresh_transcript(&ui);
    refresh_status(&ui, &core);

    let mpv = new_mpv(&prefs)?;
    let _ = mpv.set_property("speed", SPEEDS[prefs.speed.clamp(0, 5) as usize]);
    attach_renderer(&ui, mpv)?;

    let ctx = Rc::new(Ctx { ui: ui.as_weak(), core: core.clone(), mpv, rt: rt.handle().clone() });
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));

    // Modèles manquants (installation depuis le DMG) : téléchargés au premier lancement.
    if core::models_missing(&core) {
        set_progress(&ui, 0.0, "Installation des modèles…", 1);
        let (w, st) = (ui.as_weak(), core.clone());
        rt.spawn(async move {
            let w2 = w.clone();
            let progress: Progress = Arc::new(move |f, msg| {
                let w = w2.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        set_progress(&ui, f, &msg, 1);
                    }
                });
            });
            let result = core::run_setup(&st, progress).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    match result {
                        Ok(()) => set_progress(&ui, 1.0, "Modèles prêts", 0),
                        Err(e) => set_progress(&ui, 0.0, &format!("Échec de l'installation des modèles : {e:#}"), 2),
                    }
                    CTX.with(|c| c.borrow().as_ref().map(|c| refresh_status(&ui, &c.core)));
                }
            });
        });
    }

    // --- navigation et lecture
    let c = ctx.clone();
    ui.on_open_url(move |text| {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        match youtube::video_id(&text) {
            Some(id) => c.open_video(&id, false),
            None => c.search(text),
        }
    });
    let c = ctx.clone();
    ui.on_nav_back(move || {
        if let Some(id) = with_app(|a| (a.history_pos > 0).then(|| {
            a.history_pos -= 1;
            a.history[a.history_pos].clone()
        })) {
            c.open_video(&id, true);
        }
    });
    let c = ctx.clone();
    ui.on_nav_forward(move || {
        if let Some(id) = with_app(|a| (a.history_pos + 1 < a.history.len()).then(|| {
            a.history_pos += 1;
            a.history[a.history_pos].clone()
        })) {
            c.open_video(&id, true);
        }
    });
    let c = ctx.clone();
    ui.on_reload(move || {
        if let Some(id) = with_app(|a| a.video.clone()) {
            c.open_video(&id, true);
        }
    });
    ui.on_toggle_play(move || {
        let paused: bool = mpv.get_property("pause").unwrap_or(false);
        let eof: bool = mpv.get_property("eof-reached").unwrap_or(false);
        if eof {
            // Vidéo terminée : Lecture repart du début.
            let _ = mpv.command("seek", &["0", "absolute"]);
            let _ = mpv.set_property("pause", false);
        } else {
            let _ = mpv.set_property("pause", !paused);
        }
    });
    ui.on_stop(move || {
        let _ = mpv.set_property("pause", true);
        let _ = mpv.command("seek", &["0", "absolute"]);
    });
    let c = ctx.clone();
    ui.on_prev(move || c.play_offset(-1, false));
    let c = ctx.clone();
    ui.on_next(move || c.play_offset(1, false));
    let w = ui.as_weak();
    ui.on_seek_by(move |s| {
        let _ = mpv.command("seek", &[&s.to_string(), "relative"]);
        if let Some(ui) = w.upgrade() {
            toast(&ui, &format!("{}{} s", if s > 0.0 { "+" } else { "" }, s));
        }
    });
    ui.on_seek_to(move |f| {
        let _ = mpv.command("seek", &[&format!("{}", f * 100.0), "absolute-percent"]);
    });
    let w = ui.as_weak();
    ui.on_volume_changed(move |v| {
        let _ = mpv.set_property("volume", v as f64);
        if let Some(ui) = w.upgrade() {
            if ui.get_muted() && v > 0.0 {
                ui.set_muted(false);
                let _ = mpv.set_property("mute", false);
            }
            save_prefs(&ui);
        }
    });
    let w = ui.as_weak();
    ui.on_mute_changed(move |m| {
        let _ = mpv.set_property("mute", m);
        if let Some(ui) = w.upgrade() {
            toast(&ui, if m { "Muet" } else { "Son activé" });
        }
    });
    let w = ui.as_weak();
    ui.on_speed_changed(move |i| {
        let _ = mpv.set_property("speed", SPEEDS[i.clamp(0, 5) as usize]);
        if let Some(ui) = w.upgrade() {
            save_prefs(&ui);
        }
    });
    let w = ui.as_weak();
    ui.on_quality_changed(move |i| {
        let _ = mpv.set_property("ytdl-format", ytdl_format(HEIGHTS[i.clamp(0, 3) as usize]));
        let Some(ui) = w.upgrade() else { return };
        save_prefs(&ui);
        // Recharge la vidéo en cours à la même position avec la nouvelle qualité.
        if let Some(id) = with_app(|a| a.video.clone()) {
            let t: f64 = mpv.get_property("time-pos").unwrap_or(0.0);
            let _ = mpv.command("loadfile", &[&watch_url(&id), "replace", "-1", &format!("start={t:.1}")]);
        }
    });
    let w = ui.as_weak();
    ui.on_toggle_fullscreen(move || {
        let Some(ui) = w.upgrade() else { return };
        let on = !ui.window().is_fullscreen();
        ui.window().set_fullscreen(on);
        ui.set_fullscreen(on);
    });

    // --- sous-titres
    let c = ctx.clone();
    ui.on_generate(move |refresh| c.generate(refresh));
    let w = ui.as_weak();
    ui.on_export_srt(move || {
        let Some(ui) = w.upgrade() else { return };
        let (title, lang, cues) = with_app(|a| (a.title.clone(), a.result.as_ref().map(|r| r.1.clone()).unwrap_or_default(), a.cues.clone()));
        if cues.is_empty() {
            toast(&ui, "Rien à exporter");
            return;
        }
        match core::export_srt(&title, &lang, if ui.get_dual() { "dual" } else { "text" }, &cues) {
            Ok(path) => {
                toast(&ui, "Exporté");
                ui.set_status_text(format!("SRT enregistré : {}", path.display()).into());
            }
            Err(e) => ui.set_status_text(format!("Export impossible : {e:#}").into()),
        }
    });
    let c = ctx.clone();
    ui.on_swap_langs(move || {
        let ui = c.ui();
        let src = ui.get_src_lang();
        if src == 0 {
            toast(&ui, "Choisissez une langue source précise pour inverser");
            return;
        }
        let dst = ui.get_dst_lang();
        ui.set_src_lang(dst + 1);
        ui.set_dst_lang(src - 1);
        save_prefs(&ui);
        if with_app(|a| a.video.is_some()) && TRANSLATORS[ui.get_translator() as usize] != "claude" {
            c.generate(false);
        }
    });
    let c = ctx.clone();
    ui.on_prefs_changed(move || {
        let ui = c.ui();
        save_prefs(&ui);
        if with_app(|a| a.video.is_some()) && TRANSLATORS[ui.get_translator().clamp(0, 3) as usize] != "claude" {
            c.generate(false);
        }
    });
    let w = ui.as_weak();
    ui.on_subtitle_style_changed(move || {
        if let Some(ui) = w.upgrade() {
            save_prefs(&ui);
        }
    });

    // --- playlist, recherche, transcription
    let w = ui.as_weak();
    ui.on_playlist_select(move |i| {
        with_app(|a| a.selected = Some(i as usize));
        if let Some(ui) = w.upgrade() {
            refresh_playlist(&ui);
        }
    });
    let c = ctx.clone();
    ui.on_playlist_activate(move |i| {
        // Un clic lit la vidéo et la sélectionne (pour « − RETIRER »).
        let Some(id) = with_app(|a| {
            a.selected = Some(i as usize);
            a.playlist.get(i as usize).map(|e| e.id.clone())
        }) else {
            return;
        };
        if with_app(|a| a.video.as_deref() == Some(id.as_str())) {
            refresh_playlist(&c.ui()); // déjà en lecture : juste la sélection
        } else {
            c.open_video(&id, false);
        }
    });
    let w = ui.as_weak();
    ui.on_playlist_add(move || {
        let Some(ui) = w.upgrade() else { return };
        match youtube::video_id(&ui.get_url_text()) {
            Some(id) => {
                with_app(|a| {
                    if !a.playlist.iter().any(|e| e.id == id) {
                        a.playlist.push(Entry { id: id.clone(), title: id.clone() });
                    }
                });
                save_playlist();
                refresh_playlist(&ui);
                toast(&ui, "Ajouté");
            }
            None => toast(&ui, "URL YouTube invalide dans la barre d'adresse"),
        }
    });
    let w = ui.as_weak();
    ui.on_playlist_remove(move || {
        let Some(ui) = w.upgrade() else { return };
        let removed = with_app(|a| match a.selected.take() {
            Some(i) if i < a.playlist.len() => {
                a.playlist.remove(i);
                true
            }
            _ => false,
        });
        if removed {
            save_playlist();
            refresh_playlist(&ui);
        } else {
            toast(&ui, "Sélectionnez un élément");
        }
    });
    let w = ui.as_weak();
    ui.on_playlist_clear(move || {
        with_app(|a| {
            a.playlist.clear();
            a.selected = None;
        });
        save_playlist();
        if let Some(ui) = w.upgrade() {
            refresh_playlist(&ui);
        }
    });
    let c = ctx.clone();
    ui.on_result_play(move |i| {
        if let Some(h) = with_app(|a| a.results.get(i as usize).map(|h| (h.id.clone(), h.title.clone()))) {
            with_app(|a| {
                if !a.playlist.iter().any(|e| e.id == h.0) {
                    a.playlist.push(Entry { id: h.0.clone(), title: h.1.clone() });
                }
            });
            c.ui().set_tab(0);
            c.open_video(&h.0, false);
        }
    });
    let w = ui.as_weak();
    ui.on_result_add(move |i| {
        let Some(ui) = w.upgrade() else { return };
        let added = with_app(|a| {
            let Some(h) = a.results.get(i as usize).cloned() else { return false };
            if a.playlist.iter().any(|e| e.id == h.id) {
                return false;
            }
            a.playlist.push(Entry { id: h.id, title: h.title });
            true
        });
        if added {
            save_playlist();
            refresh_playlist(&ui);
            toast(&ui, "Ajouté à la playlist");
        }
    });
    let w = ui.as_weak();
    ui.on_transcript_click(move |row| {
        let start = with_app(|a| a.shown.get(row as usize).and_then(|&i| a.cues.get(i)).map(|c| c.start));
        if let (Some(start), Some(ui)) = (start, w.upgrade()) {
            let t = start + ui.get_offset() as f64 / 10.0;
            let _ = mpv.command("seek", &[&format!("{t:.2}"), "absolute"]);
        }
    });
    let w = ui.as_weak();
    ui.on_filter_changed(move |_| {
        if let Some(ui) = w.upgrade() {
            refresh_transcript(&ui);
        }
    });

    // --- réglages
    let c = ctx.clone();
    ui.on_open_settings(move || {
        let ui = c.ui();
        refresh_status(&ui, &c.core);
        ui.set_settings_open(true);
    });
    let c = ctx.clone();
    ui.on_save_settings(move |key, whisper_idx, nmt_idx| {
        let ui = c.ui();
        let key = (!key.trim().is_empty()).then(|| key.to_string());
        let whisper = core::WHISPER_MODELS[whisper_idx.clamp(0, 4) as usize].to_string();
        let nmt = NMT_CHOICES[nmt_idx.clamp(0, 1) as usize].to_string();
        match c.core.save_settings(key, whisper, nmt) {
            Ok(()) => toast(&ui, "Réglages enregistrés"),
            Err(e) => ui.set_status_text(format!("Réglages non enregistrés : {e:#}").into()),
        }
        refresh_status(&ui, &c.core);
    });

    // --- fenêtre sans bordure (barre de titre Winamp)
    use slint::winit_030::{WinitWindowAccessor, winit};
    let w = ui.as_weak();
    ui.on_win_drag(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().with_winit_window(|win: &winit::window::Window| {
                let _ = win.drag_window();
            });
        }
    });
    let w = ui.as_weak();
    ui.on_win_resize(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().with_winit_window(|win: &winit::window::Window| {
                let _ = win.drag_resize_window(winit::window::ResizeDirection::SouthEast);
            });
        }
    });
    let w = ui.as_weak();
    ui.on_win_minimize(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let w = ui.as_weak();
    ui.on_win_maximize(move || {
        if let Some(ui) = w.upgrade() {
            let m = ui.window().is_maximized();
            ui.window().set_maximized(!m);
        }
    });
    ui.on_win_close(move || {
        let _ = slint::quit_event_loop();
    });

    // --- boucles
    let c = ctx.clone();
    let ticker = slint::Timer::default();
    ticker.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || tick(&c));
    let w = ui.as_weak();
    let mut phase = 0.0f32;
    let viz = slint::Timer::default();
    viz.start(slint::TimerMode::Repeated, Duration::from_millis(40), move || {
        if let Some(ui) = w.upgrade() {
            animate_bars(&ui, &mut phase);
        }
    });

    ui.run()?;
    let _ = mpv.command("quit", &[]);
    drop(rt);
    Ok(())
}
