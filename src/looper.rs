use std::f32::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
pub const CONFIDENCE_THRESHOLD: f32 = 0.72;

const FFT_SIZE: usize = 512;
const FEAT_SR: f32 = 11_025.0;
const FEAT_HOP_11K: usize = 110; // ~9.977 ms at 11025 Hz
const STEREO_HOP: usize = FEAT_HOP_11K * 4; // 440 stereo frames at 44100 Hz
const XFADE_FRAMES: usize = 882; // 20 ms at 44100 Hz
const FEAT_DIM: usize = 20; // 12 chroma bins + 8 log sub-band energy bins

static LOOP_TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone)]
pub struct LoopDetection {
    pub loop_start_secs: f64,
    pub loop_end_secs: f64,
    pub confidence: f32,
    pub is_seamless_internal: bool,
    pub(crate) loop_start_frame: usize,
    pub(crate) loop_end_frame: usize,
    pub(crate) active_start_frame: usize,
    pub(crate) non_silent_end_frame: usize,
}

impl LoopDetection {
    pub fn summary_label(&self) -> String {
        if self.is_seamless_internal {
            format!(
                "Seamless loop {} -> {} (confianza {:.0}%)",
                format_mm_ss_ms(self.loop_start_secs),
                format_mm_ss_ms(self.loop_end_secs),
                self.confidence * 100.0
            )
        } else {
            format!(
                "Loop completo sin silencio {} -> {} (1 pasada)",
                format_mm_ss_ms(self.loop_start_secs),
                format_mm_ss_ms(self.loop_end_secs)
            )
        }
    }
}

pub struct PreparedLoopConcat {
    pub concat_list_path: PathBuf,
    pub temp_files: Vec<PathBuf>,
    pub detection: LoopDetection,
}

impl PreparedLoopConcat {
    pub fn cleanup(&self) {
        let _ = fs::remove_file(&self.concat_list_path);
        for p in &self.temp_files {
            let _ = fs::remove_file(p);
        }
    }
}

impl Drop for PreparedLoopConcat {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn format_mm_ss_ms(secs: f64) -> String {
    let clamped = secs.max(0.0);
    let mins = (clamped / 60.0).floor() as u64;
    let rem = clamped - (mins as f64) * 60.0;
    format!("{:02}:{:06.3}", mins, rem)
}

pub fn decode_audio_stereo_s16le(
    ffmpeg: &Path,
    audio_path: &Path,
    time_range: Option<(f64, Option<f64>)>,
) -> Result<Vec<i16>> {
    let mut cmd = crate::new_hidden_command(ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error");

    if let Some((start_secs, end_secs)) = time_range {
        cmd.arg("-ss").arg(format!("{:.3}", start_secs.max(0.0)));
        if let Some(end) = end_secs {
            let dur = (end - start_secs).max(0.1);
            cmd.arg("-t").arg(format!("{:.3}", dur));
        }
    }

    cmd.arg("-i")
        .arg(audio_path)
        .arg("-vn")
        .arg("-ac")
        .arg(CHANNELS.to_string())
        .arg("-ar")
        .arg(SAMPLE_RATE.to_string())
        .arg("-f")
        .arg("s16le")
        .arg("-");

    let out = cmd.output().with_context(|| {
        format!(
            "No se pudo decodificar PCM para detección de loop: {}",
            audio_path.display()
        )
    })?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "FFmpeg falló decodificando audio para loop ({}): {}",
            audio_path.display(),
            if stderr.is_empty() {
                "sin detalles".to_string()
            } else {
                stderr
            }
        );
    }

    let num_samples = (out.stdout.len() / 2) & !1;
    let mut samples = Vec::with_capacity(num_samples);
    for chunk in out.stdout[..num_samples * 2].chunks_exact(2) {
        samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }

    if samples.len() < (SAMPLE_RATE as usize) * CHANNELS {
        bail!(
            "Audio demasiado corto (< 1s) para análisis de loop: {}",
            audio_path.display()
        );
    }

    Ok(samples)
}

fn fft_in_place(re: &mut [f32; FFT_SIZE], im: &mut [f32; FFT_SIZE]) {
    let n = FFT_SIZE;
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2usize;
    while len <= n {
        let half = len >> 1;
        let angle = -2.0 * PI / (len as f32);
        let (w_step_im, w_step_re) = angle.sin_cos();
        for start in (0..n).step_by(len) {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..half {
                let u = start + k;
                let v = u + half;
                let t_re = w_re * re[v] - w_im * im[v];
                let t_im = w_re * im[v] + w_im * re[v];
                re[v] = re[u] - t_re;
                im[v] = im[u] - t_im;
                re[u] += t_re;
                im[u] += t_im;
                let next_re = w_re * w_step_re - w_im * w_step_im;
                let next_im = w_re * w_step_im + w_im * w_step_re;
                w_re = next_re;
                w_im = next_im;
            }
        }
        len <<= 1;
    }
}

