// YouTube Live Translator — interface (lecteur, sous-titres, playlist, LCD).
"use strict";

const $ = (id) => document.getElementById(id);
const LANG_NAMES = { ar: "العربية — Arabe", fr: "Français", en: "English — Anglais", de: "Deutsch — Allemand", tr: "Türkçe — Turc", es: "Español — Espagnol" };

// ------------------------------------------------------------ stockage local
const store = {
  get(key, fallback) {
    try { const v = localStorage.getItem("ytlt." + key); return v == null ? fallback : JSON.parse(v); } catch { return fallback; }
  },
  set(key, value) {
    try { localStorage.setItem("ytlt." + key, JSON.stringify(value)); } catch { /* stockage indisponible */ }
  },
};

// ------------------------------------------------------------ IPC fenêtre
function sendIpc(msg) {
  if (window.ipc && window.ipc.postMessage) { window.ipc.postMessage(msg); return true; }
  return false;
}
document.querySelectorAll("[data-ipc]").forEach((b) => b.addEventListener("click", () => {
  if (!sendIpc(b.dataset.ipc) && b.dataset.ipc === "close") window.close();
}));
$("titlebar").addEventListener("mousedown", (e) => { if (!e.target.closest("button") && e.button === 0) sendIpc("drag"); });
$("titlebar").addEventListener("dblclick", (e) => { if (!e.target.closest("button")) sendIpc("max"); });
$("grip").addEventListener("mousedown", (e) => { if (e.button === 0) sendIpc("resize"); });

// ------------------------------------------------------------ état
const state = {
  player: null,
  apiReady: false,
  videoId: null,
  title: "",
  cues: [],
  result: null,
  history: [],
  historyPos: -1,
  playlist: store.get("playlist", []),
  selected: -1,
  jobToken: 0,
  offset: 0,
  muted: false,
  volume: store.get("volume", 80),
  loop: false,
};

function videoId(input) {
  const s = (input || "").trim();
  if (/^[\w-]{11}$/.test(s)) return s;
  try {
    const u = new URL(s.startsWith("http") ? s : "https://" + s);
    if (!/(^|\.)youtube\.com$|(^|\.)youtu\.be$|youtube-nocookie\.com$/.test(u.hostname)) return null;
    const v = u.searchParams.get("v");
    if (v && /^[\w-]{11}$/.test(v)) return v;
    const m = u.pathname.match(/^\/(?:shorts\/|embed\/|live\/|v\/)?([\w-]{11})(?:\/|$)/);
    return m ? m[1] : null;
  } catch { return null; }
}

const fmt = (t) => {
  if (!isFinite(t) || t < 0) t = 0;
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = Math.floor(t % 60);
  const mm = String(m).padStart(2, "0"), ss = String(s).padStart(2, "0");
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
};

let toastTimer;
function toast(msg) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 1400);
}
const status = (msg) => { $("statusMsg").textContent = msg; };

async function api(path, opts = {}) {
  const res = await fetch(path, { headers: { "Content-Type": "application/json" }, ...opts });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(body.error || res.statusText);
  return body;
}

// ------------------------------------------------------------ outils / langues
async function refreshStatus() {
  try {
    const s = await api("/api/status");
    const dep = (ok, name) => `<span class="${ok ? "ok" : "ko"}">${ok ? "●" : "○"} ${name}</span>`;
    $("statusDeps").innerHTML = [dep(s.ytdlp, "yt-dlp"), dep(s.ffmpeg, "ffmpeg"), dep(s.whisper, "whisper"),
      dep(s.nmt_ready, "NMT local"), dep(s.claude_key, "clé Claude")].join(" &nbsp; ");
    $("whisperModel").value = s.whisper_model;
    $("nmtModel").value = s.nmt_model;
    if (s.setup && (s.setup.running || s.setup.message.startsWith("Échec"))) {
      setProgress(s.setup.progress, s.setup.message.toUpperCase(), s.setup.running ? "busy" : "error");
      if (s.setup.running) setTimeout(refreshStatus, 1500);
    } else if (s.setup && s.setup.message === "Modèles prêts" && $("progressText").textContent.startsWith("INSTALLATION")) {
      setProgress(1, "MODÈLES PRÊTS");
    }
    $("keyState").textContent = s.claude_key ? "Une clé est configurée." : "Aucune clé : le traducteur Claude est indisponible.";
    return s;
  } catch (e) {
    $("statusDeps").textContent = "Serveur local injoignable : " + e.message;
  }
}

