use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{has_extension, new_hidden_command};

pub(crate) const SINGLE_IMAGE_AUDIO_EXTS: &[&str] = &["flac", "wav", "ape", "wv", "m4a", "mp3"];
pub(crate) const CUE_AUDIO_EXTS: &[&str] = &["flac", "wav", "ape", "wv"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CueTrack {
    pub track_number: u32,
    pub title: String,
    pub performer: Option<String>,
    pub start_secs: f64,
    pub end_secs: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CueSheet {
    pub cue_path: PathBuf,
    pub audio_path: PathBuf,
    pub album_title: Option<String>,
    pub performer: Option<String>,
    pub catalog: Option<String>,
    pub date: Option<String>,
    pub tracks: Vec<CueTrack>,
}

fn decode_cue_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.trim_start_matches('\u{feff}').to_string();
    }
    // Respaldo para .cue guardados en ANSI / Windows-1252
    bytes
        .iter()
        .map(|&b| match b {
            0x80 => '\u{20AC}',
            0x82 => '\u{201A}',
            0x84 => '\u{201E}',
            0x85 => '\u{2026}',
            0x91 => '\u{2018}',
            0x92 => '\u{2019}',
            0x93 => '\u{201C}',
            0x94 => '\u{201D}',
            0x96 => '\u{2013}',
            0x97 => '\u{2014}',
            _ => b as char,
        })
        .collect()
}

fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_file_directive(rest: &str) -> String {
    let trimmed = rest.trim();
    if let Some(after_open) = trimmed.strip_prefix('"') {
        if let Some(end_quote) = after_open.find('"') {
            return after_open[..end_quote].trim().to_string();
        }
    }
    trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

fn parse_cue_index_time(raw: &str) -> Option<f64> {
    let mut parts = raw.trim().split(':');
    let mm: f64 = parts.next()?.trim().parse().ok()?;
    let ss: f64 = parts.next()?.trim().parse().ok()?;
    let ff: f64 = parts.next()?.trim().parse().ok()?;
    Some(mm * 60.0 + ss + (ff / 75.0))
}

fn resolve_audio_file(cue_path: &Path, file_ref: Option<&str>) -> Result<PathBuf> {
    let cue_dir = cue_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if let Some(ref_str) = file_ref {
        let candidate_name = Path::new(ref_str)
            .file_name()
            .unwrap_or_else(|| OsStr::new(ref_str));
        let direct = cue_dir.join(candidate_name);
        if direct.is_file() {
            return Ok(direct);
        }

        if let Some(stem) = Path::new(candidate_name).file_stem().and_then(OsStr::to_str) {
            for ext in SINGLE_IMAGE_AUDIO_EXTS {
                let alt = cue_dir.join(format!("{}.{}", stem, ext));
                if alt.is_file() {
                    return Ok(alt);
                }
            }
        }
    }

    if let Some(cue_stem) = cue_path.file_stem().and_then(OsStr::to_str) {
        for ext in SINGLE_IMAGE_AUDIO_EXTS {
            let alt = cue_dir.join(format!("{}.{}", cue_stem, ext));
            if alt.is_file() {
                return Ok(alt);
            }
        }
    }

    // Respaldo: si hay un único archivo de audio lossless en la carpeta
    if let Ok(entries) = fs::read_dir(cue_dir) {
        let mut found: Option<PathBuf> = None;
        let mut multiple = false;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && has_extension(&p, CUE_AUDIO_EXTS) {
                if found.is_some() {
                    multiple = true;
                    break;
                }
                found = Some(p);
            }
        }
        if !multiple {
            if let Some(p) = found {
                return Ok(p);
            }
        }
    }

    bail!(
        "No se encontró el archivo de audio asociado al CUE '{}'{}.",
        cue_path.display(),
        file_ref
            .map(|r| format!(" (referenciado como '{}')", r))
            .unwrap_or_default()
    );
}

pub(crate) fn resolve_cue_for_input(path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    if has_extension(path, &["cue"]) {
        return Some(path.to_path_buf());
    }
    if !has_extension(path, CUE_AUDIO_EXTS) {
        return None;
    }

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if let Some(stem) = path.file_stem().and_then(OsStr::to_str) {
        let same_stem = dir.join(format!("{}.cue", stem));
        if same_stem.is_file() {
            return Some(same_stem);
        }
    }
    if let Some(fname) = path.file_name().and_then(OsStr::to_str) {
        let appended = dir.join(format!("{}.cue", fname));
        if appended.is_file() {
            return Some(appended);
        }
    }

    if let Ok(entries) = fs::read_dir(dir) {
        let mut found: Option<PathBuf> = None;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && has_extension(&p, &["cue"]) {
                if found.is_some() {
                    return None;
                }
                found = Some(p);
            }
        }
        return found;
    }

    None
}