fn extract_features(stereo_pcm: &[i16]) -> (Vec<[f32; FEAT_DIM]>, Vec<f32>) {
    // Downsample 4x stereo (44100 Hz) -> mono 11025 Hz
    let num_11k = stereo_pcm.len() / 8;
    let mut mono_11k = Vec::with_capacity(num_11k);
    let scale = 1.0f32 / (8.0 * 32768.0);
    for ch in stereo_pcm.chunks_exact(8) {
        let sum = ch[0] as f32
            + ch[1] as f32
            + ch[2] as f32
            + ch[3] as f32
            + ch[4] as f32
            + ch[5] as f32
            + ch[6] as f32
            + ch[7] as f32;
        mono_11k.push(sum * scale);
    }

    if mono_11k.len() < FFT_SIZE {
        return (Vec::new(), Vec::new());
    }

    let mut hann = [0.0f32; FFT_SIZE];
    for (i, h) in hann.iter_mut().enumerate() {
        *h = 0.5 * (1.0 - (2.0 * PI * (i as f32) / (FFT_SIZE as f32)).cos());
    }

    let mut bin_chroma = [usize::MAX; FFT_SIZE / 2];
    let mut bin_band = [usize::MAX; FFT_SIZE / 2];
    let band_edges = [
        40.0f32, 100.0, 200.0, 400.0, 800.0, 1400.0, 2400.0, 3800.0, 5500.0,
    ];
    for k in 1..(FFT_SIZE / 2) {
        let freq = (k as f32) * FEAT_SR / (FFT_SIZE as f32);
        if (65.0..=4200.0).contains(&freq) {
            let midi = 69.0 + 12.0 * (freq / 440.0).log2();
            let pc = (midi.round() as i32).rem_euclid(12) as usize;
            bin_chroma[k] = pc;
        }
        for b in 0..8 {
            if freq >= band_edges[b] && freq < band_edges[b + 1] {
                bin_band[k] = b;
                break;
            }
        }
    }

    let est_frames = (mono_11k.len() - FFT_SIZE) / FEAT_HOP_11K + 1;
    let mut feats = Vec::with_capacity(est_frames);
    let mut rms_list = Vec::with_capacity(est_frames);

    let mut re = [0.0f32; FFT_SIZE];
    let mut im = [0.0f32; FFT_SIZE];
    let chroma_w = 0.65f32.sqrt();
    let band_w = 0.35f32.sqrt();

    let mut pos = 0usize;
    while pos + FFT_SIZE <= mono_11k.len() {
        let slice = &mono_11k[pos..pos + FFT_SIZE];
        let mut sq_sum = 0.0f32;
        for i in 0..FFT_SIZE {
            let s = slice[i];
            sq_sum += s * s;
            re[i] = s * hann[i];
            im[i] = 0.0;
        }
        let rms = (sq_sum / (FFT_SIZE as f32)).sqrt();
        rms_list.push(rms);

        fft_in_place(&mut re, &mut im);

        let mut chroma = [0.0f32; 12];
        let mut bands = [0.0f32; 8];
        for k in 1..(FFT_SIZE / 2) {
            let mag = (re[k] * re[k] + im[k] * im[k]).sqrt();
            let c_idx = bin_chroma[k];
            if c_idx < 12 {
                chroma[c_idx] += mag;
            }
            let b_idx = bin_band[k];
            if b_idx < 8 {
                bands[b_idx] += mag;
            }
        }

        let c_norm = chroma.iter().map(|v| v * v).sum::<f32>().sqrt();
        let b_norm = bands.iter().map(|v| v * v).sum::<f32>().sqrt();

        let mut vec20 = [0.0f32; FEAT_DIM];
        if c_norm > 1e-8 {
            let inv = chroma_w / c_norm;
            for i in 0..12 {
                vec20[i] = chroma[i] * inv;
            }
        }
        if b_norm > 1e-8 {
            let inv = band_w / b_norm;
            for i in 0..8 {
                vec20[12 + i] = bands[i] * inv;
            }
        }
        feats.push(vec20);
        pos += FEAT_HOP_11K;
    }

    (feats, rms_list)
}

#[inline]
fn feat_dot(a: &[f32; FEAT_DIM], b: &[f32; FEAT_DIM]) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..FEAT_DIM {
        sum += a[i] * b[i];
    }
    sum
}

#[inline]
fn pair_similarity(
    feats: &[[f32; FEAT_DIM]],
    rms: &[f32],
    a: usize,
    b: usize,
) -> f32 {
    let ra = rms[a];
    let rb = rms[b];
    if ra < 1e-4 && rb < 1e-4 {
        return 0.0;
    }
    let sim = feat_dot(&feats[a], &feats[b]).clamp(0.0, 1.0);
    let rms_ratio = (ra.min(rb) + 1e-4) / (ra.max(rb) + 1e-4);
    sim * rms_ratio
}

