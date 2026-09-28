//! Cues de sous-titres : parsing (WebVTT, json3 YouTube), regroupement et export SRT.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cue {
    /// Début en secondes.
    pub start: f64,
    /// Fin en secondes.
    pub end: f64,
    /// Texte affiché (traduit si une traduction a été faite).
    pub text: String,
    /// Texte dans la langue d'origine.
    pub orig: String,
}

impl Cue {
    fn new(start: f64, end: f64, text: String) -> Self {
        Self { start, end, orig: text.clone(), text }
    }
}

/// Parse un fichier WebVTT, y compris les sous-titres automatiques "roll-up"
/// de YouTube où chaque cue répète la ligne précédente.
pub fn parse_vtt(src: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut prev_lines: Vec<String> = Vec::new();
    let mut lines = src.lines().peekable();

    while let Some(line) = lines.next() {
        let Some((start, end)) = parse_timing_line(line) else { continue };
        let mut text_lines = Vec::new();
        while let Some(l) = lines.peek() {
            if l.trim().is_empty() && !text_lines.is_empty() {
                break;
            }
            if parse_timing_line(l).is_some() {
                break;
            }
            let clean = strip_tags(lines.next().unwrap()).trim().to_string();
            if !clean.is_empty() {
                text_lines.push(clean);
            }
        }
        // Les cues de 10 ms servent seulement de transition dans les auto-captions.
        if end - start < 0.05 {
            continue;
        }
        let fresh: Vec<&String> = text_lines.iter().filter(|l| !prev_lines.contains(l)).collect();
        if !fresh.is_empty() {
            let text = fresh.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
            cues.push(Cue::new(start, end, decode_entities(&text)));
        }
        prev_lines = text_lines;
    }
    clip_overlaps(&mut cues);
    cues
}

fn parse_timing_line(line: &str) -> Option<(f64, f64)> {
    let (a, rest) = line.split_once("-->")?;
    let b = rest.trim().split_whitespace().next()?;
    Some((parse_ts(a.trim())?, parse_ts(b)?))
}

fn parse_ts(s: &str) -> Option<f64> {
    let s = s.replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (h.parse::<f64>().ok()?, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        [m, s] => (0.0, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        _ => return None,
    };
    Some(h * 3600.0 + m * 60.0 + sec)
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Parse le format json3 de YouTube (`events[].segs[].utf8`).
pub fn parse_json3(src: &str) -> anyhow::Result<Vec<Cue>> {
    #[derive(Deserialize)]
    struct Doc {
        #[serde(default)]
        events: Vec<Event>,
    }
    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "tStartMs", default)]
        start: f64,
        #[serde(rename = "dDurationMs", default)]
        dur: f64,
        #[serde(default)]
        segs: Vec<Seg>,
    }
    #[derive(Deserialize)]
    struct Seg {
        #[serde(default)]
        utf8: String,
    }

    let doc: Doc = serde_json::from_str(src)?;
    let mut cues: Vec<Cue> = doc
        .events
        .into_iter()
        .filter_map(|e| {
            let text: String = e.segs.iter().map(|s| s.utf8.as_str()).collect();
            let text = text.replace('\n', " ").trim().to_string();
            (!text.is_empty()).then(|| Cue::new(e.start / 1000.0, (e.start + e.dur) / 1000.0, text))
        })
        .collect();
    clip_overlaps(&mut cues);
    Ok(cues)
}

fn clip_overlaps(cues: &mut [Cue]) {
    for i in 0..cues.len().saturating_sub(1) {
        let next_start = cues[i + 1].start;
        if cues[i].end > next_start {
            cues[i].end = next_start.max(cues[i].start + 0.2);
        }
    }
}

/// Regroupe les fragments courts (auto-captions mot à mot) en phrases lisibles,
/// ce qui donne aussi une bien meilleure traduction.
pub fn merge_short(cues: Vec<Cue>) -> Vec<Cue> {
    const MAX_CHARS: usize = 110;
    const MAX_SECS: f64 = 7.0;
    let mut out: Vec<Cue> = Vec::new();
    for cue in cues {
        if let Some(last) = out.last_mut() {
            let ends_sentence = last.orig.trim_end().ends_with(['.', '!', '?', '…', '؟', '。']);
            let fits = last.orig.chars().count() + cue.orig.chars().count() < MAX_CHARS
                && cue.end - last.start <= MAX_SECS
                && cue.start - last.end < 1.0;
            if fits && !ends_sentence {
                last.orig = format!("{} {}", last.orig, cue.orig);
                last.text = last.orig.clone();
                last.end = cue.end;
                continue;
            }
        }
        out.push(cue);
    }
    out
}

pub fn to_srt(cues: &[Cue], field: impl Fn(&Cue) -> &str) -> String {
    fn ts(t: f64) -> String {
        let ms = (t * 1000.0).round() as u64;
        format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
    }
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, ts(c.start), ts(c.end), field(c)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtt_rollup_is_deduplicated() {
        let src = "WEBVTT\n\n00:00:12.559 --> 00:00:14.350 align:start\n \nso<00:00:12.759><c> in</c>\n\n\
                   00:00:14.350 --> 00:00:14.360\nso in\n \n\n\
                   00:00:14.360 --> 00:00:17.029\nso in\ncollege<00:00:15.360><c> I</c>\n";
        let cues = parse_vtt(src);
        let texts: Vec<_> = cues.iter().map(|c| c.orig.as_str()).collect();
        assert_eq!(texts, ["so in", "college I"]);
        assert!((cues[1].start - 14.36).abs() < 1e-6);
    }

    #[test]
    fn merge_joins_fragments_until_punctuation() {
        let cues = vec![
            Cue::new(0.0, 1.0, "Bonjour à".into()),
            Cue::new(1.0, 2.0, "tous.".into()),
            Cue::new(2.0, 3.0, "Suite".into()),
        ];
        let merged = merge_short(cues);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].orig, "Bonjour à tous.");
    }

    #[test]
    fn srt_timestamps() {
        let srt = to_srt(&[Cue::new(3661.5, 3662.25, "x".into())], |c| &c.text);
        assert!(srt.contains("01:01:01,500 --> 01:01:02,250"));
    }
}
