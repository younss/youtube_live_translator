//! Décodage audio en Rust pur (sans ffmpeg) : flux HLS de YouTube -> PCM 16 kHz mono.
//!
//! Les segments audio HLS de YouTube sont de l'« audio empaqueté » : une balise ID3 suivie de
//! trames AAC au format ADTS (44,1 kHz stéréo). On découpe les trames, on les décode avec
//! symphonia, on mélange en mono et on rééchantillonne à 16 kHz pour Whisper.

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::audio::{Channels, Position};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::packet::Packet;
use symphonia::core::units::{Duration, Timestamp};

const OUT_RATE: f64 = 16_000.0;
/// Échantillons (par canal) d'une trame AAC-LC.
const AAC_FRAME: usize = 1024;
const SAMPLE_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// Décodeur AAC/ADTS à état : accepte les segments dans l'ordre et renvoie du PCM 16 kHz mono.
pub struct AdtsDecoder {
    decoder: Option<(Box<dyn AudioDecoder>, u32)>,
    resampler: Option<Resampler>,
    frames: i64,
    scratch: Vec<f32>,
}

impl AdtsDecoder {
    pub fn new() -> Self {
        Self { decoder: None, resampler: None, frames: 0, scratch: Vec::new() }
    }

    /// Décode un segment (ou un fichier ADTS entier) ; les balises ID3 sont ignorées.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<f32>> {
        let mut mono = Vec::new();
        let mut rate = 0;
        let mut i = 0;
        while i + 7 <= data.len() {
            if &data[i..i + 3] == b"ID3" && i + 10 <= data.len() {
                i += id3_len(&data[i..]);
                continue;
            }
            if data[i] != 0xFF || data[i + 1] & 0xF0 != 0xF0 {
                i += 1; // resynchronisation sur le prochain mot de synchro ADTS
                continue;
            }
            let h = &data[i..];
            let protection_absent = h[1] & 1 == 1;
            let profile = (h[2] >> 6) & 3;
            let sf_index = ((h[2] >> 2) & 0xF) as usize;
            let chan_cfg = ((h[2] & 1) << 2) | (h[3] >> 6);
            let frame_len = (((h[3] & 3) as usize) << 11) | ((h[4] as usize) << 3) | ((h[5] as usize) >> 5);
            let header_len = if protection_absent { 7 } else { 9 };
            if frame_len < header_len || i + frame_len > data.len() || sf_index >= SAMPLE_RATES.len() {
                i += 1;
                continue;
            }
            let payload = &data[i + header_len..i + frame_len];
            i += frame_len;

            let sample_rate = SAMPLE_RATES[sf_index];
            if self.decoder.as_ref().is_none_or(|(_, r)| *r != sample_rate) {
                self.decoder = Some((make_decoder(profile, sf_index as u8, chan_cfg, sample_rate)?, sample_rate));
                self.resampler = Some(Resampler::new(sample_rate as f64));
            }
            let (decoder, r) = self.decoder.as_mut().unwrap();
            rate = *r;
            let packet = Packet::new(0, Timestamp::new(self.frames * 1024), Duration::new(1024), payload.to_vec());
            self.frames += 1;
            let buf = match decoder.decode(&packet) {
                Ok(buf) => buf,
                Err(_) => {
                    // Trame corrompue : on la remplace par du silence de même durée. La sauter
                    // raccourcirait l'audio et décalerait tous les sous-titres suivants.
                    mono.extend(std::iter::repeat_n(0.0, AAC_FRAME));
                    continue;
                }
            };
            let channels = buf.spec().channels().count().max(1);
            self.scratch.clear();
            buf.copy_to_vec_interleaved::<f32>(&mut self.scratch);
            mono.extend(self.scratch.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
        }
        if rate == 0 {
            return Ok(Vec::new());
        }
        Ok(self.resampler.as_mut().unwrap().process(&mono))
    }
}

/// Taille d'une balise ID3v2 (en-tête de 10 octets, taille « synchsafe », pied optionnel).
fn id3_len(h: &[u8]) -> usize {
    let size = ((h[6] as usize & 0x7F) << 21) | ((h[7] as usize & 0x7F) << 14) | ((h[8] as usize & 0x7F) << 7) | (h[9] as usize & 0x7F);
    let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

fn make_decoder(profile: u8, sf_index: u8, chan_cfg: u8, sample_rate: u32) -> Result<Box<dyn AudioDecoder>> {
    // AudioSpecificConfig : type d'objet (5 bits) | fréquence (4) | canaux (4) | 3 bits à 0.
    let object_type = profile + 1;
    let asc = [(object_type << 3) | (sf_index >> 1), ((sf_index & 1) << 7) | (chan_cfg << 3)];
    let channels = match chan_cfg {
        1 => Channels::Positioned(Position::FRONT_CENTER),
        _ => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
    };
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(sample_rate)
        .with_channels(channels)
        .with_extra_data(Box::new(asc));
    symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| anyhow!("décodeur AAC : {e}"))
}

/// Rééchantillonneur vers 16 kHz : filtre passe-bas à sinus cardinal fenêtré (anti-repliement)
/// puis interpolation. Garde l'historique entre deux appels pour un flux sans coupure.
struct Resampler {
    step: f64,
    taps: Vec<f32>,
    hist: Vec<f32>,
    pos: f64,
}

