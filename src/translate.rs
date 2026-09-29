//! Traduction des cues : Google Translate (gratuit, sans clé) ou Claude (clé API Anthropic).

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::subs::Cue;

pub const LANGS: [(&str, &str); 6] = [
    ("ar", "Arabic"),
    ("fr", "French"),
    ("en", "English"),
    ("de", "German"),
    ("tr", "Turkish"),
    ("es", "Spanish"),
];

pub fn lang_name(code: &str) -> &str {
    LANGS.iter().find(|(c, _)| *c == code).map(|(_, n)| *n).unwrap_or(code)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Google,
    Claude,
}

pub type Progress = Arc<dyn Fn(f32, String) + Send + Sync>;

/// Ce que le traducteur doit savoir en plus des lignes : de quoi parle la vidéo et qui parle.
#[derive(Clone, Debug, Default)]
pub struct TranslationContext {
    pub title: String,
    /// Qui parle : "auto", "female" ou "male".
    pub voice: String,
    /// À qui / de qui on parle : "auto", "female" ou "male".
    pub addressee: String,
}

/// Traduit `cue.orig` vers `target` et remplit `cue.text`.
/// Renvoie le nom du service réellement utilisé (Google peut basculer sur MyMemory).
pub async fn translate_cues(
    cues: &mut [Cue],
    source: &str,
    target: &str,
    engine: Engine,
    api_key: Option<String>,
    context: TranslationContext,
    progress: Progress,
) -> Result<String> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        // Google signale un blocage par une redirection vers /sorry : on veut la voir.
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    match engine {
        Engine::Claude => {
            let key = api_key.ok_or_else(|| anyhow!("Aucune clé API Claude configurée (Réglages ou ANTHROPIC_API_KEY)"))?;
            translate_claude(cues, source, target, &http, key, context, progress).await?;
            Ok("Claude".into())
        }
        Engine::Google => {
            // Google traduit ligne à ligne sans contexte : on lui envoie des phrases entières,
            // puis on répartit chaque traduction sur les cues de sa phrase.
            let groups = crate::subs::translation_groups(cues);
            let mut units: Vec<Cue> = groups
                .iter()
                .map(|g| {
                    let text = crate::subs::group_text(&cues[g.clone()]);
                    Cue { start: cues[g.start].start, end: cues[g.end - 1].end, orig: text.clone(), text }
                })
                .collect();
            let who = translate_free(&mut units, source, target, &http, progress).await?;
            for (g, unit) in groups.iter().zip(&units) {
                crate::subs::distribute(&unit.text, &mut cues[g.clone()]);
            }
            Ok(who)
        }
    }
}

async fn translate_claude(
    cues: &mut [Cue],
    source: &str,
    target: &str,
    http: &reqwest::Client,
    key: String,
    context: TranslationContext,
    progress: Progress,
) -> Result<()> {
    const BATCH: usize = 80;
    let texts: Vec<String> = cues.iter().map(|c| c.orig.clone()).collect();
    let batches: Vec<(usize, Vec<String>)> =
        texts.chunks(BATCH).enumerate().map(|(i, b)| (i * BATCH, b.to_vec())).collect();
    let total = batches.len();
    let (key, context, limit) = (Arc::new(key), Arc::new(context), Arc::new(Semaphore::new(4)));
    let mut set = JoinSet::new();
    for (offset, batch) in batches {
        let (http, limit, key, context) = (http.clone(), limit.clone(), key.clone(), context.clone());
        let (source, target) = (source.to_string(), target.to_string());
        set.spawn(async move {
            let _permit = limit.acquire_owned().await?;
            let out = claude_batch(&http, &key, &batch, &source, &target, &context).await?;
            anyhow::Ok((offset, out))
        });
    }
    let mut done = 0;
    while let Some(res) = set.join_next().await {
        let (offset, out) = res??;
        for (i, t) in out.into_iter().enumerate() {
            if let Some(c) = cues.get_mut(offset + i) {
                c.text = t;
            }
        }
        done += 1;
        progress(done as f32 / total as f32, format!("Traduction Claude {done}/{total}"));
    }
    Ok(())
}

// ---------------------------------------------------------------- Google, puis MyMemory