function fillLangs() {
  for (const [code, name] of Object.entries(LANG_NAMES)) {
    $("srcLang").add(new Option(name, code));
    $("dstLang").add(new Option(name, code));
  }
  const prefs = store.get("prefs", {});
  $("srcLang").value = prefs.src || "auto";
  $("dstLang").value = prefs.dst || "fr";
  $("mode").value = prefs.mode || "auto";
  $("translator").value = prefs.translator || "local";
  $("voice").value = prefs.voice || "auto";
  $("addressee").value = prefs.addressee || "auto";
  $("fontSize").value = prefs.fontSize || 28;
  $("subPos").value = prefs.subPos || 8;
  $("subBg").value = prefs.subBg ?? 55;
  setToggle("btnDual", !!prefs.dual);
  $("engine").value = prefs.engine || "native";
  $("quality").value = prefs.quality || "720";
  applySubStyle();
}

function savePrefs() {
  store.set("prefs", {
    src: $("srcLang").value, dst: $("dstLang").value, mode: $("mode").value, translator: $("translator").value, voice: $("voice").value, addressee: $("addressee").value,
    engine: $("engine").value, quality: $("quality").value,
    fontSize: $("fontSize").value, subPos: $("subPos").value, subBg: $("subBg").value, dual: isOn("btnDual"),
  });
}

function applySubStyle() {
  const subs = $("subs");
  subs.style.setProperty("--sub-size", $("fontSize").value + "px");
  subs.style.setProperty("--sub-bottom", $("subPos").value + "%");
  subs.style.setProperty("--sub-bg", $("subBg").value / 100);
  subs.classList.toggle("dual", isOn("btnDual"));
  subs.classList.toggle("off", !isOn("btnSubs"));
}

// ------------------------------------------------------------ lecteur natif (façon VLC)
// YouTube ne sert presque plus de flux combinés : on lit la vidéo H.264 et l'audio AAC
// dans deux éléments synchronisés (la vidéo est l'horloge maîtresse).
class NativePlayer {
  // Horloge maîtresse = l'audio (une coupure de son s'entend, une image sautée se voit à peine).
  // On ne repositionne jamais l'audio sauf quand l'utilisateur cherche ; c'est la vidéo qui suit.
  constructor(video, audio, onState) {
    this.v = video; this.a = audio; this.onState = onState;
    this.hasAudio = false; this.want = false; this.title = ""; this.loaded = false; this.hls = null; this.rate = 1;
    const v = video, a = audio;
    const master = () => this.master();

    for (const el of [v, a]) {
      // WebKit annule un play() lancé avant que le flux soit prêt : on relance dès qu'il l'est.
      el.addEventListener("canplay", () => { if (this.want && el.paused) el.play().catch(() => {}); });
      el.addEventListener("error", () => {
        window.__log?.(`${el === v ? "video" : "audio"} error ${el.error?.code} ${el.error?.message}`);
        if (el === v) this.onError?.(v.error);
      });
    }
    v.addEventListener("loadedmetadata", () => onState(this.want ? 1 : 2));
    const report = () => onState(master().ended ? 0 : master().paused ? 2 : 1);
    for (const ev of ["play", "playing", "pause"]) {
      v.addEventListener(ev, () => { if (!this.hasAudio) report(); });
      a.addEventListener(ev, () => { if (this.hasAudio) report(); });
    }
    v.addEventListener("ended", () => { if (!this.hasAudio) onState(0); });
    a.addEventListener("ended", () => { if (this.hasAudio) { v.pause(); onState(0); } });

    // Si l'audio attend des données, la vidéo l'attend ; elle repart avec lui.
    a.addEventListener("waiting", () => { if (this.hasAudio) v.pause(); });
    a.addEventListener("playing", () => { if (this.hasAudio && this.want && v.paused) v.play().catch(() => {}); });

    // Synchronisation douce : on ajuste la vitesse de la vidéo, saut seulement si > 1 s d'écart.
    setInterval(() => {
      if (!this.hasAudio || !this.loaded || a.paused || v.seeking) return;
      const drift = v.currentTime - a.currentTime;
      if (Math.abs(drift) > 1) v.currentTime = a.currentTime;
      else if (Math.abs(drift) > 0.08) v.playbackRate = this.rate * (drift > 0 ? 0.95 : 1.05);
      else if (v.playbackRate !== this.rate) v.playbackRate = this.rate;
      if (v.paused && this.want && v.readyState >= 2) v.play().catch(() => {});
    }, 250);
  }
  master() { return this.hasAudio ? this.a : this.v; }
  async load(id, quality) {
    this.loaded = false; this.want = true;
    this.hls?.destroy(); this.hls = null;
    this.v.pause(); this.a.pause();
    // WebKit (l'app) lit mal les MP4 fragmentés de YouTube mais parfaitement le HLS.
    const nativeHls = !!this.v.canPlayType("application/vnd.apple.mpegurl");
    const s = await api(`/api/stream/${id}?q=${quality}&hls=${nativeHls}`);
    this.title = s.title;
    this.hasAudio = !!s.audio;
    this.v.muted = this.hasAudio;
    if (s.hls && !this.v.canPlayType("application/vnd.apple.mpegurl")) {
      await loadScript("https://cdn.jsdelivr.net/npm/hls.js@1/dist/hls.min.js");
      this.hls = new Hls();
      this.hls.loadSource(s.video);
      this.hls.attachMedia(this.v);
    } else {
      this.v.src = s.video;
    }
    if (s.audio) this.a.src = s.audio; else this.a.removeAttribute("src");
    this.loaded = true;
    this.applyVolume();
    this.startBoth();
    return s;
  }
  startBoth() {
    for (const el of this.hasAudio ? [this.a, this.v] : [this.v]) {
      el.play().catch((e) => {
        // AbortError = chargement pas encore prêt ; « canplay » relancera.
        if (e.name !== "AbortError") { window.__log?.("play() refusé : " + e.name + " " + e.message); this.want = false; this.onState(2); }
      });
    }
  }
  applyVolume() { this.master().volume = state.volume / 100; this.master().muted = state.muted; if (this.hasAudio) this.v.muted = true; }
  getCurrentTime() { return this.master().currentTime || 0; }
  getDuration() { const d = this.master().duration; return isFinite(d) ? d : 0; }
  seekTo(t) { this.a.currentTime = t; this.v.currentTime = t; }
  playVideo() { this.want = true; this.startBoth(); }
  pauseVideo() { this.want = false; this.a.pause(); this.v.pause(); }
  stopVideo() { this.pauseVideo(); this.seekTo(0); this.onState(-1); }
  getPlayerState() { const m = this.master(); return !this.loaded ? -1 : m.ended ? 0 : m.paused ? 2 : 1; }
  setVolume() { this.applyVolume(); }
  mute() { this.master().muted = true; }
  unMute() { this.master().muted = false; }
  setPlaybackRate(r) { this.rate = r; this.a.playbackRate = r; this.v.playbackRate = r; }
  getVideoLoadedFraction() {
    const b = this.master().buffered, d = this.getDuration();
    return b.length && d ? b.end(b.length - 1) / d : 0;
  }
  getVideoData() { return { title: this.title, author: "" }; }
  unloadModule() {}
  unload() { this.want = false; this.v.pause(); this.a.pause(); this.loaded = false; }
}