struct LagEval {
    lag: usize,
    score: f32,
}

fn evaluate_lag(
    feats: &[[f32; FEAT_DIM]],
    rms: &[f32],
    active_start: usize,
    sustained_end: usize,
    lag: usize,
    t_step: usize,
) -> Option<LagEval> {
    if active_start + lag + 120 > sustained_end {
        return None;
    }
    let overlap_frames = sustained_end - active_start - lag;
    let sampled_len = (overlap_frames + t_step - 1) / t_step;
    if sampled_len < 10 {
        return None;
    }

    let win_frames = ((lag * 4) / 5).clamp(150, 1200).min(overlap_frames);
    let win = (win_frames / t_step).max(1).min(sampled_len);

    let mut vals = Vec::with_capacity(sampled_len);
    let mut high_count = 0usize;
    let mut t = active_start;
    let limit = active_start + overlap_frames;
    while t < limit {
        let s = pair_similarity(feats, rms, t, t + lag);
        if s >= 0.80 {
            high_count += 1;
        }
        vals.push(s);
        t += t_step;
    }

    let mut running = vals[..win].iter().sum::<f32>();
    let mut best_win_sim = running / (win as f32);
    for i in win..vals.len() {
        running += vals[i] - vals[i - win];
        let avg = running / (win as f32);
        if avg > best_win_sim {
            best_win_sim = avg;
        }
    }

    let coverage = (overlap_frames as f32 / lag as f32).min(1.0);
    let high_frac = (high_count as f32) / (vals.len() as f32);
    let score = best_win_sim * (1.0 + 0.08 * coverage + 0.05 * high_frac);
    Some(LagEval { lag, score })
}

fn refine_sample_alignment(
    stereo_pcm: &[i16],
    start_frame: usize,
    approx_end_frame: usize,
    max_search_frame: usize,
) -> (usize, f32) {
    let total_frames = stereo_pcm.len() / CHANNELS;
    if total_frames < XFADE_FRAMES + 100 || start_frame >= approx_end_frame {
        return (approx_end_frame, 0.0);
    }

    let left_ctx = 551usize.min(start_frame); // ~12.5 ms before splice
    let right_ctx = 551usize.min(total_frames.saturating_sub(start_frame)); // ~12.5 ms after splice
    if left_ctx + right_ctx < 256 {
        return (approx_end_frame, 0.0);
    }
    let wlen = left_ctx + right_ctx;

    let mut template = Vec::with_capacity(wlen);
    for i in (start_frame - left_ctx)..(start_frame + right_ctx) {
        let m = (stereo_pcm[i * 2] as f32 + stereo_pcm[i * 2 + 1] as f32) * (0.5 / 32768.0);
        template.push(m);
    }
    let norm_a = template.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm_a < 1e-5 {
        return (approx_end_frame, 0.0);
    }

    let min_end = (start_frame + 44_100).max(left_ctx);
    let max_end = max_search_frame
        .min(total_frames.saturating_sub(right_ctx))
        .max(min_end);
    if min_end >= max_end {
        return (approx_end_frame.clamp(min_end, total_frames.saturating_sub(1)), 0.0);
    }

    let eval_ncc = |cand_end: usize| -> f32 {
        let base = cand_end - left_ctx;
        let mut dot = 0.0f32;
        let mut sum_sq_b = 0.0f32;
        for (k, &va) in template.iter().enumerate() {
            let idx = (base + k) * 2;
            let vb = (stereo_pcm[idx] as f32 + stereo_pcm[idx + 1] as f32) * (0.5 / 32768.0);
            dot += va * vb;
            sum_sq_b += vb * vb;
        }
        let denom = norm_a * sum_sq_b.sqrt();
        if denom > 1e-6 {
            dot / denom
        } else {
            0.0
        }
    };

    let lo = (approx_end_frame as isize - 1764).max(min_end as isize) as usize;
    let hi = (approx_end_frame + 1764).min(max_end);
    if lo > hi {
        return (approx_end_frame.min(max_end), 0.0);
    }

    let mut best_coarse_end = approx_end_frame.clamp(lo, hi);
    let mut best_coarse_ncc = f32::NEG_INFINITY;
    let mut d = lo;
    while d <= hi {
        let ncc = eval_ncc(d);
        if ncc > best_coarse_ncc {
            best_coarse_ncc = ncc;
            best_coarse_end = d;
        }
        d += 4;
    }

    let fine_lo = best_coarse_end.saturating_sub(4).max(lo);
    let fine_hi = (best_coarse_end + 4).min(hi);
    let mut best_end = best_coarse_end;
    let mut best_ncc = best_coarse_ncc;
    for cand in fine_lo..=fine_hi {
        let ncc = eval_ncc(cand);
        if ncc > best_ncc {
            best_ncc = ncc;
            best_end = cand;
        }
    }

    (best_end, best_ncc)
}

