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

/// Traduit `cue.orig` vers `target` et remplit `cue.text`.
pub async fn translate_cues(
    cues: &mut [Cue],
    source: &str,
    target: &str,
    engine: Engine,
    api_key: Option<String>,
    progress: Progress,
) -> Result<()> {
    let batch_size = match engine {
        Engine::Google => 25,
        Engine::Claude => 80,
    };
    let texts: Vec<String> = cues.iter().map(|c| c.orig.clone()).collect();
    let batches: Vec<(usize, Vec<String>)> =
        texts.chunks(batch_size).enumerate().map(|(i, b)| (i * batch_size, b.to_vec())).collect();
    let total = batches.len();

    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300)).build()?;
    let limit = Arc::new(Semaphore::new(match engine {
        Engine::Google => 3,
        Engine::Claude => 4,
    }));
    let key = api_key.map(Arc::new);
    let mut set = JoinSet::new();
    for (offset, batch) in batches {
        let (http, limit, key) = (http.clone(), limit.clone(), key.clone());
        let (source, target) = (source.to_string(), target.to_string());
        set.spawn(async move {
            let _permit = limit.acquire_owned().await?;
            let out = match engine {
                Engine::Google => google_batch(&http, &batch, &source, &target).await,
                Engine::Claude => {
                    let key = key.ok_or_else(|| anyhow!("Aucune clé API Claude configurée (Réglages ou ANTHROPIC_API_KEY)"))?;
                    claude_batch(&http, &key, &batch, &source, &target).await
                }
            }?;
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
        progress(done as f32 / total as f32, format!("Traduction {done}/{total}"));
    }
    Ok(())
}

// ---------------------------------------------------------------- Google

async fn google_batch(http: &reqwest::Client, lines: &[String], source: &str, target: &str) -> Result<Vec<String>> {
    // Une requête par lot : les lignes sont séparées par des retours à la ligne,
    // que Google conserve. Si le découpage ne correspond pas, on repasse ligne par ligne.
    let joined = lines.join("\n");
    let out = google_one(http, &joined, source, target).await?;
    let parts: Vec<String> = out.split('\n').map(|s| s.trim().to_string()).collect();
    if parts.len() == lines.len() {
        return Ok(parts);
    }
    let mut res = Vec::with_capacity(lines.len());
    for l in lines {
        res.push(google_one(http, l, source, target).await?.trim().to_string());
    }
    Ok(res)
}

async fn google_one(http: &reqwest::Client, text: &str, source: &str, target: &str) -> Result<String> {
    let sl = if source.is_empty() { "auto" } else { source };
    let resp: Value = http
        .post("https://translate.googleapis.com/translate_a/single")
        .query(&[("client", "gtx"), ("sl", sl), ("tl", target), ("dt", "t")])
        .form(&[("q", text)])
        .send()
        .await
        .context("Google Translate injoignable")?
        .error_for_status()
        .context("Google Translate a refusé la requête (limite de débit ?)")?
        .json()
        .await?;
    let segments = resp[0].as_array().ok_or_else(|| anyhow!("réponse Google inattendue"))?;
    Ok(segments.iter().filter_map(|s| s[0].as_str()).collect())
}

// ---------------------------------------------------------------- Claude

const CLAUDE_MODEL: &str = "claude-opus-5";

async fn claude_batch(http: &reqwest::Client, key: &str, lines: &[String], source: &str, target: &str) -> Result<Vec<String>> {
    let from = if source == "auto" { "the detected source language".to_string() } else { lang_name(source).to_string() };
    let system = format!(
        "You translate video subtitles from {from} into {to}. You receive a JSON array of subtitle lines in order; \
         they are consecutive fragments of spoken speech, so use the surrounding lines as context. \
         Return exactly one translation per input line, in the same order, keeping each line short and natural \
         for on-screen reading. Never merge, split, skip or add lines. Keep names, numbers and brands as-is.",
        to = lang_name(target)
    );
    let body = json!({
        "model": CLAUDE_MODEL,
        "max_tokens": 16000,
        "fallbacks": "default",
        "system": system,
        "output_config": {
            "effort": "low",
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