function loadScript(src) {
  return new Promise((resolve, reject) => {
    if (document.querySelector(`script[src="${src}"]`)) return resolve();
    const el = document.createElement("script");
    el.src = src; el.onload = resolve; el.onerror = () => reject(new Error("script non chargé"));
    document.head.append(el);
  });
}

const native = new NativePlayer($("video"), $("audio"), (code) => handleState(code));
native.onError = () => {
  if (state.player !== native || !state.videoId) return;
  toast("Flux natif illisible — bascule sur le lecteur YouTube");
  useEmbed(state.videoId);
};
$("video").addEventListener("click", togglePlay);

// ------------------------------------------------------------ lecteur YouTube (secours)
window.onYouTubeIframeAPIReady = () => {
  state.apiReady = true;
  if (state.pendingEmbed) { const id = state.pendingEmbed; state.pendingEmbed = null; useEmbed(id); }
};

function createPlayer(id) {
  state.yt = new YT.Player("player", {
    videoId: id,
    playerVars: {
      autoplay: 1, controls: 0, rel: 0, playsinline: 1, iv_load_policy: 3,
      cc_load_policy: 0, disablekb: 1, fs: 0, origin: location.origin,
    },
    events: {
      onReady: (e) => { e.target.setVolume(state.volume); e.target.playVideo(); hideNativeCaptions(); updateTitle(); },
      onStateChange: (e) => handleState(e.data),
      onError: (e) => {
        const why = { 2: "ID invalide", 5: "erreur du lecteur HTML5", 100: "vidéo introuvable ou privée", 101: "lecture intégrée interdite par l'auteur", 150: "lecture intégrée interdite par l'auteur", 153: "configuration du lecteur refusée" }[e.data] || `code ${e.data}`;
        if (state.player === state.yt && $("engine").value === "embed") {
          toast("YouTube refuse l'intégration — bascule sur le lecteur natif");
          useNative(state.videoId);
        } else setProgress(0, "ERREUR LECTEUR : " + why, "error");
      },
    },
  });
  state.player = state.yt;
}

function useEmbed(id) {
  native.unload();
  $("video").classList.add("hidden");
  $("ytWrap").classList.remove("hidden");
  if (!state.apiReady) { state.pendingEmbed = id; return; }
  if (state.yt && state.yt.loadVideoById) { state.player = state.yt; state.yt.loadVideoById(id); }
  else createPlayer(id);
}

async function useNative(id) {
  state.yt?.pauseVideo?.();
  $("ytWrap").classList.add("hidden");
  $("video").classList.remove("hidden");
  state.player = native;
  status("Résolution du flux vidéo…");
  try {
    await native.load(id, $("quality").value);
    if (state.videoId !== id) return;
    updateTitle();
    status("");
  } catch (e) {
    if (state.videoId !== id) return;
    toast("Flux natif indisponible — lecteur YouTube");
    status("Flux natif : " + e.message);
    useEmbed(id);
  }
}