pub fn detect_loop_points_from_pcm(stereo_pcm: &[i16], sample_rate: u32) -> LoopDetection {
    let total_frames = stereo_pcm.len() / CHANNELS;
    let sr = sample_rate.max(1) as f64;
    let total_secs = (total_frames as f64) / sr;

    let (feats, rms) = extract_features(stereo_pcm);
    if feats.len() < 400 {
        return LoopDetection {
            loop_start_secs: 0.0,
            loop_end_secs: total_secs,
            confidence: 0.0,
            is_seamless_internal: false,
            loop_start_frame: 0,
            loop_end_frame: total_frames,
            active_start_frame: 0,
            non_silent_end_frame: total_frames,
        };
    }

    let mut sorted_rms = rms.clone();
    sorted_rms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p98_idx = ((sorted_rms.len() as f32) * 0.98) as usize;
    let peak_rms = sorted_rms[p98_idx.min(sorted_rms.len() - 1)].max(1e-4);

    let silence_thresh = (peak_rms * 0.015).max(1e-4);
    let sustained_thresh = (peak_rms * 0.25).max(1e-3);

    let active_start = rms
        .iter()
        .position(|&r| r > silence_thresh)
        .unwrap_or(0);
    let non_silent_end = rms
        .iter()
        .rposition(|&r| r > silence_thresh)
        .map(|i| (i + 1).min(feats.len()))
        .unwrap_or(feats.len());
    let sustained_end = rms
        .iter()
        .rposition(|&r| r > sustained_thresh)
        .map(|i| (i + 1).min(feats.len()))
        .unwrap_or(non_silent_end);

    let active_start_frame = (active_start * STEREO_HOP).min(total_frames);
    let non_silent_end_frame = ((non_silent_end * STEREO_HOP) + FFT_SIZE * 4)
        .min(total_frames)
        .max(active_start_frame + 1);

    let active_frames = sustained_end.saturating_sub(active_start);
    if active_frames < 400 {
        return LoopDetection {
            loop_start_secs: (active_start_frame as f64) / sr,
            loop_end_secs: (non_silent_end_frame as f64) / sr,
            confidence: 0.0,
            is_seamless_internal: false,
            loop_start_frame: active_start_frame,
            loop_end_frame: non_silent_end_frame,
            active_start_frame,
            non_silent_end_frame,
        };
    }

    let min_lag = ((active_frames as f32 * 0.20) as usize).max(200);
    let max_lag = ((active_frames as f32 * 0.78) as usize).min(active_frames.saturating_sub(150));
    if min_lag >= max_lag {
        return LoopDetection {
            loop_start_secs: (active_start_frame as f64) / sr,
            loop_end_secs: (non_silent_end_frame as f64) / sr,
            confidence: 0.0,
            is_seamless_internal: false,
            loop_start_frame: active_start_frame,
            loop_end_frame: non_silent_end_frame,
            active_start_frame,
            non_silent_end_frame,
        };
    }

    // Coarse scan over lag L
    let t_step_coarse = (active_frames / 3000).clamp(1, 4);
    let mut coarse_evals = Vec::new();
    let mut lag = min_lag;
    while lag <= max_lag {
        if let Some(ev) =
            evaluate_lag(&feats, &rms, active_start, sustained_end, lag, t_step_coarse)
        {
            coarse_evals.push(ev);
        }
        lag += 4;
    }

    if coarse_evals.is_empty() {
        return LoopDetection {
            loop_start_secs: (active_start_frame as f64) / sr,
            loop_end_secs: (non_silent_end_frame as f64) / sr,
            confidence: 0.0,
            is_seamless_internal: false,
            loop_start_frame: active_start_frame,
            loop_end_frame: non_silent_end_frame,
            active_start_frame,
            non_silent_end_frame,
        };
    }

    coarse_evals.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut top_peaks: Vec<usize> = Vec::with_capacity(6);
    for ev in &coarse_evals {
        if top_peaks
            .iter()
            .all(|&p| ev.lag.abs_diff(p) >= 25)
        {
            top_peaks.push(ev.lag);
            if top_peaks.len() >= 6 {
                break;
            }
        }
    }

    // Fine scan around the top 6 lag peaks at exact 1-frame (10 ms) resolution
    let mut best_eval = LagEval {
        lag: coarse_evals[0].lag,
        score: -1.0,
    };
    for &peak in &top_peaks {
        let lo = peak.saturating_sub(4).max(min_lag);
        let hi = (peak + 4).min(max_lag);
        for cand_lag in lo..=hi {
            if let Some(ev) = evaluate_lag(&feats, &rms, active_start, sustained_end, cand_lag, 1)
            {
                if ev.score > best_eval.score {
                    best_eval = ev;
                }
            }
        }
    }

    let best_lag = best_eval.lag;
    let overlap_frames = sustained_end - active_start - best_lag;
    let min_t_frame = active_start.max(4); // at least ~40 ms into audio so XFADE_FRAMES fits cleanly
    let max_t_frame = active_start + overlap_frames.saturating_sub(40);

    // Stage 2: Select t_start along the best lag L
    let mut local_sims = Vec::with_capacity(overlap_frames);
    for t in active_start..(active_start + overlap_frames) {
        local_sims.push(pair_similarity(&feats, &rms, t, t + best_lag));
    }

    let local_win = (best_lag / 6).clamp(50, 150).min(local_sims.len()).max(1);
    let mut smoothed = vec![0.0f32; local_sims.len()];
    let mut acc = local_sims[..local_win].iter().sum::<f32>();
    for i in 0..local_sims.len() {
        smoothed[i] = if i + local_win <= local_sims.len() {
            let avg = acc / (local_win as f32);
            if i + local_win < local_sims.len() {
                acc += local_sims[i + local_win] - local_sims[i];
            }
            avg
        } else {
            let rem = &local_sims[i..];
            rem.iter().sum::<f32>() / (rem.len() as f32)
        };
    }

    let max_smoothed = smoothed.iter().copied().fold(0.0f32, f32::max);
    let plateau_thresh = (max_smoothed * 0.88).max(0.65);

    let short_onset_sim = |i: usize| -> f32 {
        let end = (i + 10).min(local_sims.len());
        if end <= i {
            return 0.0;
        }
        local_sims[i..end].iter().sum::<f32>() / ((end - i) as f32)
    };

    // Pick early inside the plateau (+5 frames ≈ 50 ms so both pre-splice and post-splice match)
    let mut best_t_frame = min_t_frame.min(active_start + overlap_frames.saturating_sub(1));
    let mut feat_sim = max_smoothed;
    let first_rel_opt = (0..smoothed.len())
        .find(|&i| smoothed[i] >= plateau_thresh && short_onset_sim(i) >= plateau_thresh)
        .or_else(|| smoothed.iter().position(|&v| v >= plateau_thresh));
    if let Some(first_rel) = first_rel_opt {
        let plateau_start_frame = active_start + first_rel;
        let offset = 6usize.min(overlap_frames.saturating_sub(first_rel + 1));
        let candidate_t = (plateau_start_frame + offset).clamp(min_t_frame, max_t_frame.max(min_t_frame));
        let rel_idx = candidate_t.saturating_sub(active_start).min(smoothed.len() - 1);
        best_t_frame = candidate_t;
        feat_sim = smoothed[rel_idx];
    }

    let raw_start_frame = (best_t_frame * STEREO_HOP).max(XFADE_FRAMES);
    let approx_end_frame = ((best_t_frame + best_lag) * STEREO_HOP)
        .min(total_frames.saturating_sub(1))
        .max(raw_start_frame + 44_100);

    // Stage 3: Sample-level cross-correlation refinement
    let max_search_frame = non_silent_end_frame.min(total_frames);
    let (refined_end_frame, ncc) =
        refine_sample_alignment(stereo_pcm, raw_start_frame, approx_end_frame, max_search_frame);

    let confidence = (0.80 * feat_sim + 0.20 * ncc.max(0.0)).clamp(0.0, 1.0);
    let is_seamless_internal =
        confidence >= CONFIDENCE_THRESHOLD && refined_end_frame > raw_start_frame + XFADE_FRAMES * 2;

    if is_seamless_internal {
        LoopDetection {
            loop_start_secs: (raw_start_frame as f64) / sr,
            loop_end_secs: (refined_end_frame as f64) / sr,
            confidence,
            is_seamless_internal: true,
            loop_start_frame: raw_start_frame,
            loop_end_frame: refined_end_frame,
            active_start_frame,
            non_silent_end_frame,
        }
    } else {
        LoopDetection {
            loop_start_secs: (active_start_frame as f64) / sr,
            loop_end_secs: (non_silent_end_frame as f64) / sr,
            confidence,
            is_seamless_internal: false,
            loop_start_frame: active_start_frame,
            loop_end_frame: non_silent_end_frame,
            active_start_frame,
            non_silent_end_frame,
        }
    }
}

