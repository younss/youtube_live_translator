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

fn ends_sentence(text: &str) -> bool {
    text.trim_end().ends_with(['.', '!', '?', '…', '؟', '。'])
}

/// Regroupe les fragments très courts (auto-captions mot à mot) en lignes lisibles.
/// Les lignes restent courtes (~2 lignes à l'écran, ≤ 5 s) pour rester calées sur la voix :
/// le contexte nécessaire à la traduction est reconstitué à part, par [`translation_groups`].
pub fn merge_short(cues: Vec<Cue>) -> Vec<Cue> {
    const MAX_CHARS: usize = 84;
    const MAX_SECS: f64 = 5.0;
    let mut out: Vec<Cue> = Vec::new();
    for cue in cues {
        if let Some(last) = out.last_mut() {
            let fits = last.orig.chars().count() + cue.orig.chars().count() < MAX_CHARS
                && cue.end - last.start <= MAX_SECS
                && cue.start - last.end < 1.0;
            if fits && !ends_sentence(&last.orig) {
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

/// Découpe les cues en unités de traduction : des phrases entières quand la ponctuation le
/// permet, sinon des blocs bornés par les pauses de la voix. Traduire une ligne isolée (souvent
/// une moitié de phrase) donne des contresens ; traduire la phrase entière, beaucoup moins.
pub fn translation_groups(cues: &[Cue]) -> Vec<std::ops::Range<usize>> {
    const MAX_CHARS: usize = 160;
    const MAX_SECS: f64 = 10.0;
    const PAUSE: f64 = 1.2;
    let mut groups = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (i, c) in cues.iter().enumerate() {
        let n = c.orig.chars().count();
        if i > start {
            let prev = &cues[i - 1];
            let split = ends_sentence(&prev.orig)
                || c.start - prev.end > PAUSE
                || chars + n > MAX_CHARS
                || c.end - cues[start].start > MAX_SECS;
            if split {
                groups.push(start..i);
                start = i;
                chars = 0;
            }
        }
        chars += n + 1;
    }
    if start < cues.len() {
        groups.push(start..cues.len());
    }
    groups
}

/// Texte source d'un groupe, sur une seule ligne.
pub fn group_text(cues: &[Cue]) -> String {
    cues.iter().map(|c| c.orig.trim()).collect::<Vec<_>>().join(" ").replace('\n', " ")
}

/// Répartit la traduction d'un groupe sur ses cues, au prorata de la longueur du texte source
/// de chacune (coupure entre deux mots) : chaque morceau s'affiche pendant que la phrase
/// correspondante est prononcée, au lieu d'afficher toute la phrase d'avance.
pub fn distribute(translation: &str, cues: &mut [Cue]) {
    let translation = translation.trim();
    let words: Vec<&str> = translation.split_whitespace().collect();
    if cues.len() <= 1 || words.len() < cues.len() {
        for c in cues.iter_mut() {
            c.text = translation.to_string();
        }
        return;
    }
    let weights: Vec<f64> = cues.iter().map(|c| c.orig.chars().count().max(1) as f64).collect();
    let total_w: f64 = weights.iter().sum();
    let lens: Vec<usize> = words.iter().map(|w| w.chars().count() + 1).collect();
    let total_len = lens.iter().sum::<usize>() as f64;
    let (mut from, mut len, mut acc_w) = (0usize, 0usize, 0.0);
    let last = cues.len() - 1;
    for (i, cue) in cues.iter_mut().enumerate() {
        let to = if i == last {
            words.len()
        } else {
            acc_w += weights[i];
            let target = acc_w / total_w * total_len;
            // Au moins un mot par cue, et un mot au moins pour chacune des suivantes.
            let max_to = words.len() - (last - i);
            let mut to = from + 1;
            len += lens[from];
            while to < max_to && ((len + lens[to]) as f64 - target).abs() <= (len as f64 - target).abs() {
                len += lens[to];
                to += 1;
            }
            to
        };
        cue.text = words[from..to].join(" ");
        from = to;
    }
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
    fn groups_follow_sentences_and_pauses() {
        let cues = vec![
            Cue::new(0.0, 1.0, "I don't know".into()),
            Cue::new(1.0, 2.0, "why you left.".into()),
            Cue::new(2.1, 3.0, "Come back".into()),
            Cue::new(5.0, 6.0, "tomorrow".into()),
        ];
        assert_eq!(translation_groups(&cues), [0..2, 2..3, 3..4]);
        assert_eq!(group_text(&cues[0..2]), "I don't know why you left.");
    }

    #[test]
    fn distribute_splits_by_source_length() {
        let mut cues = vec![Cue::new(0.0, 1.0, "I don't know".into()), Cue::new(1.0, 2.0, "why you left me here.".into())];
        distribute("Je ne sais pas pourquoi tu m'as laissé ici.", &mut cues);
        assert_eq!(cues[0].text, "Je ne sais pas");
        assert_eq!(cues[1].text, "pourquoi tu m'as laissé ici.");
        // Traduction trop courte pour être répartie : chaque cue reçoit tout.
        distribute("Non.", &mut cues);
        assert!(cues.iter().all(|c| c.text == "Non."));
    }

    #[test]
    fn srt_timestamps() {
        let srt = to_srt(&[Cue::new(3661.5, 3662.25, "x".into())], |c| &c.text);
        assert!(srt.contains("01:01:01,500 --> 01:01:02,250"));
    }
}