function handleState(code) {
  const icon = code === 1 ? "M3 2h4v12H3zM9 2h4v12H9z" : "M4 2v12l10-6z";
  $("playIcon").setAttribute("d", icon);
  $("lcdState").textContent = code === 1 ? "▶" : code === 2 ? "❚❚" : "■";
  if (code === 1) { hideNativeCaptions(); updateTitle(); }
  if (code === 0) {
    if (state.loop) { state.player.seekTo(0); state.player.playVideo(); } else playOffset(1, true);
  }
}

// Nos sous-titres remplacent ceux de YouTube : on décharge son module de CC.
function hideNativeCaptions() {
  try { state.yt?.unloadModule("captions"); state.yt?.unloadModule("cc"); } catch { /* module absent */ }
}

function updateTitle() {
  const d = state.player?.getVideoData?.();
  if (d && d.title) {
    state.title = d.title;
    $("marquee").textContent = `*** ${d.title.toUpperCase()} *** ${d.author ? d.author + " ***" : ""}`;
    document.title = d.title + " — YouTube Live Translator";
    const item = state.playlist.find((p) => p.id === state.videoId);
    if (item && item.title !== d.title) { item.title = d.title; savePlaylist(); renderPlaylist(); }
  }
}

function openVideo(id, { fromHistory = false } = {}) {
  if (!id) return;
  $("splash").classList.add("hidden");
  $("urlInput").value = "https://www.youtube.com/watch?v=" + id;
  if (!fromHistory) {
    state.history = state.history.slice(0, state.historyPos + 1);
    if (state.history[state.history.length - 1] !== id) state.history.push(id);
    state.historyPos = state.history.length - 1;
  }
  state.videoId = id;
  state.cues = [];
  state.result = null;
  renderTranscript();
  renderSub(null);
  setTags();
  if (!state.playlist.some((p) => p.id === id)) {
    state.playlist.push({ id, title: id });
    savePlaylist();
  }
  renderPlaylist();
  if ($("engine").value === "embed") useEmbed(id); else useNative(id);
  // Génération automatique, sauf avec Claude (payant) où l'on attend un clic.
  if ($("translator").value !== "claude") generate();
  else setProgress(0, "CLIQUEZ SUR GÉNÉRER (CLAUDE)");
}

// ------------------------------------------------------------ génération
function setProgress(p, text, kind = "") {
  $("progressFill").style.width = Math.round(p * 100) + "%";
  $("progressText").textContent = text;
  $("progress").className = "progress " + kind;
}

async function generate(refresh = false) {
  if (!state.videoId) { toast("Ouvrez d'abord une vidéo"); return; }
  const token = ++state.jobToken;
  const body = {
    url: state.videoId, source: $("srcLang").value, target: $("dstLang").value,
    mode: $("mode").value, translator: $("translator").value, voice: $("voice").value, addressee: $("addressee").value, addressee: $("addressee").value, refresh,
  };
  $("btnGen").disabled = true;
  setProgress(0.01, "DÉMARRAGE…", "busy");
  try {
    const { id } = await api("/api/jobs", { method: "POST", body: JSON.stringify(body) });
    let partialRev = 0;
    for (;;) {
      await new Promise((r) => setTimeout(r, 500));
      if (token !== state.jobToken) return; // remplacé par une nouvelle demande
      const job = await api("/api/jobs/" + id);
      if (job.state === "running") {
        setProgress(job.progress, job.stage.toUpperCase(), "busy");
        // Affichage progressif : les sous-titres déjà traduits s'affichent sans attendre la fin.
        if (job.partial && job.partial_rev !== partialRev) {
          partialRev = job.partial_rev;
          state.cues = job.partial;
          shownCue = -2;
          renderTranscript();
          $("tagCues").textContent = state.cues.length + " CUES…";
        }
        continue;
      }
      if (job.state === "error") throw new Error(job.error);
      const r = job.result;
      if (r.video_id !== state.videoId) return;
      state.result = r;
      state.cues = r.cues;
      setTags();
      renderTranscript();
      setProgress(1, `${r.cues.length} SOUS-TITRES · ${r.origin} → ${r.translator}`.toUpperCase());
      status(r.title);
      toast("Sous-titres prêts");
      return;
    }
  } catch (e) {
    if (token === state.jobToken) setProgress(0, "ERREUR : " + e.message, "error");
  } finally {
    if (token === state.jobToken) $("btnGen").disabled = false;
  }
}

function setTags() {
  const r = state.result;
  $("tagSrc").textContent = "SRC " + (r ? r.source_lang.toUpperCase() : "--");
  $("tagDst").textContent = "DST " + (r ? r.target_lang.toUpperCase() : "--");
  $("tagEng").textContent = r ? r.translator.toUpperCase().slice(0, 10) : "---";
  $("tagCues").textContent = (r ? r.cues.length : 0) + " CUES";
}