pub fn build_crossfaded_loop_unit(
    stereo_pcm: &[i16],
    start_frame: usize,
    end_frame: usize,
) -> Vec<i16> {
    let total_frames = stereo_pcm.len() / CHANNELS;
    let s = start_frame.min(total_frames);
    let e = end_frame.min(total_frames);
    if e <= s {
        return Vec::new();
    }

    let loop_frames = e - s;
    let xfade = XFADE_FRAMES.min(loop_frames / 3).min(s);
    let mut out = Vec::with_capacity(loop_frames * CHANNELS);

    let steady_end = e - xfade;
    out.extend_from_slice(&stereo_pcm[(s * CHANNELS)..(steady_end * CHANNELS)]);

    for k in 0..xfade {
        let w = 0.5 * (1.0 - (PI * (k as f32) / (xfade as f32)).cos());
        let tail_idx = (e - xfade + k) * CHANNELS;
        let head_idx = (s - xfade + k) * CHANNELS;
        for ch in 0..CHANNELS {
            let a = stereo_pcm[tail_idx + ch] as f32;
            let b = stereo_pcm[head_idx + ch] as f32;
            let mixed = a * (1.0 - w) + b * w;
            out.push(mixed.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
        }
    }

    out
}

fn build_trimmed_single_pass_loop_unit(
    stereo_pcm: &[i16],
    active_start_frame: usize,
    non_silent_end_frame: usize,
) -> Vec<i16> {
    let total_frames = stereo_pcm.len() / CHANNELS;
    let s = active_start_frame.min(total_frames.saturating_sub(1));
    let e = non_silent_end_frame.min(total_frames).max(s + 1);
    let frames = e - s;
    let mut out = stereo_pcm[(s * CHANNELS)..(e * CHANNELS)].to_vec();

    let fade = XFADE_FRAMES.min(frames / 4);
    if fade > 0 {
        for k in 0..fade {
            let gain_in = (k as f32) / (fade as f32);
            let idx_in = k * CHANNELS;
            out[idx_in] = ((out[idx_in] as f32) * gain_in).round() as i16;
            out[idx_in + 1] = ((out[idx_in + 1] as f32) * gain_in).round() as i16;

            let gain_out = ((fade - k) as f32) / (fade as f32);
            let idx_out = (frames - fade + k) * CHANNELS;
            out[idx_out] = ((out[idx_out] as f32) * gain_out).round() as i16;
            out[idx_out + 1] = ((out[idx_out + 1] as f32) * gain_out).round() as i16;
        }
    }
    out
}

pub fn write_wav_s16le_stereo(path: &Path, samples: &[i16], sample_rate: u32) -> Result<()> {
    let data_bytes = (samples.len() * 2) as u32;
    let riff_size = 36u32.saturating_add(data_bytes);
    let byte_rate = sample_rate * (CHANNELS as u32) * 2;
    let block_align = (CHANNELS as u16) * 2;

    let mut buf = Vec::with_capacity(44 + data_bytes as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    buf.extend_from_slice(&(CHANNELS as u16).to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());

    for &s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }

    fs::write(path, &buf).with_context(|| format!("No se pudo escribir WAV {}", path.display()))
}

fn escape_ffconcat(path: &Path) -> String {
    let mut s = path.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string();
    }
    s.replace('\\', "/").replace('\'', r"'\''")
}