impl Resampler {
    fn new(rate_in: f64) -> Self {
        const N: usize = 63;
        let cutoff = 7_600.0 / rate_in; // un peu sous Nyquist (8 kHz)
        let mid = (N / 2) as f64;
        let mut taps: Vec<f32> = (0..N)
            .map(|k| {
                let x = k as f64 - mid;
                let sinc = if x == 0.0 { 2.0 * cutoff } else { (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x) };
                let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / (N - 1) as f64).cos()
                    + 0.08 * (4.0 * std::f64::consts::PI * k as f64 / (N - 1) as f64).cos(); // Blackman
                (sinc * w) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self { step: rate_in / OUT_RATE, hist: vec![0.0; N], taps, pos: N as f64 }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let n = self.taps.len();
        let mut buf = std::mem::take(&mut self.hist);
        buf.extend_from_slice(input);
        let filtered = |i: usize| -> f32 { self.taps.iter().zip(&buf[i + 1 - n..=i]).map(|(t, x)| t * x).sum() };
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 1);
        while (self.pos as usize) + 1 < buf.len() {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let (a, b) = (filtered(i), filtered(i + 1));
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        let keep = n + 1;
        let drop = buf.len().saturating_sub(keep);
        self.hist = buf[drop..].to_vec();
        self.pos -= drop as f64;
        out
    }
}

/// Lit une playlist HLS audio et appelle `sink` avec le PCM 16 kHz de chaque segment, dans l'ordre.
pub async fn stream_hls(http: &reqwest::Client, playlist_url: &str, mut sink: impl FnMut(&[f32])) -> Result<()> {
    let playlist = http.get(playlist_url).send().await?.error_for_status().context("playlist HLS")?.text().await?;
    let base = playlist_url.rsplit_once('/').map(|(b, _)| b).unwrap_or(playlist_url);
    let segments: Vec<String> = playlist
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| if l.starts_with("http") { l.to_string() } else { format!("{base}/{l}") })
        .collect();
    if segments.is_empty() {
        bail!("playlist HLS vide");
    }
    let mut decoder = AdtsDecoder::new();
    // On télécharge le segment suivant pendant qu'on décode le courant.
    let mut next = Some(tokio::spawn(fetch(http.clone(), segments[0].clone())));
    for i in 0..segments.len() {
        let data = next.take().unwrap().await??;
        next = segments.get(i + 1).map(|u| tokio::spawn(fetch(http.clone(), u.clone())));
        let pcm = decoder.feed(&data)?;
        sink(&pcm);
    }
    Ok(())
}

async fn fetch(http: reqwest::Client, url: String) -> Result<bytes::Bytes> {
    let mut last = anyhow!("segment inaccessible");
    for _ in 0..3 {
        match http.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => return Ok(r.bytes().await?),
            Err(e) => last = e.into(),
        }
    }
    Err(last)
}

/// Décode un fichier audio téléchargé (repli) : ADTS/HLS concaténé, ou MP4/M4A via symphonia.
pub fn decode_file(path: &std::path::Path) -> Result<Vec<f32>> {
    let data = std::fs::read(path)?;
    let looks_adts = data.starts_with(b"ID3") || (data.len() > 2 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0);
    if looks_adts {
        return AdtsDecoder::new().feed(&data);
    }
    decode_container(data)
}

fn decode_container(data: Vec<u8>) -> Result<Vec<f32>> {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;

    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), Default::default())
        .map_err(|e| anyhow!("format audio non reconnu : {e}"))?;
    let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("pas de piste audio"))?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or_else(|| anyhow!("paramètres audio manquants"))?.clone();
    let rate = params.sample_rate.ok_or_else(|| anyhow!("fréquence inconnue"))? as f64;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| anyhow!("codec audio non pris en charge : {e}"))?;
    let mut resampler = Resampler::new(rate);
    let (mut out, mut scratch) = (Vec::new(), Vec::new());
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track_id {
            continue;
        }
        let Ok(buf) = decoder.decode(&packet) else {
            out.extend(resampler.process(&[0.0; AAC_FRAME])); // garde la chronologie (voir `feed`)
            continue;
        };
        let channels = buf.spec().channels().count().max(1);
        scratch.clear();
        buf.copy_to_vec_interleaved::<f32>(&mut scratch);
        let mono: Vec<f32> = scratch.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect();
        out.extend(resampler.process(&mono));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    #[test]
    fn resampler_keeps_duration_and_level() {
        // 1 s de 440 Hz à 44,1 kHz, en deux morceaux -> ~16000 échantillons, amplitude conservée.
        let tone: Vec<f32> = (0..44_100).map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44_100.0).sin()).collect();
        let mut r = Resampler::new(44_100.0);
        let mut out = r.process(&tone[..20_000]);
        out.extend(r.process(&tone[20_000..]));
        assert!((out.len() as i64 - 16_000).abs() < 100, "{}", out.len());
        let peak = out[1000..15_000].iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!((peak - 1.0).abs() < 0.05, "{peak}");
    }

    #[test]
    fn id3_header_length() {
        let h = [b'I', b'D', b'3', 3, 0, 0, 0, 0, 0x01, 0x7F];
        assert_eq!(super::id3_len(&h), 10 + 255);
    }
}