// ------------------------------------------------------------ sous-titres
function cueAt(t) {
  const c = state.cues;
  let lo = 0, hi = c.length - 1, found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (c[mid].start <= t) { found = mid; lo = mid + 1; } else hi = mid - 1;
  }
  return found >= 0 && t <= c[found].end + 0.35 ? found : -1;
}

let shownCue = -2;
function renderSub(i) {
  if (i === shownCue) return;
  shownCue = i;
  const cue = i != null && i >= 0 ? state.cues[i] : null;
  $("subMain").textContent = cue ? cue.text : "";
  $("subOrig").textContent = cue && cue.orig !== cue.text ? cue.orig : "";
  highlightTranscript(i);
}

function renderTranscript() {
  const list = $("transcript");
  list.innerHTML = "";
  if (!state.cues.length) {
    list.innerHTML = '<li class="empty">Aucune transcription — cliquez sur GÉNÉRER.</li>';
    return;
  }
  const frag = document.createDocumentFragment();
  state.cues.forEach((c, i) => {
    const li = document.createElement("li");
    li.dataset.i = i;
    li.dataset.search = (c.text + " " + c.orig).toLowerCase();
    const n = document.createElement("span"); n.className = "n"; n.textContent = fmt(c.start);
    const t = document.createElement("span"); t.className = "t"; t.dir = "auto"; t.textContent = c.text;
    if (c.orig && c.orig !== c.text) { const s = document.createElement("small"); s.dir = "auto"; s.textContent = c.orig; t.append(s); }
    li.append(n, t);
    frag.append(li);
  });
  list.append(frag);
  applyFilter();
}

let lastHighlighted = null;
function highlightTranscript(i) {
  lastHighlighted?.classList.remove("now");
  lastHighlighted = null;
  if (i == null || i < 0) return;
  const li = $("transcript").children[i];
  if (!li || !li.dataset.i) return;
  li.classList.add("now");
  lastHighlighted = li;
  if (!$("tab-transcript").classList.contains("hidden") && !transcriptHover) li.scrollIntoView({ block: "nearest" });
}

let transcriptHover = false;
$("transcript").addEventListener("mouseenter", () => { transcriptHover = true; });
$("transcript").addEventListener("mouseleave", () => { transcriptHover = false; });
$("transcript").addEventListener("click", (e) => {
  const li = e.target.closest("li[data-i]");
  if (li && state.player) state.player.seekTo(state.cues[li.dataset.i].start + state.offset, true);
});
function applyFilter() {
  const q = $("trFilter").value.trim().toLowerCase();
  for (const li of $("transcript").children) if (li.dataset.search) li.classList.toggle("hidden", q && !li.dataset.search.includes(q));
}
$("trFilter").addEventListener("input", applyFilter);

// ------------------------------------------------------------ boucle d'affichage
function tick() {
  const p = state.player;
  if (p && p.getCurrentTime) {
    const t = p.getCurrentTime() || 0, d = p.getDuration() || 0;
    const frac = d ? t / d : 0;
    $("tCur").textContent = fmt(t);
    $("tDur").textContent = fmt(d);
    $("lcdTime").textContent = fmt(t);
    if (!seeking) {
      $("seekFill").style.width = frac * 100 + "%";
      $("seekKnob").style.left = frac * 100 + "%";
    }
    $("seekBuf").style.width = (p.getVideoLoadedFraction?.() || 0) * 100 + "%";
    renderSub(state.cues.length ? cueAt(t - state.offset) : -1);
  }
}
setInterval(tick, 100);

// Analyseur de spectre décoratif (l'audio de l'iframe YouTube n'est pas accessible).
const viz = $("viz"), vctx = viz.getContext("2d");
const bars = new Array(19).fill(0), peaks = new Array(19).fill(0);
function drawViz() {
  const playing = state.player?.getPlayerState?.() === 1;
  // Rien ne joue (ou fenêtre masquée) et barres retombées : on ralentit à 2 images/s.
  if ((!playing || document.hidden) && bars.every((b) => b < 0.01) && peaks.every((p) => p < 0.01)) {
    setTimeout(() => requestAnimationFrame(drawViz), 500);
    return;
  }
  const w = viz.width, h = viz.height, bw = w / bars.length;
  vctx.clearRect(0, 0, w, h);
  const now = performance.now() / 1000;
  bars.forEach((v, i) => {
    const target = playing ? (0.25 + 0.75 * Math.abs(Math.sin(now * (1.3 + i * 0.37) + i) * Math.sin(now * 0.7 + i * 1.7))) * (1 - i / 40) : 0;
    bars[i] = v + (target - v) * (target > v ? 0.5 : 0.12);
    peaks[i] = Math.max(peaks[i] - 0.012, bars[i]);
    const bh = Math.round(bars[i] * h);
    for (let y = 0; y < bh; y += 2) {
      const r = y / h;
      vctx.fillStyle = r > 0.8 ? "#ff5a4f" : r > 0.55 ? "#ffd23d" : "#3dff8a";
      vctx.fillRect(i * bw + 1, h - y - 1, bw - 2, 1);
    }
    vctx.fillStyle = "#c9c9c9";
    vctx.fillRect(i * bw + 1, h - Math.round(peaks[i] * h) - 1, bw - 2, 1);
  });
  requestAnimationFrame(drawViz);
}
requestAnimationFrame(drawViz);