pub(crate) fn parse_cue_file(cue_path: &Path) -> Result<CueSheet> {
    let bytes = fs::read(cue_path)
        .with_context(|| format!("No se pudo leer el archivo CUE: {}", cue_path.display()))?;
    let content = decode_cue_bytes(&bytes);

    let mut album_title: Option<String> = None;
    let mut album_performer: Option<String> = None;
    let mut catalog: Option<String> = None;
    let mut date: Option<String> = None;
    let mut file_ref: Option<String> = None;

    struct PendingTrack {
        track_number: u32,
        title: Option<String>,
        performer: Option<String>,
        index_01: Option<f64>,
        index_00: Option<f64>,
    }

    let mut raw_tracks: Vec<PendingTrack> = Vec::new();
    let mut current_track: Option<PendingTrack> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let (cmd, rest) = match line.split_once(char::is_whitespace) {
            Some((c, r)) => (c.trim(), r.trim()),
            None => (line, ""),
        };

        match cmd.to_ascii_uppercase().as_str() {
            "REM" => {
                if let Some((rem_key, rem_val)) = rest.split_once(char::is_whitespace) {
                    match rem_key.trim().to_ascii_uppercase().as_str() {
                        "DATE" if date.is_none() => {
                            let v = unquote(rem_val);
                            if !v.is_empty() {
                                date = Some(v);
                            }
                        }
                        "CATALOG" if catalog.is_none() => {
                            let v = unquote(rem_val);
                            if !v.is_empty() {
                                catalog = Some(v);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "CATALOG" => {
                let v = unquote(rest);
                if !v.is_empty() {
                    catalog = Some(v);
                }
            }
            "FILE" => {
                if file_ref.is_none() {
                    let parsed = parse_file_directive(rest);
                    if !parsed.is_empty() {
                        file_ref = Some(parsed);
                    }
                }
            }
            "TRACK" => {
                if let Some(prev) = current_track.take() {
                    raw_tracks.push(prev);
                }
                let num = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or((raw_tracks.len() + 1) as u32);
                current_track = Some(PendingTrack {
                    track_number: num,
                    title: None,
                    performer: None,
                    index_01: None,
                    index_00: None,
                });
            }
            "TITLE" => {
                let v = unquote(rest);
                if !v.is_empty() {
                    if let Some(ref mut tr) = current_track {
                        tr.title = Some(v);
                    } else if album_title.is_none() {
                        album_title = Some(v);
                    }
                }
            }
            "PERFORMER" => {
                let v = unquote(rest);
                if !v.is_empty() {
                    if let Some(ref mut tr) = current_track {
                        tr.performer = Some(v);
                    } else if album_performer.is_none() {
                        album_performer = Some(v);
                    }
                }
            }
            "INDEX" => {
                if let Some(ref mut tr) = current_track {
                    let mut parts = rest.split_whitespace();
                    let idx_kind = parts.next().unwrap_or("");
                    let time_str = parts.next().unwrap_or("");
                    if let Some(secs) = parse_cue_index_time(time_str) {
                        if idx_kind == "01" {
                            tr.index_01 = Some(secs);
                        } else if idx_kind == "00" && tr.index_00.is_none() {
                            tr.index_00 = Some(secs);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(last) = current_track.take() {
        raw_tracks.push(last);
    }

    if raw_tracks.is_empty() {
        bail!("El archivo CUE '{}' no contiene pistas (TRACK).", cue_path.display());
    }

    let mut tracks: Vec<CueTrack> = raw_tracks
        .into_iter()
        .filter_map(|t| {
            let start_secs = t.index_01.or(t.index_00)?;
            let title = t
                .title
                .unwrap_or_else(|| format!("Track {:02}", t.track_number));
            Some(CueTrack {
                track_number: t.track_number,
                title,
                performer: t.performer,
                start_secs,
                end_secs: None,
            })
        })
        .collect();

    if tracks.is_empty() {
        bail!(
            "El archivo CUE '{}' no contiene marcas de tiempo válidas (INDEX 01).",
            cue_path.display()
        );
    }

    for i in 0..tracks.len().saturating_sub(1) {
        let next_start = tracks[i + 1].start_secs;
        if next_start > tracks[i].start_secs {
            tracks[i].end_secs = Some(next_start);
        }
    }

    let audio_path = resolve_audio_file(cue_path, file_ref.as_deref())?;

    Ok(CueSheet {
        cue_path: cue_path.to_path_buf(),
        audio_path,
        album_title,
        performer: album_performer,
        catalog,
        date,
        tracks,
    })
}

pub(crate) fn format_timestamp(secs: f64) -> String {
    let clamped = secs.max(0.0);
    let mins = (clamped / 60.0).floor() as u32;
    let rem = clamped - (mins as f64 * 60.0);
    format!("{:02}:{:05.2}", mins, rem)
}

fn parse_timestamp(raw: &str) -> Result<f64> {
    let s = raw.trim();
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => s
            .parse::<f64>()
            .with_context(|| format!("Tiempo inválido '{}'", s)),
        2 => {
            let mins: f64 = parts[0]
                .trim()
                .parse()
                .with_context(|| format!("Minutos inválidos en '{}'", s))?;
            let secs: f64 = parts[1]
                .trim()
                .parse()
                .with_context(|| format!("Segundos inválidos en '{}'", s))?;
            Ok(mins * 60.0 + secs)
        }
        3 => {
            let hrs: f64 = parts[0]
                .trim()
                .parse()
                .with_context(|| format!("Horas inválidas en '{}'", s))?;
            let mins: f64 = parts[1]
                .trim()
                .parse()
                .with_context(|| format!("Minutos inválidos en '{}'", s))?;
            let secs: f64 = parts[2]
                .trim()
                .parse()
                .with_context(|| format!("Segundos inválidos en '{}'", s))?;
            Ok(hrs * 3600.0 + mins * 60.0 + secs)
        }
        _ => bail!("Formato de tiempo inválido '{}'", s),
    }
}

pub(crate) fn format_tracks_for_review(tracks: &[CueTrack]) -> String {
    tracks
        .iter()
        .map(|tr| {
            let start = format_timestamp(tr.start_secs);
            let end = match tr.end_secs {
                Some(e) => format_timestamp(e),
                None => "FIN     ".to_string(),
            };
            format!("{:02} | {} - {} | {}", tr.track_number, start, end, tr.title)
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

pub(crate) fn parse_tracks_from_review(text: &str, original: &[CueTrack]) -> Result<Vec<CueTrack>> {
    let mut result = Vec::new();

    for (line_idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, '|').collect();
        if parts.len() != 3 {
            bail!(
                "Formato inválido en línea {} de pistas: '{}'. Debe respetar: NN | MM:SS.CC - MM:SS.CC | Título",
                line_idx + 1,
                line
            );
        }

        let track_number: u32 = parts[0]
            .trim()
            .parse()
            .with_context(|| format!("Número de pista inválido en línea {}: '{}'", line_idx + 1, parts[0].trim()))?;

        let (start_str, end_str) = parts[1]
            .split_once('-')
            .with_context(|| format!("Rango de tiempo inválido en línea {}: '{}'", line_idx + 1, parts[1]))?;

        let start_secs = parse_timestamp(start_str)?;
        let end_trimmed = end_str.trim();
        let end_secs = if end_trimmed.eq_ignore_ascii_case("FIN")
            || end_trimmed.eq_ignore_ascii_case("END")
            || end_trimmed.is_empty()
        {
            None
        } else {
            let e = parse_timestamp(end_trimmed)?;
            if e <= start_secs {
                bail!(
                    "El tiempo final debe ser mayor al inicial en pista {:02}.",
                    track_number
                );
            }
            Some(e)
        };

        let title = parts[2].trim();
        let final_title = if title.is_empty() {
            format!("Track {:02}", track_number)
        } else {
            title.to_string()
        };

        let performer = original
            .iter()
            .find(|t| t.track_number == track_number)
            .and_then(|t| t.performer.clone());

        result.push(CueTrack {
            track_number,
            title: final_title,
            performer,
            start_secs,
            end_secs,
        });
    }

    if result.is_empty() {
        bail!("No hay pistas seleccionadas en la lista de revisión del CUE.");
    }

    Ok(result)
}

pub(crate) fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => ' ',
            other => other,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "Track".to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn cue_track_output_stem(track: &CueTrack) -> String {
    format!("{:02} - {}", track.track_number, sanitize_filename(&track.title))
}

pub(crate) fn split_cue_to_mp3<FLog, FProg>(
    cue: &CueSheet,
    tracks: &[CueTrack],
    output_dir: &Path,
    album_override: Option<&str>,
    artist_override: Option<&str>,
    ffmpeg: &Path,
    jobs: usize,
    overwrite: bool,
    cancel_flag: Option<&AtomicBool>,
    mut on_log: FLog,
    mut on_progress: FProg,
) -> Result<(usize, usize, usize)>
where
    FLog: FnMut(&str) + Send,
    FProg: FnMut(usize, usize, &str) + Send,
{
    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "No se pudo crear la carpeta de salida: {}",
            output_dir.display()
        )
    })?;

    let mut pending = Vec::new();
    let mut skipped_count = 0usize;

    for tr in tracks {
        let stem = cue_track_output_stem(tr);
        let out_mp3 = output_dir.join(format!("{}.mp3", stem));
        if out_mp3.exists() && !overwrite {
            skipped_count += 1;
            continue;
        }
        pending.push((tr, out_mp3));
    }

    if skipped_count > 0 {
        on_log(&format!(
            "[CUE -> MP3] Omitidos por ya existir: {}",
            skipped_count
        ));
    }

    let total = pending.len();
    if total == 0 {
        return Ok((0, skipped_count, 0));
    }

    let album_meta = album_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cue.album_title.clone());
    let default_artist = artist_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cue.performer.clone());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()
        .context("No se pudo iniciar el pool de hilos para dividir MP3")?;

    let ok_count = AtomicUsize::new(0);
    let err_count = AtomicUsize::new(0);
    let done_count = AtomicUsize::new(0);
    let log_mutex = std::sync::Mutex::new(&mut on_log);
    let prog_mutex = std::sync::Mutex::new(&mut on_progress);

    pool.install(|| {
        pending.par_iter().for_each(|(tr, out_mp3)| {
            if let Some(flag) = cancel_flag {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
            }

            let file_name = out_mp3
                .file_name()
                .unwrap_or_else(|| OsStr::new("track.mp3"))
                .to_string_lossy()
                .to_string();

            if let Ok(mut p) = prog_mutex.lock() {
                p(done_count.load(Ordering::Relaxed), total, &format!("Extrayendo: {}", file_name));
            }

            let temp_mp3 = out_mp3.with_file_name(format!(".{}.part.mp3", file_name));
            let mut cmd = new_hidden_command(ffmpeg);
            cmd.arg("-hide_banner")
                .arg("-loglevel")
                .arg("error")
                .arg("-y")
                .arg("-ss")
                .arg(format!("{:.3}", tr.start_secs));

            if let Some(end) = tr.end_secs {
                let dur = (end - tr.start_secs).max(0.1);
                cmd.arg("-t").arg(format!("{:.3}", dur));
            }

            cmd.arg("-i")
                .arg(&cue.audio_path)
                .arg("-vn")
                .arg("-c:a")
                .arg("libmp3lame")
                .arg("-b:a")
                .arg("320k")
                .arg("-id3v2_version")
                .arg("3")
                .arg("-metadata")
                .arg(format!("title={}", tr.title))
                .arg("-metadata")
                .arg(format!("track={}", tr.track_number));

            if let Some(ref alb) = album_meta {
                cmd.arg("-metadata").arg(format!("album={}", alb));
            }
            if let Some(art) = tr.performer.as_deref().or(default_artist.as_deref()) {
                cmd.arg("-metadata").arg(format!("artist={}", art));
            }
            if let Some(ref d) = cue.date {
                cmd.arg("-metadata").arg(format!("date={}", d));
            }

            cmd.arg(&temp_mp3);

            let res = cmd.output();
            match res {
                Ok(out) if out.status.success() => {
                    if out_mp3.exists() {
                        let _ = fs::remove_file(out_mp3);
                    }
                    match fs::rename(&temp_mp3, out_mp3) {
                        Ok(()) => {
                            ok_count.fetch_add(1, Ordering::Relaxed);
                            if let Ok(mut l) = log_mutex.lock() {
                                l(&format!("[MP3 OK] {}", file_name));
                            }
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&temp_mp3);
                            err_count.fetch_add(1, Ordering::Relaxed);
                            if let Ok(mut l) = log_mutex.lock() {
                                l(&format!("[MP3 ERROR] {}: {}", file_name, e));
                            }
                        }
                    }
                }
                Ok(out) => {
                    let _ = fs::remove_file(&temp_mp3);
                    err_count.fetch_add(1, Ordering::Relaxed);
                    let err_msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if let Ok(mut l) = log_mutex.lock() {
                        l(&format!("[MP3 ERROR] {}: {}", file_name, err_msg));
                    }
                }
                Err(e) => {
                    let _ = fs::remove_file(&temp_mp3);
                    err_count.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut l) = log_mutex.lock() {
                        l(&format!("[MP3 ERROR] {}: {}", file_name, e));
                    }
                }
            }

            let done = done_count.fetch_add(1, Ordering::Relaxed) + 1;
            if let Ok(mut p) = prog_mutex.lock() {
                p(done, total, &file_name);
            }
        });
    });

    Ok((
        ok_count.load(Ordering::Relaxed),
        skipped_count,
        err_count.load(Ordering::Relaxed),
    ))
}