pub fn prepare_seamless_loop_concat(
    ffmpeg: &Path,
    audio_path: &Path,
    time_range: Option<(f64, Option<f64>)>,
    target_secs: f64,
) -> Result<PreparedLoopConcat> {
    let stereo_pcm = decode_audio_stereo_s16le(ffmpeg, audio_path, time_range)?;
    let detection = detect_loop_points_from_pcm(&stereo_pcm, SAMPLE_RATE);

    let seq = LOOP_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let tmp_dir = std::env::temp_dir();

    let concat_list_path = tmp_dir.join(format!("mp3video_loop_{}_{}_{}.txt", pid, nanos, seq));
    let mut temp_files = Vec::new();
    let mut concat = String::from("ffconcat version 1.0\n");

    if detection.is_seamless_internal {
        let intro_samples = &stereo_pcm[..(detection.loop_start_frame * CHANNELS)];
        let loop_samples = build_crossfaded_loop_unit(
            &stereo_pcm,
            detection.loop_start_frame,
            detection.loop_end_frame,
        );

        let intro_path =
            tmp_dir.join(format!("mp3video_intro_{}_{}_{}.wav", pid, nanos, seq));
        let unit_path =
            tmp_dir.join(format!("mp3video_unit_{}_{}_{}.wav", pid, nanos, seq));

        write_wav_s16le_stereo(&intro_path, intro_samples, SAMPLE_RATE)?;
        write_wav_s16le_stereo(&unit_path, &loop_samples, SAMPLE_RATE)?;

        temp_files.push(intro_path.clone());
        temp_files.push(unit_path.clone());

        let loop_dur = (detection.loop_end_secs - detection.loop_start_secs).max(0.5);
        let needed = (target_secs + 5.0 - detection.loop_start_secs).max(1.0);
        let repeats = (needed / loop_dur).ceil() as usize + 1;

        concat.push_str(&format!("file '{}'\n", escape_ffconcat(&intro_path)));
        let unit_esc = escape_ffconcat(&unit_path);
        for _ in 0..repeats {
            concat.push_str(&format!("file '{}'\n", unit_esc));
        }
    } else {
        let unit_samples = build_trimmed_single_pass_loop_unit(
            &stereo_pcm,
            detection.active_start_frame,
            detection.non_silent_end_frame,
        );
        let unit_path =
            tmp_dir.join(format!("mp3video_unit_{}_{}_{}.wav", pid, nanos, seq));
        write_wav_s16le_stereo(&unit_path, &unit_samples, SAMPLE_RATE)?;
        temp_files.push(unit_path.clone());

        let unit_dur =
            ((unit_samples.len() / CHANNELS) as f64 / (SAMPLE_RATE as f64)).max(0.5);
        let repeats = ((target_secs + 5.0) / unit_dur).ceil() as usize + 1;
        let unit_esc = escape_ffconcat(&unit_path);
        for _ in 0..repeats {
            concat.push_str(&format!("file '{}'\n", unit_esc));
        }
    }

    fs::write(&concat_list_path, concat.as_bytes()).with_context(|| {
        for p in &temp_files {
            let _ = fs::remove_file(p);
        }
        format!(
            "No se pudo escribir concat list para loop: {}",
            concat_list_path.display()
        )
    })?;

    Ok(PreparedLoopConcat {
        concat_list_path,
        temp_files,
        detection,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_chord(t: f64, freqs: &[f64], amp: f64) -> f64 {
        let mut s = 0.0;
        for &f in freqs {
            s += (2.0 * std::f64::consts::PI * f * t).sin();
        }
        amp * s / (freqs.len().max(1) as f64)
    }

    fn synth_vgm_pattern(t_rel: f64) -> (i16, i16) {
        // 6.0-second distinct harmonic progression (4 x 1.5s chords + bright melody harmonics)
        let phase = t_rel.rem_euclid(6.0);
        let chord: &[f64] = if phase < 1.5 {
            &[261.63, 329.63, 392.00, 523.25] // C major
        } else if phase < 3.0 {
            &[293.66, 349.23, 440.00, 587.33] // D minor
        } else if phase < 4.5 {
            &[246.94, 392.00, 493.88, 659.25] // G6/E
        } else {
            &[220.00, 261.63, 329.63, 440.00] // A minor
        };
        let beat = ((t_rel * 4.0).fract() * std::f64::consts::PI).sin();
        let l = synth_chord(t_rel, chord, 0.55) + 0.15 * beat * (2.0 * std::f64::consts::PI * 880.0 * t_rel).sin();
        let r = synth_chord(t_rel + 0.0005, chord, 0.55) + 0.15 * beat * (2.0 * std::f64::consts::PI * 660.0 * t_rel).sin();
        let li = (l * 24000.0).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        let ri = (r * 24000.0).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        (li, ri)
    }

    #[test]
    fn test_detect_seamless_loop_on_synthetic_vgm_track() {
        // Structure: [Intro 0..2s] + [Loop 1: 2..8s (6s)] + [Loop 2: 8..14s (6s)] + [Fade 14..16s]
        let sr = SAMPLE_RATE as usize;
        let total_frames = 16 * sr;
        let mut pcm = Vec::with_capacity(total_frames * CHANNELS);

        for i in 0..total_frames {
            let t = (i as f64) / (sr as f64);
            let (l, r) = if t < 2.0 {
                // Distinct low F#dim intro
                let v = synth_chord(t, &[185.00, 220.00, 261.63, 370.00], 0.5);
                let s = (v * 24000.0) as i16;
                (s, s)
            } else if t < 14.0 {
                synth_vgm_pattern(t - 2.0)
            } else {
                let fade = ((16.0 - t) / 2.0).clamp(0.0, 1.0);
                let (l0, r0) = synth_vgm_pattern(t - 2.0);
                (((l0 as f64) * fade) as i16, ((r0 as f64) * fade) as i16)
            };
            pcm.push(l);
            pcm.push(r);
        }

        let det = detect_loop_points_from_pcm(&pcm, SAMPLE_RATE);
        assert!(
            det.is_seamless_internal,
            "Expected seamless loop detection, got {:?}",
            det
        );
        assert!(
            det.confidence >= 0.85,
            "Expected confidence >= 0.85, got {}",
            det.confidence
        );
        let period = det.loop_end_secs - det.loop_start_secs;
        assert!(
            (period - 6.0).abs() <= 0.03,
            "Expected 6.0s loop period, got {:.4}s (start={:.4}, end={:.4})",
            period,
            det.loop_start_secs,
            det.loop_end_secs
        );
        assert!(
            det.loop_start_secs >= 1.95 && det.loop_start_secs <= 3.5,
            "Expected loop_start near intro boundary (~2.0s), got {:.4}s",
            det.loop_start_secs
        );
    }

    #[test]
    fn test_single_pass_fallback_when_no_repeat() {
        // Distinct 2-second through-composed sections with unrelated chords/keys and no repetition
        let sr = SAMPLE_RATE as usize;
        let chords: &[&[f64]] = &[
            &[130.81, 196.00, 329.63],       // C3 G3 E4
            &[155.56, 233.08, 415.30],       // Eb3 Bb3 Ab4
            &[185.00, 277.18, 493.88, 739.99], // F#3 C#4 B4 F#5
            &[146.83, 207.65, 349.23, 554.37], // D3 G#3 F4 C#5
            &[246.94, 311.13, 466.16, 622.25], // B3 D#4 Bb4 Eb5
            &[164.81, 246.94, 392.00, 659.25], // E3 B3 G4 E5
        ];
        let total_frames = chords.len() * 2 * sr;
        let mut pcm = Vec::with_capacity(total_frames * CHANNELS);

        for i in 0..total_frames {
            let t = (i as f64) / (sr as f64);
            let sec = ((t / 2.0) as usize).min(chords.len() - 1);
            let v = synth_chord(t, chords[sec], 0.55) * 20000.0;
            let s = v.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            pcm.push(s);
            pcm.push(s);
        }

        let det = detect_loop_points_from_pcm(&pcm, SAMPLE_RATE);
        assert!(
            !det.is_seamless_internal,
            "Expected single-pass fallback on non-repeating track, got {:?}",
            det
        );
        assert!(det.loop_end_secs > det.loop_start_secs);
    }

    #[test]
    fn test_loop_unit_crossfade_has_zero_boundary_step() {
        // Pure periodic sine wave at 441 Hz (exact 100 samples per period at 44100 Hz)
        let sr = SAMPLE_RATE as usize;
        let freq = 441.0;
        let total_frames = 6 * sr;
        let mut pcm = Vec::with_capacity(total_frames * CHANNELS);
        for i in 0..total_frames {
            let t = (i as f64) / (sr as f64);
            let v = (2.0 * std::f64::consts::PI * freq * t).sin() * 20000.0;
            let s = v.round() as i16;
            pcm.push(s);
            pcm.push(s);
        }

        // Loop period of 2.0s = 88200 frames (882 exact periods of 441 Hz)
        let start_frame = sr; // 1.0s
        let end_frame = 3 * sr; // 3.0s
        let unit = build_crossfaded_loop_unit(&pcm, start_frame, end_frame);
        let unit_frames = unit.len() / CHANNELS;
        assert_eq!(unit_frames, end_frame - start_frame);

        let expected_step = (pcm[start_frame * 2] as i32 - pcm[(start_frame - 1) * 2] as i32).abs();
        let actual_step = (unit[0] as i32 - unit[(unit_frames - 1) * 2] as i32).abs();
        assert!(
            (actual_step - expected_step).abs() <= 5,
            "Expected smooth wrap-around step (~{}), got {} (unit[0]={}, unit[end]={})",
            expected_step,
            actual_step,
            unit[0],
            unit[(unit_frames - 1) * 2]
        );
    }

    #[test]
    fn test_detect_seamless_loop_with_long_sustained_chords() {
        // [Intro 2s] + [Loop 1: 2.5s C + 2.5s Dm] + [Loop 2: 2.5s C + 2.5s Dm] + [Fade 2s]
        let sr = SAMPLE_RATE as usize;
        let total_frames = 14 * sr;
        let mut pcm = Vec::with_capacity(total_frames * CHANNELS);

        for i in 0..total_frames {
            let t = (i as f64) / (sr as f64);
            let sample = if t < 2.0 {
                synth_chord(t, &[185.0, 220.0], 0.5)
            } else {
                let phase = (t - 2.0).rem_euclid(5.0);
                let base = if phase < 2.5 {
                    synth_chord(phase, &[261.63, 329.63, 523.25], 0.5)
                } else {
                    synth_chord(phase - 2.5, &[293.66, 349.23, 587.33], 0.5)
                };
                if t >= 12.0 {
                    base * ((14.0 - t) / 2.0).clamp(0.0, 1.0)
                } else {
                    base
                }
            };
            let s = (sample * 24000.0).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            pcm.push(s);
            pcm.push(s);
        }

        let det = detect_loop_points_from_pcm(&pcm, SAMPLE_RATE);
        assert!(det.is_seamless_internal, "Expected seamless loop, got {:?}", det);
        let period = det.loop_end_secs - det.loop_start_secs;
        assert!(
            (period - 5.0).abs() <= 0.03,
            "Expected exact 5.0s loop period, got {:.4}s (start={:.4}, end={:.4})",
            period,
            det.loop_start_secs,
            det.loop_end_secs
        );
    }
}