// ------------------------------------------------------------ contrôles
const P = () => state.player;
function togglePlay() {
  const p = P(); if (!p?.getPlayerState) return;
  p.getPlayerState() === 1 ? p.pauseVideo() : p.playVideo();
}
function seekBy(s) { const p = P(); if (p?.getCurrentTime) { p.seekTo(Math.max(0, p.getCurrentTime() + s), true); toast((s > 0 ? "+" : "") + s + " s"); } }
function setVolume(v) {
  state.volume = Math.max(0, Math.min(100, Math.round(v)));
  $("volFill").style.setProperty("--v", state.volume + "%");
  P()?.setVolume?.(state.volume);
  if (state.muted && state.volume > 0) toggleMute(false);
  store.set("volume", state.volume);
}
function toggleMute(force) {
  state.muted = force ?? !state.muted;
  state.muted ? P()?.mute?.() : P()?.unMute?.();
  if (!state.muted) P()?.setVolume?.(state.volume);
  $("muteWaves").style.display = state.muted ? "none" : "";
  toast(state.muted ? "Muet" : "Son " + state.volume + "%");
}
const isOn = (id) => $(id).classList.contains("on");
function setToggle(id, on) { $(id).classList.toggle("on", on); }
function flip(id) { setToggle(id, !isOn(id)); applySubStyle(); savePrefs(); return isOn(id); }

$("btnPlay").onclick = togglePlay;
$("btnStop").onclick = () => { P()?.stopVideo?.(); };
$("btnBack10").onclick = () => seekBy(-10);
$("btnFwd10").onclick = () => seekBy(10);
$("btnPrev").onclick = () => playOffset(-1);
$("btnNext").onclick = () => playOffset(1);
$("btnSubs").onclick = () => toast(flip("btnSubs") ? "Sous-titres activés" : "Sous-titres masqués");
$("btnDual").onclick = () => toast(flip("btnDual") ? "Double ligne : original + traduction" : "Traduction seule");
$("btnLoop").onclick = () => { state.loop = !state.loop; setToggle("btnLoop", state.loop); toast(state.loop ? "Répétition activée" : "Répétition désactivée"); };
$("btnMute").onclick = () => toggleMute();
for (const id of ["engine", "quality"]) $(id).addEventListener("change", () => {
  savePrefs();
  if (!state.videoId) return;
  const t = P()?.getCurrentTime?.() || 0;
  const id2 = state.videoId;
  const done = () => { if (t > 1) setTimeout(() => P()?.seekTo?.(t, true), 800); };
  if ($("engine").value === "embed") { useEmbed(id2); done(); } else useNative(id2).then(done);
});
$("rate").onchange = (e) => P()?.setPlaybackRate?.(parseFloat(e.target.value));

// Le plein écran est piloté par la fenêtre Rust, qui renvoie l'état réel via __setTheater.
window.__setTheater = (on) => { $("app").classList.toggle("theater", !!on); };
function toggleFullscreen() {
  if (sendIpc("fs")) return;
  // Navigateur classique : API Fullscreen du document.
  if (!document.fullscreenElement) document.documentElement.requestFullscreen?.().catch(() => {});
  else document.exitFullscreen();
}
document.addEventListener("fullscreenchange", () => window.__setTheater(!!document.fullscreenElement));
$("btnFs").onclick = toggleFullscreen;
$("stage").addEventListener("dblclick", toggleFullscreen);
let controlsTimer;
$("stage").addEventListener("mousemove", () => {
  $("app").classList.add("show-controls");
  clearTimeout(controlsTimer);
  controlsTimer = setTimeout(() => $("app").classList.remove("show-controls"), 2000);
});

// Barre de progression (glisser pour chercher)
let seeking = false;
function seekFromEvent(e, commit) {
  const r = $("seek").getBoundingClientRect();
  const frac = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
  $("seekFill").style.width = frac * 100 + "%";
  $("seekKnob").style.left = frac * 100 + "%";
  const d = P()?.getDuration?.() || 0;
  $("tCur").textContent = fmt(frac * d);
  if (commit && d) P().seekTo(frac * d, true);
}
$("seek").addEventListener("mousedown", (e) => {
  seeking = true; seekFromEvent(e, false);
  const move = (ev) => seekFromEvent(ev, false);
  const up = (ev) => { seekFromEvent(ev, true); seeking = false; removeEventListener("mousemove", move); removeEventListener("mouseup", up); };
  addEventListener("mousemove", move); addEventListener("mouseup", up);
});
$("vol").addEventListener("mousedown", (e) => {
  const set = (ev) => { const r = $("vol").getBoundingClientRect(); setVolume(((ev.clientX - r.left) / r.width) * 100); };
  set(e);
  const up = () => { removeEventListener("mousemove", set); removeEventListener("mouseup", up); };
  addEventListener("mousemove", set); addEventListener("mouseup", up);
});
$("vol").addEventListener("wheel", (e) => { e.preventDefault(); setVolume(state.volume - Math.sign(e.deltaY) * 5); }, { passive: false });