/// Google est essayé en premier, par lots, un seul à la fois pour ne pas déclencher son
/// anti-robot. S'il bloque (captcha « unusual traffic »), on continue avec MyMemory.
async fn translate_free(cues: &mut [Cue], source: &str, target: &str, http: &reqwest::Client, progress: Progress) -> Result<String> {
    const BATCH: usize = 40;
    let total = cues.len();
    let mut google_ok = true;
    let mut i = 0;
    while i < total {
        let end = (i + BATCH).min(total);
        let lines: Vec<String> = cues[i..end].iter().map(|c| c.orig.clone()).collect();
        let out = if google_ok {
            match google_batch(http, &lines, source, target).await {
                Ok(out) => Some(out),
                Err(e) if e.is::<GoogleBlocked>() => {
                    google_ok = false;
                    progress(i as f32 / total as f32, "Google bloqué — bascule sur MyMemory…".into());
                    None
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let out = match out {
            Some(out) => out,
            None => mymemory_batch(http, &lines, source, target).await?,
        };
        for (c, t) in cues[i..end].iter_mut().zip(out) {
            c.text = t;
        }
        i = end;
        let who = if google_ok { "Google" } else { "MyMemory" };
        progress(i as f32 / total as f32, format!("Traduction {who} {i}/{total}"));
    }
    Ok(if google_ok { "Google".into() } else { "MyMemory (Google bloqué)".into() })
}

#[derive(Debug)]
struct GoogleBlocked;

impl std::fmt::Display for GoogleBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Google Translate bloque temporairement cette adresse IP (trop de requêtes) — réessayez plus tard ou utilisez Claude")
    }
}

impl std::error::Error for GoogleBlocked {}

async fn google_batch(http: &reqwest::Client, lines: &[String], source: &str, target: &str) -> Result<Vec<String>> {
    // Les lignes sont séparées par des retours à la ligne, que Google conserve en général.
    let out = google_one(http, &lines.join("\n"), source, target).await?;
    let parts: Vec<String> = out.split('\n').map(|s| s.trim().to_string()).collect();
    if parts.len() == lines.len() {
        return Ok(parts);
    }
    // Découpage différent (rare) : on renvoie le lot en deux moitiés plutôt que ligne par ligne,
    // pour limiter le nombre de requêtes.
    if lines.len() == 1 {
        return Ok(vec![out.trim().to_string()]);
    }
    let mid = lines.len() / 2;
    let mut left = Box::pin(google_batch(http, &lines[..mid], source, target)).await?;
    left.extend(Box::pin(google_batch(http, &lines[mid..], source, target)).await?);
    Ok(left)
}

async fn google_one(http: &reqwest::Client, text: &str, source: &str, target: &str) -> Result<String> {
    let sl = if source.is_empty() { "auto" } else { source };
    let resp = http
        .post("https://translate.googleapis.com/translate_a/single")
        .query(&[("client", "gtx"), ("sl", sl), ("tl", target), ("dt", "t")])
        .form(&[("q", text)])
        .send()
        .await
        .context("Google Translate injoignable")?;
    let status = resp.status();
    if status.is_redirection() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(GoogleBlocked.into());
    }
    let resp: Value = resp.error_for_status().context("Google Translate a refusé la requête")?.json().await?;
    let segments = resp[0].as_array().ok_or_else(|| anyhow!("réponse Google inattendue"))?;
    Ok(segments.iter().filter_map(|s| s[0].as_str()).collect())
}

