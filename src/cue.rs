use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::vgmdb::{match_track_for_file, read_mp3_tags, AlbumMetadata};
use crate::{
    collect_audio_files, find_ffprobe, has_extension, new_hidden_command, AUDIO_EXTENSIONS,
};

pub(crate) const SINGLE_IMAGE_AUDIO_EXTS: &[&str] = AUDIO_EXTENSIONS;
pub(crate) const CUE_AUDIO_EXTS: &[&str] = AUDIO_EXTENSIONS;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CueTrack {
    pub track_number: u32,
    pub title: String,
    pub performer: Option<String>,
    pub start_secs: f64,
    pub end_secs: Option<f64>,
    #[serde(default)]
    pub source_audio: Option<PathBuf>,
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

#[derive(Clone, Debug)]
pub(crate) struct ChapterEntry {
    pub title: String,
    pub start_secs: f64,
    pub end_secs: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct TimelinePlan {
    pub single_direct_audio: Option<PathBuf>,
    pub chapters: Vec<ChapterEntry>,
    pub total_duration_secs: f64,
}

fn decode_cue_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.trim_start_matches('\u{feff}').to_string();
    }
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

pub(crate) fn parse_cue_content(
    content: &str,
    cue_path: &Path,
    audio_override: Option<&Path>,
) -> Result<CueSheet> {
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

    let audio_path = if let Some(audio_ovr) = audio_override {
        audio_ovr.to_path_buf()
    } else {
        resolve_audio_file(cue_path, file_ref.as_deref())?
    };

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
                source_audio: Some(audio_path.clone()),
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

pub(crate) fn parse_cue_file(cue_path: &Path) -> Result<CueSheet> {
    let bytes = fs::read(cue_path)
        .with_context(|| format!("No se pudo leer el archivo CUE: {}", cue_path.display()))?;
    let content = decode_cue_bytes(&bytes);
    parse_cue_content(&content, cue_path, None)
}

pub(crate) fn try_extract_embedded_cuesheet(
    audio_path: &Path,
    ffmpeg_path: Option<&Path>,
) -> Option<CueSheet> {
    let ffprobe = find_ffprobe(ffmpeg_path)?;
    let out = new_hidden_command(&ffprobe)
        .arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg(audio_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let tags = json.get("format")?.get("tags")?.as_object()?;
    for (k, v) in tags {
        if k.eq_ignore_ascii_case("cuesheet") {
            if let Some(cue_text) = v.as_str() {
                if let Ok(sheet) = parse_cue_content(cue_text, audio_path, Some(audio_path)) {
                    return Some(sheet);
                }
            }
        }
    }
    None
}

pub(crate) fn try_extract_chapters_as_cue(
    audio_path: &Path,
    ffmpeg_path: Option<&Path>,
) -> Option<CueSheet> {
    let ffprobe = find_ffprobe(ffmpeg_path)?;
    let out = new_hidden_command(&ffprobe)
        .arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_chapters")
        .arg(audio_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let chapters = json.get("chapters")?.as_array()?;
    if chapters.is_empty() {
        return None;
    }
    let mut tracks = Vec::new();
    for (idx, ch) in chapters.iter().enumerate() {
        let start: f64 = ch.get("start_time")?.as_str()?.parse().ok()?;
        let end: Option<f64> = ch
            .get("end_time")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok());
        let title = ch
            .get("tags")
            .and_then(|t| t.get("title"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("Track {:02}", idx + 1));
        tracks.push(CueTrack {
            track_number: (idx + 1) as u32,
            title,
            performer: None,
            start_secs: start,
            end_secs: end,
            source_audio: Some(audio_path.to_path_buf()),
        });
    }
    if tracks.is_empty() {
        return None;
    }
    Some(CueSheet {
        cue_path: audio_path.to_path_buf(),
        audio_path: audio_path.to_path_buf(),
        album_title: None,
        performer: None,
        catalog: None,
        date: None,
        tracks,
    })
}

pub(crate) fn build_cue_from_single_audio_file(
    path: &Path,
    album: Option<&AlbumMetadata>,
    ffmpeg_path: Option<&Path>,
) -> Result<CueSheet> {
    if let Some(cue) = try_extract_embedded_cuesheet(path, ffmpeg_path) {
        return Ok(cue);
    }
    if let Some(cue) = try_extract_chapters_as_cue(path, ffmpeg_path) {
        return Ok(cue);
    }
    let id3 = read_mp3_tags(path, ffmpeg_path);
    let matched = match_track_for_file(path, 0, album, &id3);
    let title = if album.is_some() || id3.title.is_some() {
        matched.track_title
    } else {
        let stem = path
            .file_stem()
            .unwrap_or_else(|| OsStr::new("Track"))
            .to_string_lossy();
        clean_mp3_stem_title(&stem)
    };
    let dur = probe_audio_duration(path, ffmpeg_path);
    let track = CueTrack {
        track_number: 1,
        title,
        performer: id3.composer.or(id3.artist),
        start_secs: 0.0,
        end_secs: dur,
        source_audio: Some(path.to_path_buf()),
    };
    Ok(CueSheet {
        cue_path: path.to_path_buf(),
        audio_path: path.to_path_buf(),
        album_title: album
            .map(|a| a.album_title.clone())
            .filter(|s| !s.is_empty())
            .or(id3.album),
        performer: album
            .map(|a| a.composers.join(", "))
            .filter(|s| !s.is_empty()),
        catalog: album
            .map(|a| a.catalog.clone())
            .filter(|s| !s.is_empty()),
        date: album
            .map(|a| a.release_date.clone())
            .filter(|s| !s.is_empty())
            .or(id3.date),
        tracks: vec![track],
    })
}

pub(crate) fn probe_audio_duration(audio_path: &Path, ffmpeg_path: Option<&Path>) -> Option<f64> {
    let ffprobe = find_ffprobe(ffmpeg_path)?;
    let out = new_hidden_command(&ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(audio_path)
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|&d| d.is_finite() && d > 0.0)
}

fn clean_mp3_stem_title(stem: &str) -> String {
    let trimmed = stem.trim();
    let bytes = trimmed.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx > 0 && idx < bytes.len() {
        let rest = trimmed[idx..].trim_start_matches([' ', '-', '.', '_']);
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    trimmed.to_string()
}

pub(crate) fn build_tracks_from_directory(
    dir: &Path,
    output_dir: &Path,
    recursive: bool,
    album: Option<&AlbumMetadata>,
    ffmpeg_path: Option<&Path>,
) -> Result<CueSheet> {
    if !dir.is_dir() {
        bail!("La ruta '{}' no es una carpeta.", dir.display());
    }

    let max_depth = if recursive { usize::MAX } else { 1 };
    let mut cue_files = Vec::new();
    for entry in WalkDir::new(dir)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        let p = entry.path();
        if p.is_file() && has_extension(p, &["cue"]) {
            cue_files.push(p.to_path_buf());
        }
    }
    cue_files.sort();

    if !cue_files.is_empty() {
        let mut first_sheet: Option<CueSheet> = None;
        let mut combined_tracks = Vec::new();
        let mut seq_num = 1u32;

        for cue_path in &cue_files {
            let sheet = parse_cue_file(cue_path)?;
            if first_sheet.is_none() {
                first_sheet = Some(sheet.clone());
            }
            for mut tr in sheet.tracks {
                tr.track_number = seq_num;
                seq_num += 1;
                if tr.source_audio.is_none() {
                    tr.source_audio = Some(sheet.audio_path.clone());
                }
                combined_tracks.push(tr);
            }
        }

        let mut merged = first_sheet.unwrap();
        merged.tracks = combined_tracks;
        return Ok(merged);
    }

    let (_, audio_files) = collect_audio_files(dir, output_dir, recursive)?;
    if audio_files.is_empty() {
        bail!(
            "No se encontraron archivos .cue ni archivos de audio (.flac, .mp3, .wav, etc.) en '{}'.",
            dir.display()
        );
    }

    let mut tracks = Vec::with_capacity(audio_files.len());
    let mut first_album = None;
    let mut first_artist = None;
    let mut first_date = None;

    for (idx, audio_path) in audio_files.iter().enumerate() {
        let id3 = read_mp3_tags(audio_path, ffmpeg_path);
        if first_album.is_none() {
            first_album = id3.album.clone();
        }
        if first_artist.is_none() {
            first_artist = id3.composer.clone().or_else(|| id3.artist.clone());
        }
        if first_date.is_none() {
            first_date = id3.date.clone();
        }

        let matched = match_track_for_file(audio_path, idx, album, &id3);
        let title = if album.is_some() || id3.title.is_some() {
            matched.track_title
        } else {
            let stem = audio_path
                .file_stem()
                .unwrap_or_else(|| OsStr::new("Track"))
                .to_string_lossy();
            clean_mp3_stem_title(&stem)
        };

        let dur = probe_audio_duration(audio_path, ffmpeg_path);
        tracks.push(CueTrack {
            track_number: (idx + 1) as u32,
            title,
            performer: id3.composer.or(id3.artist),
            start_secs: 0.0,
            end_secs: dur,
            source_audio: Some(audio_path.clone()),
        });
    }

    let fallback_audio = audio_files[0].clone();
    Ok(CueSheet {
        cue_path: dir.to_path_buf(),
        audio_path: fallback_audio,
        album_title: album
            .map(|a| a.album_title.clone())
            .filter(|s| !s.is_empty())
            .or(first_album),
        performer: album
            .map(|a| a.composers.join(", "))
            .filter(|s| !s.is_empty())
            .or(first_artist),
        catalog: album.map(|a| a.catalog.clone()).filter(|s| !s.is_empty()),
        date: album
            .map(|a| a.release_date.clone())
            .filter(|s| !s.is_empty())
            .or(first_date),
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
    let mut count_by_num: HashMap<u32, usize> = HashMap::new();
    for tr in original {
        *count_by_num.entry(tr.track_number).or_insert(0) += 1;
    }

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
            .with_context(|| {
                format!(
                    "Número de pista inválido en línea {}: '{}'",
                    line_idx + 1,
                    parts[0].trim()
                )
            })?;

        let (start_str, end_str) = parts[1].split_once('-').with_context(|| {
            format!(
                "Rango de tiempo inválido en línea {}: '{}'",
                line_idx + 1,
                parts[1]
            )
        })?;

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

        let matched_orig = if count_by_num.get(&track_number).copied().unwrap_or(0) == 1 {
            original.iter().find(|t| t.track_number == track_number)
        } else {
            original
                .get(line_idx)
                .filter(|t| t.track_number == track_number)
                .or_else(|| original.iter().find(|t| t.track_number == track_number))
        };

        result.push(CueTrack {
            track_number,
            title: final_title,
            performer: matched_orig.and_then(|t| t.performer.clone()),
            start_secs,
            end_secs,
            source_audio: matched_orig.and_then(|t| t.source_audio.clone()),
        });
    }

    if result.is_empty() {
        bail!("No hay pistas seleccionadas en la lista de revisión.");
    }

    Ok(result)
}

pub(crate) fn compute_timeline(
    tracks: &[CueTrack],
    fallback_audio: &Path,
    ffmpeg_path: Option<&Path>,
) -> TimelinePlan {
    let mut duration_cache: HashMap<PathBuf, f64> = HashMap::new();
    let mut get_dur = |path: &Path| -> f64 {
        if let Some(&d) = duration_cache.get(path) {
            return d;
        }
        let d = probe_audio_duration(path, ffmpeg_path).unwrap_or(0.0);
        duration_cache.insert(path.to_path_buf(), d);
        d
    };

    let first_audio = tracks
        .first()
        .and_then(|t| t.source_audio.as_deref())
        .unwrap_or(fallback_audio);

    let all_same_file = !tracks.is_empty()
        && tracks.iter().all(|t| {
            t.source_audio.as_deref().unwrap_or(fallback_audio) == first_audio
        });

    let is_contiguous_full = all_same_file
        && tracks[0].start_secs < 2.0
        && tracks.last().is_some_and(|last| last.end_secs.is_none())
        && tracks.windows(2).all(|w| {
            w[0].end_secs
                .is_some_and(|e| (e - w[1].start_secs).abs() < 0.05)
        });

    if is_contiguous_full {
        let total_dur = get_dur(first_audio);
        let mut chapters = Vec::with_capacity(tracks.len());
        for (idx, tr) in tracks.iter().enumerate() {
            let start = if idx == 0 { 0.0 } else { tr.start_secs };
            let end = tr.end_secs.unwrap_or_else(|| {
                if total_dur > start {
                    total_dur
                } else {
                    start + 1.0
                }
            });
            chapters.push(ChapterEntry {
                title: tr.title.clone(),
                start_secs: start,
                end_secs: end.max(start + 0.1),
            });
        }
        let final_dur = chapters.last().map(|c| c.end_secs).unwrap_or(total_dur);
        return TimelinePlan {
            single_direct_audio: Some(first_audio.to_path_buf()),
            chapters,
            total_duration_secs: final_dur,
        };
    }

    let mut chapters = Vec::with_capacity(tracks.len());
    let mut cursor = 0.0f64;

    for tr in tracks {
        let src = tr.source_audio.as_deref().unwrap_or(fallback_audio);
        let clip_dur = match tr.end_secs {
            Some(end) if end > tr.start_secs => end - tr.start_secs,
            _ => {
                let full_dur = get_dur(src);
                if full_dur > tr.start_secs {
                    full_dur - tr.start_secs
                } else {
                    1.0
                }
            }
        }
        .max(0.1);

        let start = cursor;
        let end = cursor + clip_dur;
        cursor = end;
        chapters.push(ChapterEntry {
            title: tr.title.clone(),
            start_secs: start,
            end_secs: end,
        });
    }

    TimelinePlan {
        single_direct_audio: None,
        chapters,
        total_duration_secs: cursor,
    }
}

pub(crate) fn format_youtube_timestamps(plan: &TimelinePlan) -> String {
    let use_hours = plan.total_duration_secs >= 3600.0;
    plan.chapters
        .iter()
        .enumerate()
        .map(|(idx, ch)| {
            let total_s = if idx == 0 {
                0
            } else {
                ch.start_secs.round() as u64
            };
            if use_hours {
                let h = total_s / 3600;
                let m = (total_s % 3600) / 60;
                let s = total_s % 60;
                format!("{:02}:{:02}:{:02} {}", h, m, s, ch.title)
            } else {
                let m = total_s / 60;
                let s = total_s % 60;
                format!("{:02}:{:02} {}", m, s, ch.title)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_ffmetadata(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' | '=' | ';' | '#' => {
                out.push('\\');
                out.push(ch);
            }
            '\r' | '\n' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn write_ffmetadata_chapters(
    path: &Path,
    album_title: Option<&str>,
    chapters: &[ChapterEntry],
) -> Result<()> {
    let mut content = String::from(";FFMETADATA1\n");
    if let Some(alb) = album_title.map(str::trim).filter(|s| !s.is_empty()) {
        content.push_str(&format!("title={}\n", escape_ffmetadata(alb)));
    }
    for ch in chapters {
        let start_ms = (ch.start_secs * 1000.0).round() as u64;
        let end_ms = ((ch.end_secs * 1000.0).round() as u64).max(start_ms + 100);
        content.push_str("[CHAPTER]\nTIMEBASE=1/1000\n");
        content.push_str(&format!("START={}\n", start_ms));
        content.push_str(&format!("END={}\n", end_ms));
        content.push_str(&format!("title={}\n", escape_ffmetadata(&ch.title)));
    }
    fs::write(path, content).with_context(|| format!("No se pudo escribir {}", path.display()))
}

pub(crate) fn write_ffconcat_file(
    path: &Path,
    tracks: &[CueTrack],
    fallback_audio: &Path,
) -> Result<()> {
    let mut content = String::from("ffconcat version 1.0\n");
    for tr in tracks {
        let src = tr.source_audio.as_deref().unwrap_or(fallback_audio);
        let abs = fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
        let mut s = abs.to_string_lossy().to_string();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            s = stripped.to_string();
        }
        let escaped = s.replace('\'', r"'\''");
        content.push_str(&format!("file '{}'\n", escaped));
        if tr.start_secs > 0.0 {
            content.push_str(&format!("inpoint {:.3}\n", tr.start_secs));
        }
        if let Some(end) = tr.end_secs {
            if end > tr.start_secs {
                content.push_str(&format!("outpoint {:.3}\n", end));
            }
        }
    }
    fs::write(path, content).with_context(|| format!("No se pudo escribir {}", path.display()))
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

#[allow(clippy::too_many_arguments)]
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
                p(
                    done_count.load(Ordering::Relaxed),
                    total,
                    &format!("Extrayendo: {}", file_name),
                );
            }

            let src_audio = tr.source_audio.as_deref().unwrap_or(&cue.audio_path);
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
                .arg(src_audio)
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