// Réglages des sous-titres
for (const id of ["fontSize", "subPos", "subBg"]) $(id).addEventListener("input", () => { applySubStyle(); savePrefs(); });
$("offset").addEventListener("input", (e) => {
  state.offset = e.target.value / 10;
  $("offsetLabel").textContent = `SYNC ${state.offset > 0 ? "+" : ""}${state.offset.toFixed(1)}s`;
});
$("offset").addEventListener("dblclick", (e) => { e.target.value = 0; e.target.dispatchEvent(new Event("input")); });
for (const id of ["srcLang", "dstLang", "mode", "translator", "voice", "addressee"]) $(id).addEventListener("change", () => {
  if ((id === "voice" || id === "addressee") && $("translator").value !== "claude") toast("Genre (il/elle) pris en compte par le traducteur Claude uniquement");
  savePrefs();
  if (state.videoId && $("translator").value !== "claude") generate();
});
$("btnSwap").onclick = () => {
  const src = $("srcLang").value, dst = $("dstLang").value;
  if (src === "auto") { toast("Choisissez une langue source précise pour inverser"); return; }
  $("srcLang").value = dst; $("dstLang").value = src;
  $("dstLang").dispatchEvent(new Event("change"));
};
$("btnGen").onclick = () => generate(false);
$("btnRegen").onclick = () => generate(true);
$("btnExport").onclick = async () => {
  if (!state.cues.length) { toast("Rien à exporter"); return; }
  try {
    const r = await api("/api/export", {
      method: "POST",
      body: JSON.stringify({ title: state.title || state.videoId, lang: state.result.target_lang, which: isOn("btnDual") ? "dual" : "text", cues: state.cues }),
    });
    toast("Exporté"); status("SRT enregistré : " + r.path);
  } catch (e) { status("Export impossible : " + e.message); }
};

// ------------------------------------------------------------ navigation / recherche
$("urlForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = $("urlInput").value.trim();
  if (!text) return;
  const id = videoId(text);
  if (id) { openVideo(id); $("urlInput").blur(); return; }
  showTab("search");
  $("results").innerHTML = '<li class="empty">Recherche…</li>';
  try {
    const hits = await api("/api/search?q=" + encodeURIComponent(text));
    renderResults(hits);
  } catch (err) {
    $("results").innerHTML = "";
    const li = document.createElement("li"); li.className = "empty"; li.textContent = "Erreur : " + err.message;
    $("results").append(li);
  }
});
$("urlInput").addEventListener("focus", (e) => e.target.select());
$("btnBack").onclick = () => { if (state.historyPos > 0) openVideo(state.history[--state.historyPos], { fromHistory: true }); };
$("btnFwd").onclick = () => { if (state.historyPos < state.history.length - 1) openVideo(state.history[++state.historyPos], { fromHistory: true }); };
$("btnReload").onclick = () => { if (state.videoId) openVideo(state.videoId, { fromHistory: true }); };

function renderResults(hits) {
  const list = $("results");
  list.innerHTML = "";
  if (!hits.length) { list.innerHTML = '<li class="empty">Aucun résultat.</li>'; return; }
  hits.forEach((h, i) => {
    const li = document.createElement("li");
    li.title = `${h.title}\n${h.channel}\nDouble-clic : lire · Clic droit : ajouter à la playlist`;
    li.innerHTML = `<span class="n">${i + 1}.</span><span class="t"></span><span class="d">${h.duration ? fmt(h.duration) : "LIVE"}</span>`;
    li.querySelector(".t").textContent = h.title;
    li.ondblclick = () => { addToPlaylist(h.id, h.title); openVideo(h.id); showTab("playlist"); };
    li.oncontextmenu = (e) => { e.preventDefault(); addToPlaylist(h.id, h.title); toast("Ajouté à la playlist"); };
    list.append(li);
  });
}