/// MyMemory (gratuit, sans clé, ~5000 caractères/jour) : une ligne par requête, 500 octets max.
async fn mymemory_batch(http: &reqwest::Client, lines: &[String], source: &str, target: &str) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if source == "auto" {
            bail!("MyMemory a besoin de la langue source : choisissez-la dans « DE » au lieu de « Auto »");
        }
        let q: String = line.chars().take(450).collect();
        let v: Value = http
            .get("https://api.mymemory.translated.net/get")
            .query(&[("q", q.as_str()), ("langpair", &format!("{source}|{target}"))])
            .send()
            .await
            .context("MyMemory injoignable")?
            .json()
            .await
            .context("réponse MyMemory illisible")?;
        let code = v["responseStatus"].as_i64().or_else(|| v["responseStatus"].as_str().and_then(|s| s.parse().ok()));
        if code != Some(200) {
            bail!("MyMemory : {}", v["responseDetails"].as_str().unwrap_or("quota atteint"));
        }
        out.push(v["responseData"]["translatedText"].as_str().unwrap_or(line).to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------- Claude

const CLAUDE_MODEL: &str = "claude-opus-5";

async fn claude_batch(
    http: &reqwest::Client,
    key: &str,
    lines: &[String],
    source: &str,
    target: &str,
    context: &TranslationContext,
) -> Result<Vec<String>> {
    let from = if source == "auto" { "the detected source language".to_string() } else { lang_name(source).to_string() };
    let voice = match context.voice.as_str() {
        "female" => "The speaker (or singer) is a woman: when she refers to herself, use feminine grammatical forms \
                     (adjectives, participles, verb agreement) wherever the target language marks gender.",
        "male" => "The speaker (or singer) is a man: when he refers to himself, use masculine grammatical forms \
                   wherever the target language marks gender.",
        _ => "Infer the speaker's gender from the video title and the lines (e.g. a known female singer) and use the \
              matching grammatical forms when they refer to themselves; if it cannot be inferred, prefer neutral wording.",
    };
    let addressee = match context.addressee.as_str() {
        "female" => "The person being addressed or talked about (\"you\", \"he/she\") is a woman: use feminine forms \
                     for her (e.g. Arabic أنتِ and feminine verb endings, French feminine agreement).",
        "male" => "The person being addressed or talked about (\"you\", \"he/she\") is a man: use masculine forms \
                   for him (e.g. Arabic أنتَ and masculine verb endings, French masculine agreement).",
        _ => "The source language may not mark gender (Turkish has no gendered pronouns), but the target may require it: \
              infer the gender of the person being addressed or talked about from the whole context (title, speaker, \
              story of the lyrics) and use it consistently across all lines.",
    };
    let system = format!(
        "You translate video subtitles from {from} into {to}. The video is titled \"{title}\". \
         You receive a JSON array of subtitle lines in order; they are consecutive fragments of speech or song \
         lyrics, so use the surrounding lines as context. {voice} {addressee} \
         Never translate proper names (people, places, brands, song titles): keep them as they are, or transliterate \
         them phonetically when the target language uses another script (e.g. Arabic). \
         Lines are often fragments of one sentence split across several subtitles: translate the whole sentence \
         with its meaning, then cut your translation at the same places so each line matches what is being said \
         on screen at that moment. \
         Return exactly one translation per input line, in the same order, keeping each line short and natural \
         for on-screen reading. Never merge, split, skip or add lines.",
        to = lang_name(target),
        title = context.title.replace('"', "'"),
    );
    let body = json!({
        "model": CLAUDE_MODEL,
        "max_tokens": 16000,
        "fallbacks": "default",
        "system": system,
        "output_config": {
            "effort": "medium",
            "format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": { "translations": { "type": "array", "items": { "type": "string" } } },
                    "required": ["translations"],
                    "additionalProperties": false
                }
            }
        },
        "messages": [{ "role": "user", "content": serde_json::to_string(lines)? }]
    });

    // Un nombre de lignes différent est rare ; on réessaie une fois avant d'abandonner.
    for _ in 0..2 {
        let resp = http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "server-side-fallback-2026-07-01")
            .json(&body)
            .send()
            .await
            .context("API Claude injoignable")?;
        let status = resp.status();
        let v: Value = resp.json().await?;
        if !status.is_success() {
            bail!("API Claude {status} : {}", v["error"]["message"].as_str().unwrap_or("erreur inconnue"));
        }
        match v["stop_reason"].as_str() {
            Some("refusal") => bail!("Claude a refusé de traduire ce passage"),
            Some("max_tokens") => bail!("Réponse Claude tronquée (max_tokens)"),
            _ => {}
        }
        let text: String = v["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        let parsed: Value = serde_json::from_str(&text).context("JSON Claude invalide")?;
        let out: Vec<String> = parsed["translations"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| s.as_str().unwrap_or_default().to_string())
            .collect();
        if out.len() == lines.len() {
            return Ok(out);
        }
    }
    bail!("Claude a renvoyé un nombre de lignes différent")
}