// ------------------------------------------------------------ playlist
function savePlaylist() { store.set("playlist", state.playlist); }
function addToPlaylist(id, title) {
  if (!state.playlist.some((p) => p.id === id)) { state.playlist.push({ id, title: title || id }); savePlaylist(); renderPlaylist(); }
}
function renderPlaylist() {
  const list = $("playlist");
  list.innerHTML = "";
  if (!state.playlist.length) list.innerHTML = '<li class="empty">Playlist vide. Ouvrez une vidéo ou utilisez la recherche.</li>';
  state.playlist.forEach((p, i) => {
    const li = document.createElement("li");
    li.classList.toggle("sel", i === state.selected);
    li.classList.toggle("playing", p.id === state.videoId);
    li.innerHTML = `<span class="n">${i + 1}.</span><span class="t"></span><span class="d">${p.id === state.videoId ? "♪" : ""}</span>`;
    li.querySelector(".t").textContent = p.title;
    li.onclick = () => { state.selected = i; renderPlaylist(); };
    li.ondblclick = () => openVideo(p.id);
    list.append(li);
  });
  $("plCount").textContent = state.playlist.length + " élém.";
}
async function playOffset(dir, auto = false) {
  const i = state.playlist.findIndex((p) => p.id === state.videoId);
  const next = state.playlist[i + dir];
  if (next) { openVideo(next.id); return; }
  if (dir < 0) { toast("Début de la playlist"); return; }
  if (!state.videoId || state.fetchingMix) return;
  // Fin de la playlist : on enchaîne sur le Mix YouTube de la vidéo en cours.
  state.fetchingMix = true;
  toast("Recherche des vidéos suivantes (Mix YouTube)…");
  try {
    const hits = await api("/api/mix/" + state.videoId);
    const fresh = hits.filter((h) => !state.playlist.some((p) => p.id === h.id));
    if (!fresh.length) { toast("Pas de vidéo suivante trouvée"); return; }
    fresh.forEach((h) => state.playlist.push({ id: h.id, title: h.title }));
    savePlaylist();
    renderPlaylist();
    openVideo(fresh[0].id);
  } catch (e) {
    toast("Vidéo suivante indisponible");
    status("Mix YouTube : " + e.message);
  } finally {
    state.fetchingMix = false;
  }
}
$("plAdd").onclick = () => {
  const id = videoId($("urlInput").value);
  if (id) { addToPlaylist(id); toast("Ajouté"); } else toast("URL YouTube invalide dans la barre d'adresse");
};
$("plRem").onclick = () => {
  if (state.selected < 0) { toast("Sélectionnez un élément"); return; }
  state.playlist.splice(state.selected, 1); state.selected = -1; savePlaylist(); renderPlaylist();
};
$("plClear").onclick = () => { state.playlist = []; state.selected = -1; savePlaylist(); renderPlaylist(); };

function showTab(name) {
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t.dataset.tab === name));
  document.querySelectorAll(".tab-body").forEach((b) => b.classList.toggle("hidden", b.id !== "tab-" + name));
}
document.querySelectorAll(".tab").forEach((t) => t.addEventListener("click", () => showTab(t.dataset.tab)));

// ------------------------------------------------------------ réglages
$("btnSettings").onclick = async () => { await refreshStatus(); $("apiKey").value = ""; $("settings").showModal(); };
$("settings").addEventListener("close", async () => {
  if ($("settings").returnValue !== "save") return;
  const body = { whisper_model: $("whisperModel").value, nmt_model: $("nmtModel").value };
  if ($("apiKey").value.trim()) body.anthropic_key = $("apiKey").value.trim();
  $("apiKey").value = "";
  try { await api("/api/settings", { method: "POST", body: JSON.stringify(body) }); toast("Réglages enregistrés"); refreshStatus(); }
  catch (e) { status("Réglages non enregistrés : " + e.message); }
});

// ------------------------------------------------------------ clavier
addEventListener("keydown", (e) => {
  if (e.target.closest("input, select, textarea, dialog")) {
    if (e.key === "Escape") e.target.blur();
    return;
  }
  const k = e.key.toLowerCase();
  const actions = {
    " ": togglePlay, arrowleft: () => seekBy(-5), arrowright: () => seekBy(5),
    arrowup: () => { setVolume(state.volume + 5); toast("Volume " + state.volume + "%"); },
    arrowdown: () => { setVolume(state.volume - 5); toast("Volume " + state.volume + "%"); },
    f: toggleFullscreen, s: () => $("btnSubs").click(), d: () => $("btnDual").click(), m: () => toggleMute(),
    n: () => playOffset(1), p: () => playOffset(-1), g: () => generate(false),
    escape: () => { if ($("app").classList.contains("theater")) toggleFullscreen(); },
    l: () => $("urlInput").focus(),
  };
  if (k === "[" || k === "]") {
    const o = $("offset");
    o.value = Number(o.value) + (k === "]" ? 1 : -1);
    o.dispatchEvent(new Event("input"));
    toast($("offsetLabel").textContent);
    return;
  }
  if (actions[k]) { e.preventDefault(); actions[k](); }
});

// ------------------------------------------------------------ démarrage
fillLangs();
setVolume(state.volume);
renderPlaylist();
renderTranscript();
refreshStatus();
