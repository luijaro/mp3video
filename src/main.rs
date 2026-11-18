#![windows_subsystem = "windows"]

mod cue;
mod gui;
mod vgmdb;
mod youtube;

pub(crate) const DEFAULT_TITLE_TEMPLATE: &str = "{album} - {track_num}. {track_title}";
pub(crate) const DEFAULT_DESC_TEMPLATE: &str = "Album: {album}\nCatalog Number: {catalog}\nTrack: {disc_track} - {track_title}\nComposer(s): {composers}\nArranger(s): {arrangers}\nPerformer(s): {performers}\nPublisher: {publisher}\nRelease Date: {release_date}";

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

pub(crate) const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp"];
pub(crate) const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "m4a", "aac", "ogg", "opus", "ape", "wv", "wma",
];
const COMMON_COVER_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.png",
    "cover.jpeg",
    "folder.jpg",
    "folder.png",
    "portada.jpg",
    "portada.png",
];

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum FitMode {
    /// Ajusta la imagen completa dentro del cuadro con bordes negros.
    Contain,
    /// Recorta la imagen para llenar todo el cuadro sin bordes.
    Cover,
    /// Fondo desenfocado con la imagen centrada encima (ideal para portadas cuadradas).
    Blur,
    /// Estira la imagen para ocupar exactamente la resolución indicada.
    Stretch,
}

#[derive(Parser, Debug)]
#[command(
    name = "mp3video",
    version,
    about = "Renderiza lotes de archivos MP3 con una imagen estática a videos MP4 usando FFmpeg"
)]
struct Cli {
    /// Carpeta con archivos .mp3 o ruta a un archivo .mp3 individual
    #[arg(short, long, default_value = ".")]
    input: PathBuf,

    /// Ruta a la imagen estática por defecto (.jpg, .jpeg, .png, .webp, .bmp)
    #[arg(short = 'm', long)]
    image: Option<PathBuf>,

    /// Carpeta de salida para los videos .mp4
    #[arg(short, long, default_value = "output")]
    output: PathBuf,

    /// Buscar archivos .mp3 recursivamente en subcarpetas
    #[arg(short, long)]
    recursive: bool,

    /// Cantidad de procesos FFmpeg en paralelo (por defecto: mitad de núcleos, entre 1 y 4)
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Resolución del video de salida (ej: 1920x1080, 1280x720)
    #[arg(long, default_value = "1920x1080")]
    resolution: String,

    /// Modo de ajuste de la imagen dentro del video
    #[arg(long, value_enum, default_value_t = FitMode::Contain)]
    fit: FitMode,

    /// Framerate de la imagen en bucle (valores bajos como 2 aceleran mucho el render)
    #[arg(long, default_value_t = 2)]
    fps: u32,

    /// Factor de calidad constante H.264 (0-51; menor valor = mayor calidad)
    #[arg(long, default_value_t = 18)]
    crf: u8,

    /// Bitrate de audio AAC (ej: 128k, 192k, 320k)
    #[arg(long, default_value = "192k")]
    audio_bitrate: String,

    /// Copiar el stream de audio MP3 sin recodificar a AAC (máxima velocidad)
    #[arg(long)]
    copy_audio: bool,

    /// Sobrescribir archivos .mp4 existentes en la carpeta de salida
    #[arg(long)]
    overwrite: bool,

    /// Ruta personalizada al ejecutable ffmpeg
    #[arg(long)]
    ffmpeg_path: Option<PathBuf>,

    /// Código de catálogo (ej: SQEX-10001) o ID de VGMdb para buscar álbum y compositores
    #[arg(long)]
    vgmdb: Option<String>,

    /// Solo consultar e imprimir la ficha del disco (compositores y pistas) sin renderizar
    #[arg(long)]
    vgmdb_only: bool,

    /// URL base alternativa de la API de VGMdb (por defecto: https://vgmdb.info)
    #[arg(long)]
    vgmdb_url: Option<String>,

    /// Subir los videos .mp4 generados a YouTube al finalizar
    #[arg(long)]
    youtube_upload: bool,

    /// Ruta al archivo client_secret.json de Google OAuth para YouTube
    #[arg(long)]
    client_secret: Option<PathBuf>,

    /// Privacidad de los videos subidos a YouTube (unlisted, private, public)
    #[arg(long, default_value = "unlisted")]
    youtube_privacy: String,

    /// Crear una Playlist en YouTube con el álbum y agregar los videos
    #[arg(long)]
    youtube_playlist: bool,

    /// Plantilla para el título en YouTube
    #[arg(long, default_value = DEFAULT_TITLE_TEMPLATE)]
    title_template: String,

    /// Plantilla para la descripción en YouTube
    #[arg(long, default_value = DEFAULT_DESC_TEMPLATE)]
    desc_template: String,

    /// Extraer archivos .mp3 individuales desde una hoja .cue + .flac en lugar de generar videos
    #[arg(long)]
    split_mp3: bool,

    /// Renderizar 1 solo video largo del álbum completo con capítulos y timestamps
    #[arg(long)]
    full_album: bool,
}

pub(crate) struct RenderConfig {
    pub input: PathBuf,
    pub image: Option<PathBuf>,
    pub output: PathBuf,
    pub recursive: bool,
    pub jobs: usize,
    pub width: u32,
    pub height: u32,
    pub fit: FitMode,
    pub fps: u32,
    pub crf: u8,
    pub audio_bitrate: String,
    pub copy_audio: bool,
    pub overwrite: bool,
    pub ffmpeg_path: Option<PathBuf>,
}

pub(crate) struct RenderJob {
    pub mp3_path: PathBuf,
    pub output_path: PathBuf,
}

pub(crate) fn default_jobs() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (cores / 2).clamp(1, 4)
}

fn parse_resolution(raw: &str) -> Result<(u32, u32)> {
    let normalized = raw.trim().to_ascii_lowercase();
    let (w_str, h_str) = normalized
        .split_once('x')
        .with_context(|| {
            format!(
                "Formato de resolución inválido '{}'. Usá ANCHOxALTO (ej: 1920x1080).",
                raw
            )
        })?;

    let width: u32 = w_str
        .trim()
        .parse()
        .with_context(|| format!("Ancho inválido en resolución '{}'", raw))?;
    let height: u32 = h_str
        .trim()
        .parse()
        .with_context(|| format!("Alto inválido en resolución '{}'", raw))?;

    if width == 0 || height == 0 {
        bail!("La resolución debe ser mayor que cero.");
    }
    if width % 2 != 0 || height % 2 != 0 {
        bail!(
            "La resolución '{}' debe tener ancho y alto pares para codificar en H.264 (yuv420p).",
            raw
        );
    }

    Ok((width, height))
}

pub(crate) fn has_extension(path: &Path, expected_exts: &[&str]) -> bool {
    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };
    expected_exts
        .iter()
        .any(|candidate| ext.eq_ignore_ascii_case(candidate))
}

pub(crate) fn new_hidden_command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub(crate) fn find_ffprobe(ffmpeg_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(ff) = ffmpeg_path {
        let sibling = ff.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        if sibling.is_file() {
            return Some(sibling);
        }
    } else if let Ok(ff) = find_ffmpeg(None) {
        let sibling = ff.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        if sibling.is_file() {
            return Some(sibling);
        }
    }

    let candidate = PathBuf::from("ffprobe");
    let ok = new_hidden_command(&candidate)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(candidate)
}

fn is_executable_ffmpeg(path: &Path) -> bool {
    new_hidden_command(path)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn find_ffmpeg(custom_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = custom_path {
        if !path.is_file() {
            bail!(
                "No se encontró ffmpeg en la ruta especificada: {}",
                path.display()
            );
        }
        if !is_executable_ffmpeg(path) {
            bail!(
                "El archivo especificado en --ffmpeg-path no pudo ejecutarse como ffmpeg: {}",
                path.display()
            );
        }
        return Ok(path.to_path_buf());
    }

    let path_candidate = PathBuf::from("ffmpeg");
    if is_executable_ffmpeg(&path_candidate) {
        return Ok(path_candidate);
    }

    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let local_app = PathBuf::from(local_app_data);

        let winget_link = local_app
            .join("Microsoft")
            .join("WinGet")
            .join("Links")
            .join("ffmpeg.exe");
        if winget_link.is_file() && is_executable_ffmpeg(&winget_link) {
            return Ok(winget_link);
        }

        let winget_pkgs = local_app.join("Microsoft").join("WinGet").join("Packages");
        if winget_pkgs.is_dir() {
            if let Ok(entries) = fs::read_dir(&winget_pkgs) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with("Gyan.FFmpeg") {
                        for walk_entry in WalkDir::new(entry.path())
                            .max_depth(5)
                            .into_iter()
                            .flatten()
                        {
                            let p = walk_entry.path();
                            if p.is_file()
                                && p.file_name()
                                    .and_then(OsStr::to_str)
                                    .is_some_and(|n| n.eq_ignore_ascii_case("ffmpeg.exe"))
                                && is_executable_ffmpeg(p)
                            {
                                return Ok(p.to_path_buf());
                            }
                        }
                    }
                }
            }
        }
    }

    bail!(
        "No se encontró 'ffmpeg' en el PATH ni en las carpetas estándar de WinGet. \
         Instalalo o pasá su ruta con --ffmpeg-path."
    );
}

pub(crate) fn collect_audio_files(
    input: &Path,
    output_dir: &Path,
    recursive: bool,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    if !input.exists() {
        bail!("La ruta de entrada no existe: {}", input.display());
    }

    if input.is_file() {
        if !has_extension(input, AUDIO_EXTENSIONS) {
            bail!(
                "El archivo de entrada no es un formato de audio soportado (.flac, .mp3, .wav, etc.): {}",
                input.display()
            );
        }
        let base_dir = input
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        return Ok((base_dir, vec![input.to_path_buf()]));
    }

    let output_canon = fs::canonicalize(output_dir).ok();
    let max_depth = if recursive { usize::MAX } else { 1 };

    let mut audios = Vec::new();
    let walker = WalkDir::new(input)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() > 0 && entry.file_type().is_dir() {
                if let Some(ref out_c) = output_canon {
                    if let Ok(dir_c) = fs::canonicalize(entry.path()) {
                        if &dir_c == out_c {
                            return false;
                        }
                    }
                }
            }
            true
        });

    for entry in walker.flatten() {
        let path = entry.path();
        if path.is_file() && has_extension(path, AUDIO_EXTENSIONS) {
            audios.push(path.to_path_buf());
        }
    }

    audios.sort();
    Ok((input.to_path_buf(), audios))
}

pub(crate) fn collect_mp3_files(
    input: &Path,
    output_dir: &Path,
    recursive: bool,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    collect_audio_files(input, output_dir, recursive)
}

pub(crate) fn output_path_for(
    mp3_path: &Path,
    input_base: &Path,
    output_dir: &Path,
    recursive: bool,
) -> PathBuf {
    let stem = mp3_path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("video"))
        .to_string_lossy();
    let file_name = format!("{}.mp4", stem);

    if recursive {
        if let Ok(rel) = mp3_path.strip_prefix(input_base) {
            if let Some(rel_parent) = rel.parent() {
                if !rel_parent.as_os_str().is_empty() {
                    return output_dir.join(rel_parent).join(file_name);
                }
            }
        }
    }

    output_dir.join(file_name)
}

fn find_single_image_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut found: Option<PathBuf> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && has_extension(&path, IMAGE_EXTENSIONS) {
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }

    found
}

fn find_common_cover_in_dir(dir: &Path) -> Option<PathBuf> {
    for name in COMMON_COVER_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn resolve_image(
    mp3_path: &Path,
    default_image: Option<&Path>,
    input_dir: &Path,
) -> Option<PathBuf> {
    let mp3_dir = mp3_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // 1. Imagen con el mismo nombre base en la carpeta del MP3
    if let Some(stem) = mp3_path.file_stem().and_then(OsStr::to_str) {
        for ext in IMAGE_EXTENSIONS {
            let exact = mp3_dir.join(format!("{}.{}", stem, ext));
            if exact.is_file() {
                return Some(exact);
            }
        }
        if let Ok(entries) = fs::read_dir(mp3_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && has_extension(&p, IMAGE_EXTENSIONS) {
                    if let Some(p_stem) = p.file_stem().and_then(OsStr::to_str) {
                        if p_stem.eq_ignore_ascii_case(stem) {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }

    // 2. Imagen explícita provista por --image o seleccionada en la GUI
    if let Some(img) = default_image {
        return Some(img.to_path_buf());
    }

    // 3. Portadas comunes en la carpeta del MP3 o carpeta de entrada, o imagen única
    if let Some(cover) = find_common_cover_in_dir(mp3_dir) {
        return Some(cover);
    }
    if mp3_dir != input_dir {
        if let Some(cover) = find_common_cover_in_dir(input_dir) {
            return Some(cover);
        }
        if let Some(single) = find_single_image_in_dir(mp3_dir) {
            return Some(single);
        }
    }
    if let Some(single) = find_single_image_in_dir(input_dir) {
        return Some(single);
    }

    None
}

fn apply_video_and_codec_args(
    cmd: &mut Command,
    cfg: &RenderConfig,
    source_audio: Option<&Path>,
    allow_copy_audio: bool,
) {
    let width = cfg.width;
    let height = cfg.height;
    match cfg.fit {
        FitMode::Contain => {
            let vf = format!(
                "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black,format=yuv420p",
                w = width,
                h = height
            );
            cmd.arg("-vf").arg(vf).arg("-map").arg("0:v:0").arg("-map").arg("1:a:0");
        }
        FitMode::Cover => {
            let vf = format!(
                "scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},format=yuv420p",
                w = width,
                h = height
            );
            cmd.arg("-vf").arg(vf).arg("-map").arg("0:v:0").arg("-map").arg("1:a:0");
        }
        FitMode::Blur => {
            let fc = format!(
                "[0:v]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},boxblur=20:5[bg];[0:v]scale={w}:{h}:force_original_aspect_ratio=decrease[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2,format=yuv420p[vout]",
                w = width,
                h = height
            );
            cmd.arg("-filter_complex")
                .arg(fc)
                .arg("-map")
                .arg("[vout]")
                .arg("-map")
                .arg("1:a:0");
        }
        FitMode::Stretch => {
            let vf = format!("scale={w}:{h},format=yuv420p", w = width, h = height);
            cmd.arg("-vf").arg(vf).arg("-map").arg("0:v:0").arg("-map").arg("1:a:0");
        }
    }

    cmd.arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-tune")
        .arg("stillimage")
        .arg("-crf")
        .arg(cfg.crf.to_string());

    let can_copy = allow_copy_audio
        && cfg.copy_audio
        && source_audio.is_some_and(|p| has_extension(p, &["mp3", "m4a", "aac"]));

    if can_copy {
        cmd.arg("-c:a").arg("copy");
    } else {
        cmd.arg("-c:a").arg("aac").arg("-b:a").arg(&cfg.audio_bitrate);
    }
}

fn finalize_ffmpeg_render(
    mut cmd: Command,
    temp_output: &Path,
    output_path: &Path,
    err_ctx: &str,
) -> Result<()> {
    cmd.arg("-shortest")
        .arg("-max_interleave_delta")
        .arg("100M")
        .arg("-movflags")
        .arg("+faststart")
        .arg(temp_output);

    let output = cmd
        .output()
        .with_context(|| format!("No se pudo ejecutar ffmpeg para {}", err_ctx))?;

    if !output.status.success() {
        let _ = fs::remove_file(temp_output);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("ffmpeg terminó con código {:?}", output.status.code())
        } else {
            stderr
        };
        bail!(detail);
    }

    if output_path.exists() {
        let _ = fs::remove_file(output_path);
    }
    fs::rename(temp_output, output_path).with_context(|| {
        let _ = fs::remove_file(temp_output);
        format!(
            "No se pudo mover el video final a {}",
            output_path.display()
        )
    })?;

    Ok(())
}

fn run_ffmpeg_video_render(
    ffmpeg: &Path,
    audio_path: &Path,
    time_range: Option<(f64, Option<f64>)>,
    image_path: &Path,
    output_path: &Path,
    cfg: &RenderConfig,
) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "No se pudo crear la carpeta de salida: {}",
                parent.display()
            )
        })?;
    }

    let file_name = output_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("video.mp4"))
        .to_string_lossy();
    let temp_output = output_path.with_file_name(format!(".{}.part.mp4", file_name));

    let mut cmd = new_hidden_command(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-loop")
        .arg("1")
        .arg("-framerate")
        .arg(cfg.fps.to_string())
        .arg("-i")
        .arg(image_path);

    if let Some((start_secs, end_secs)) = time_range {
        cmd.arg("-ss").arg(format!("{:.3}", start_secs));
        if let Some(end) = end_secs {
            let dur = (end - start_secs).max(0.1);
            cmd.arg("-t").arg(format!("{:.3}", dur));
        }
    }

    cmd.arg("-i").arg(audio_path);
    apply_video_and_codec_args(&mut cmd, cfg, Some(audio_path), time_range.is_none());
    finalize_ffmpeg_render(cmd, &temp_output, output_path, &audio_path.display().to_string())
}

pub(crate) fn full_album_output_path(
    output_dir: &Path,
    cue_sheet: &cue::CueSheet,
    album: Option<&vgmdb::AlbumMetadata>,
) -> PathBuf {
    let base_name = album
        .map(|a| a.album_title.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            cue_sheet
                .album_title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            cue_sheet
                .cue_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "Album".to_string());

    let clean = cue::sanitize_filename(&base_name);
    output_dir.join(format!("{} - Full Soundtrack.mp4", clean))
}

pub(crate) fn render_full_album_to_mp4(
    ffmpeg: &Path,
    tracks: &[cue::CueTrack],
    fallback_audio: &Path,
    album_title: Option<&str>,
    image_path: &Path,
    output_path: &Path,
    cfg: &RenderConfig,
) -> Result<cue::TimelinePlan> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let plan = cue::compute_timeline(tracks, fallback_audio, Some(ffmpeg));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let ffmeta_path = std::env::temp_dir().join(format!("mp3video_meta_{}.txt", stamp));
    let concat_path = std::env::temp_dir().join(format!("mp3video_concat_{}.txt", stamp));

    cue::write_ffmetadata_chapters(&ffmeta_path, album_title, &plan.chapters)?;

    let file_name = output_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("full_album.mp4"))
        .to_string_lossy();
    let temp_output = output_path.with_file_name(format!(".{}.part.mp4", file_name));

    let mut cmd = new_hidden_command(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-loop")
        .arg("1")
        .arg("-framerate")
        .arg(cfg.fps.to_string())
        .arg("-i")
        .arg(image_path);

    if let Some(ref single_audio) = plan.single_direct_audio {
        cmd.arg("-i").arg(single_audio);
    } else {
        cue::write_ffconcat_file(&concat_path, tracks, fallback_audio)?;
        cmd.arg("-f")
            .arg("concat")
            .arg("-safe")
            .arg("0")
            .arg("-i")
            .arg(&concat_path);
    }

    cmd.arg("-f").arg("ffmetadata").arg("-i").arg(&ffmeta_path);
    apply_video_and_codec_args(&mut cmd, cfg, None, false);
    cmd.arg("-map_metadata").arg("2").arg("-map_chapters").arg("2");

    let res = finalize_ffmpeg_render(
        cmd,
        &temp_output,
        output_path,
        &fallback_audio.display().to_string(),
    );
    let _ = fs::remove_file(&ffmeta_path);
    let _ = fs::remove_file(&concat_path);
    res?;
    Ok(plan)
}

pub(crate) fn build_full_album_upload_item(
    mp4_path: &Path,
    cue_sheet: &cue::CueSheet,
    album: Option<&vgmdb::AlbumMetadata>,
    timestamps_block: &str,
    desc_tpl: &str,
) -> youtube::UploadItem {
    let album_title = album
        .map(|a| a.album_title.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| cue_sheet.album_title.clone())
        .or_else(|| {
            cue_sheet
                .cue_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "Full Soundtrack".to_string());

    let composers = album
        .map(|a| a.composers.join(", "))
        .filter(|s| !s.is_empty())
        .or_else(|| cue_sheet.performer.clone())
        .unwrap_or_default();

    let meta = vgmdb::ResolvedTrackMetadata {
        album_title: album_title.clone(),
        catalog: album
            .map(|a| a.catalog.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| cue_sheet.catalog.clone())
            .unwrap_or_default(),
        disc_number: 1,
        track_number: 1,
        track_title: String::new(),
        composers,
        arrangers: album.map(|a| a.arrangers.join(", ")).unwrap_or_default(),
        performers: album.map(|a| a.performers.join(", ")).unwrap_or_default(),
        publisher: album.map(|a| a.publisher.clone()).unwrap_or_default(),
        release_date: album
            .map(|a| a.release_date.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| cue_sheet.date.clone())
            .unwrap_or_default(),
    };

    let filtered_desc_tpl: String = desc_tpl
        .lines()
        .filter(|line| {
            !line.contains("{track_title}")
                && !line.contains("{disc_track}")
                && !line.contains("{track_num}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let header_desc = vgmdb::apply_template(&filtered_desc_tpl, &meta);
    let full_desc = if header_desc.is_empty() {
        format!("Tracklist:\n{}", timestamps_block)
    } else {
        format!("{}\n\nTracklist:\n{}", header_desc, timestamps_block)
    };

    let title = vgmdb::truncate_chars(&format!("{} - Full Soundtrack", album_title), 100);
    let description = vgmdb::truncate_chars(&full_desc, 4900);
    let mut tags = vgmdb::build_tags(&meta);
    if !tags.iter().any(|t| t.eq_ignore_ascii_case("Full Soundtrack")) {
        tags.push("Full Soundtrack".to_string());
    }

    youtube::UploadItem {
        mp4_path: mp4_path.to_path_buf(),
        title,
        description,
        tags,
    }
}

pub(crate) fn render_single(
    ffmpeg: &Path,
    job: &RenderJob,
    image_path: &Path,
    cfg: &RenderConfig,
) -> Result<()> {
    run_ffmpeg_video_render(
        ffmpeg,
        &job.mp3_path,
        None,
        image_path,
        &job.output_path,
        cfg,
    )
}

pub(crate) fn render_cue_track_to_mp4(
    ffmpeg: &Path,
    audio_path: &Path,
    track: &cue::CueTrack,
    image_path: &Path,
    output_path: &Path,
    cfg: &RenderConfig,
) -> Result<()> {
    run_ffmpeg_video_render(
        ffmpeg,
        audio_path,
        Some((track.start_secs, track.end_secs)),
        image_path,
        output_path,
        cfg,
    )
}

pub(crate) fn build_upload_items_for_cue(
    tracks: &[(cue::CueTrack, PathBuf)],
    cue_sheet: &cue::CueSheet,
    album: Option<&vgmdb::AlbumMetadata>,
    title_tpl: &str,
    desc_tpl: &str,
) -> Vec<youtube::UploadItem> {
    tracks
        .iter()
        .filter(|(_, mp4)| mp4.is_file())
        .map(|(tr, mp4)| {
            let album_title = album
                .map(|a| a.album_title.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| cue_sheet.album_title.clone())
                .unwrap_or_default();
            let composers = album
                .map(|a| a.composers.join(", "))
                .filter(|s| !s.is_empty())
                .or_else(|| tr.performer.clone())
                .or_else(|| cue_sheet.performer.clone())
                .unwrap_or_default();
            let meta = vgmdb::ResolvedTrackMetadata {
                album_title,
                catalog: album
                    .map(|a| a.catalog.clone())
                    .filter(|s| !s.is_empty())
                    .or_else(|| cue_sheet.catalog.clone())
                    .unwrap_or_default(),
                disc_number: 1,
                track_number: tr.track_number,
                track_title: tr.title.clone(),
                composers,
                arrangers: album.map(|a| a.arrangers.join(", ")).unwrap_or_default(),
                performers: album.map(|a| a.performers.join(", ")).unwrap_or_default(),
                publisher: album.map(|a| a.publisher.clone()).unwrap_or_default(),
                release_date: album
                    .map(|a| a.release_date.clone())
                    .filter(|s| !s.is_empty())
                    .or_else(|| cue_sheet.date.clone())
                    .unwrap_or_default(),
            };
            let title = vgmdb::format_title(title_tpl, &meta);
            let description = vgmdb::format_description(desc_tpl, &meta);
            let tags = vgmdb::build_tags(&meta);
            youtube::UploadItem {
                mp4_path: mp4.clone(),
                title,
                description,
                tags,
            }
        })
        .collect()
}

pub(crate) fn build_upload_items(
    pairs: &[(PathBuf, PathBuf)],
    album: Option<&vgmdb::AlbumMetadata>,
    ffmpeg_path: Option<&Path>,
    title_tpl: &str,
    desc_tpl: &str,
) -> Vec<youtube::UploadItem> {
    pairs
        .iter()
        .enumerate()
        .filter(|(_, (_, mp4))| mp4.is_file())
        .map(|(idx, (tag_source, mp4))| {
            let id3 = vgmdb::read_mp3_tags(tag_source, ffmpeg_path);
            let meta = vgmdb::match_track_for_file(tag_source, idx, album, &id3);
            let title = vgmdb::format_title(title_tpl, &meta);
            let description = vgmdb::format_description(desc_tpl, &meta);
            let tags = vgmdb::build_tags(&meta);
            youtube::UploadItem {
                mp4_path: mp4.clone(),
                title,
                description,
                tags,
            }
        })
        .collect()
}

pub(crate) fn write_video_sidecar_txt(item: &youtube::UploadItem) -> Result<PathBuf> {
    let txt_path = item.mp4_path.with_extension("txt");
    let content = format!(
        "Title:\r\n{}\r\n\r\nDescription:\r\n{}\r\n",
        item.title.trim(),
        item.description.trim()
    );
    fs::write(&txt_path, content.as_bytes())
        .with_context(|| format!("No se pudo escribir el archivo TXT: {}", txt_path.display()))?;
    Ok(txt_path)
}

pub(crate) fn write_master_tracklist_txt(
    output_dir: &Path,
    items: &[youtube::UploadItem],
) -> Result<PathBuf> {
    if items.is_empty() {
        return Ok(output_dir.join("_youtube_tracklist.txt"));
    }
    let master_path = output_dir.join("_youtube_tracklist.txt");
    let mut out = String::new();
    out.push_str("================================================================================\r\n");
    out.push_str("YOUTUBE TRACKLIST & DESCRIPTIONS\r\n");
    out.push_str("================================================================================\r\n\r\n");

    for (idx, item) in items.iter().enumerate() {
        let file_name = item
            .mp4_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        out.push_str(&format!("[Video {}] {}\r\n", idx + 1, file_name));
        out.push_str("Title:\r\n");
        out.push_str(item.title.trim());
        out.push_str("\r\n\r\nDescription:\r\n");
        out.push_str(item.description.trim());
        out.push_str("\r\n\r\n");
        if idx + 1 < items.len() {
            out.push_str("--------------------------------------------------------------------------------\r\n\r\n");
        }
    }

    fs::write(&master_path, out.as_bytes()).with_context(|| {
        format!(
            "No se pudo escribir el archivo de lista: {}",
            master_path.display()
        )
    })?;
    Ok(master_path)
}

fn print_album_summary(album: &vgmdb::AlbumMetadata, title_tpl: &str) {
    println!("Source: {}", album.source);
    println!("Album: {}", album.album_title);
    if !album.catalog.is_empty() {
        println!("Catalog Number: {}", album.catalog);
    }
    if !album.publisher.is_empty() {
        println!("Publisher: {}", album.publisher);
    }
    if !album.release_date.is_empty() {
        println!("Release Date: {}", album.release_date);
    }
    if !album.composers.is_empty() {
        println!("Composer(s): {}", album.composers.join(", "));
    }
    if !album.arrangers.is_empty() {
        println!("Arranger(s): {}", album.arrangers.join(", "));
    }
    if !album.performers.is_empty() {
        println!("Performer(s): {}", album.performers.join(", "));
    }
    println!("Tracks ({}):", album.tracks.len());
    for (idx, tr) in album.tracks.iter().enumerate() {
        let meta = vgmdb::ResolvedTrackMetadata {
            album_title: album.album_title.clone(),
            catalog: album.catalog.clone(),
            disc_number: tr.disc_number,
            track_number: tr.track_number,
            track_title: tr.title.clone(),
            composers: tr.composers.join(", "),
            arrangers: album.arrangers.join(", "),
            performers: album.performers.join(", "),
            publisher: album.publisher.clone(),
            release_date: album.release_date.clone(),
        };
        let yt_title = vgmdb::format_title(title_tpl, &meta);
        let dur = if tr.duration.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tr.duration)
        };
        println!(
            "  [{}-{:02}]{} {}  ->  YT: {}",
            tr.disc_number, tr.track_number, dur, tr.title, yt_title
        );
        if idx >= 19 && album.tracks.len() > 25 {
            println!("  ... and {} more track(s).", album.tracks.len() - idx - 1);
            break;
        }
    }
}

fn run_cli() -> Result<()> {
    let cli = Cli::parse();

    let album_meta = if let Some(ref code) = cli.vgmdb {
        let album = vgmdb::lookup_album(code, cli.vgmdb_url.as_deref())?;
        print_album_summary(&album, &cli.title_template);
        println!();
        Some(album)
    } else {
        None
    };

    if cli.vgmdb_only {
        if album_meta.is_none() {
            bail!("Usá --vgmdb <CODIGO> junto con --vgmdb-only.");
        }
        return Ok(());
    }

    if cli.fps == 0 {
        bail!("El framerate (--fps) debe ser mayor o igual a 1.");
    }
    if cli.crf > 51 {
        bail!("El valor de --crf debe estar entre 0 y 51.");
    }

    let yt_privacy = youtube::PrivacyStatus::from_str_lossy(&cli.youtube_privacy)?;
    if cli.youtube_upload
        && cli.client_secret.is_none()
        && !youtube::has_saved_tokens(&youtube::default_token_store_path())
    {
        bail!("Para usar --youtube-upload debés indicar --client-secret <RUTA_JSON> o haber conectado la cuenta en la interfaz.");
    }

    let (width, height) = parse_resolution(&cli.resolution)?;
    let jobs = cli.jobs.unwrap_or_else(default_jobs).max(1);

    if let Some(ref img) = cli.image {
        if !img.is_file() {
            bail!("La imagen especificada en --image no existe: {}", img.display());
        }
        if !has_extension(img, IMAGE_EXTENSIONS) {
            bail!(
                "El archivo de imagen '{}' no tiene una extensión soportada ({}).",
                img.display(),
                IMAGE_EXTENSIONS.join(", ")
            );
        }
    }

    let cfg = RenderConfig {
        input: cli.input,
        image: cli.image,
        output: cli.output,
        recursive: cli.recursive,
        jobs,
        width,
        height,
        fit: cli.fit,
        fps: cli.fps,
        crf: cli.crf,
        audio_bitrate: cli.audio_bitrate,
        copy_audio: cli.copy_audio,
        overwrite: cli.overwrite,
        ffmpeg_path: cli.ffmpeg_path,
    };

    let ffmpeg = find_ffmpeg(cfg.ffmpeg_path.as_deref())?;

    if let Some(cue_path) = cue::resolve_cue_for_input(&cfg.input) {
        let cue_sheet = cue::parse_cue_file(&cue_path)?;
        return run_cue_cli(
            &cue_sheet,
            &ffmpeg,
            &cfg,
            album_meta.as_ref(),
            cli.split_mp3,
            cli.full_album,
            cli.youtube_upload,
            cli.client_secret.as_deref(),
            yt_privacy,
            cli.youtube_playlist,
            &cli.title_template,
            &cli.desc_template,
        );
    }

    if cli.full_album {
        let cue_sheet = cue::build_tracks_from_directory(
            &cfg.input,
            &cfg.output,
            cfg.recursive,
            album_meta.as_ref(),
            cfg.ffmpeg_path.as_deref(),
        )?;
        return run_cue_cli(
            &cue_sheet,
            &ffmpeg,
            &cfg,
            album_meta.as_ref(),
            false,
            true,
            cli.youtube_upload,
            cli.client_secret.as_deref(),
            yt_privacy,
            cli.youtube_playlist,
            &cli.title_template,
            &cli.desc_template,
        );
    }

    let (input_base, mp3_files) = collect_mp3_files(&cfg.input, &cfg.output, cfg.recursive)?;

    if mp3_files.is_empty() {
        println!(
            "No se encontraron archivos .mp3 en '{}'.",
            cfg.input.display()
        );
        return Ok(());
    }

    fs::create_dir_all(&cfg.output).with_context(|| {
        format!(
            "No se pudo crear la carpeta de salida: {}",
            cfg.output.display()
        )
    })?;

    let mut all_pairs = Vec::with_capacity(mp3_files.len());
    let mut pending_jobs = Vec::new();
    let mut skipped_count = 0usize;

    for mp3_path in mp3_files {
        let output_path = output_path_for(&mp3_path, &input_base, &cfg.output, cfg.recursive);
        all_pairs.push((mp3_path.clone(), output_path.clone()));
        if output_path.exists() && !cfg.overwrite {
            skipped_count += 1;
            continue;
        }
        pending_jobs.push(RenderJob {
            mp3_path,
            output_path,
        });
    }

    let total_pending = pending_jobs.len();
    println!(
        "FFmpeg: {} | Resolución: {}x{} | Ajuste: {:?} | FPS: {} | Hilos: {}",
        ffmpeg.display(),
        cfg.width,
        cfg.height,
        cfg.fit,
        cfg.fps,
        cfg.jobs
    );
    if skipped_count > 0 {
        println!(
            "Omitidos por ya existir: {} (usá --overwrite para regenerarlos)",
            skipped_count
        );
    }

    let mut err_count = 0usize;
    if total_pending == 0 {
        println!("No hay videos pendientes para renderizar.");
    } else {
        println!("Renderizando {} video(s)...\n", total_pending);

        let multi = MultiProgress::new();
        let main_pb = multi.add(ProgressBar::new(total_pending as u64));
        main_pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        main_pb.enable_steady_tick(Duration::from_millis(120));

        let worker_style = ProgressStyle::with_template("  {spinner:.yellow} {msg}").unwrap();
        let worker_bars: Vec<ProgressBar> = (0..cfg.jobs)
            .map(|i| {
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(worker_style.clone());
                pb.set_message(format!("Hilo {}: esperando...", i + 1));
                pb.enable_steady_tick(Duration::from_millis(120));
                pb
            })
            .collect();

        let rendered_count = AtomicUsize::new(0);
        let errors: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());
        let started = Instant::now();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(cfg.jobs)
            .build()
            .context("No se pudo inicializar el pool de hilos de Rayon")?;

        pool.install(|| {
            pending_jobs.par_iter().for_each(|job| {
                let worker_idx = rayon::current_thread_index().unwrap_or(0) % worker_bars.len();
                let worker_pb = &worker_bars[worker_idx];
                let song_name = job
                    .mp3_path
                    .file_name()
                    .unwrap_or_else(|| OsStr::new("?"))
                    .to_string_lossy();

                worker_pb.set_message(format!("Hilo {}: {}", worker_idx + 1, song_name));

                let result = match resolve_image(&job.mp3_path, cfg.image.as_deref(), &input_base) {
                    Some(image_path) => render_single(&ffmpeg, job, &image_path, &cfg),
                    None => Err(anyhow::anyhow!(
                        "No se encontró imagen asociada (indicá una con --image, colocá una imagen con el mismo nombre o una única portada en la carpeta)"
                    )),
                };

                match result {
                    Ok(()) => {
                        rendered_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(err) => {
                        if let Ok(mut guard) = errors.lock() {
                            guard.push((job.mp3_path.clone(), format!("{:#}", err)));
                        }
                    }
                }

                main_pb.inc(1);
                worker_pb.set_message(format!("Hilo {}: libre", worker_idx + 1));
            });
        });

        for pb in worker_bars {
            pb.finish_and_clear();
        }
        main_pb.finish_with_message("Completado");

        let elapsed = started.elapsed();
        let ok_count = rendered_count.load(Ordering::Relaxed);
        let err_list = errors.into_inner().unwrap_or_default();
        err_count = err_list.len();

        println!(
            "\nResumen en {:.1}s: {} renderizado(s), {} omitido(s), {} error(es).",
            elapsed.as_secs_f64(),
            ok_count,
            skipped_count,
            err_count
        );

        if !err_list.is_empty() {
            eprintln!("\nErrores detectados:");
            for (path, msg) in &err_list {
                eprintln!("  - {}: {}", path.display(), msg);
            }
        }
    }

    let upload_items = build_upload_items(
        &all_pairs,
        album_meta.as_ref(),
        cfg.ffmpeg_path.as_deref(),
        &cli.title_template,
        &cli.desc_template,
    );
    for item in &upload_items {
        let _ = write_video_sidecar_txt(item);
    }
    if let Ok(master) = write_master_tracklist_txt(&cfg.output, &upload_items) {
        println!("Metadata YouTube guardada en: {}", master.display());
    }

    if cli.youtube_upload {
        let pl_title_opt = playlist_meta_tuple(cli.youtube_playlist, album_meta.as_ref());

        let summary = youtube::upload_videos_batch(
            &upload_items,
            &cfg.output,
            cli.client_secret.as_deref(),
            yt_privacy,
            pl_title_opt.as_ref().map(|(t, d)| (t.as_str(), d.as_str())),
            None,
            |line| println!("{}", line),
            |done, total, msg| println!("YouTube [{}/{}] {}", done, total, msg),
        )?;

        println!(
            "\nResumen YouTube: {} subido(s), {} omitido(s), {} error(es).",
            summary.uploaded_count, summary.skipped_count, summary.err_count
        );
        if summary.err_count > 0 {
            err_count += summary.err_count;
        }
    }

    if err_count > 0 {
        bail!("Finalizó con {} error(es).", err_count);
    }

    Ok(())
}

fn playlist_meta_tuple(
    create_playlist: bool,
    album_meta: Option<&vgmdb::AlbumMetadata>,
) -> Option<(String, String)> {
    if !create_playlist {
        return None;
    }
    album_meta.map(|a| {
        let t = if a.catalog.is_empty() {
            a.album_title.clone()
        } else {
            format!("{} ({})", a.album_title, a.catalog)
        };
        let d = if a.composers.is_empty() {
            a.album_title.clone()
        } else {
            format!("Composer(s): {}", a.composers.join(", "))
        };
        (t, d)
    })
}

#[allow(clippy::too_many_arguments)]
fn run_cue_cli(
    cue_sheet: &cue::CueSheet,
    ffmpeg: &Path,
    cfg: &RenderConfig,
    album_meta: Option<&vgmdb::AlbumMetadata>,
    split_mp3: bool,
    full_album: bool,
    youtube_upload: bool,
    client_secret: Option<&Path>,
    yt_privacy: youtube::PrivacyStatus,
    youtube_playlist: bool,
    title_tpl: &str,
    desc_tpl: &str,
) -> Result<()> {
    println!(
        "CUE/Álbum cargado: {} -> Audio: {} ({} pistas)",
        cue_sheet.cue_path.display(),
        cue_sheet.audio_path.display(),
        cue_sheet.tracks.len()
    );

    if split_mp3 {
        let album_ovr = album_meta.map(|a| a.album_title.as_str());
        let artist_ovr = album_meta
            .map(|a| a.composers.join(", "))
            .filter(|s| !s.is_empty());
        let (ok, skipped, errs) = cue::split_cue_to_mp3(
            cue_sheet,
            &cue_sheet.tracks,
            &cfg.output,
            album_ovr,
            artist_ovr.as_deref(),
            ffmpeg,
            cfg.jobs,
            cfg.overwrite,
            None,
            |line| println!("{}", line),
            |done, total, msg| println!("[{}/{}] {}", done, total, msg),
        )?;
        println!(
            "\nResumen CUE -> MP3: {} extraído(s), {} omitido(s), {} error(es).",
            ok, skipped, errs
        );
        if errs > 0 {
            bail!("Finalizó con {} error(es).", errs);
        }
        return Ok(());
    }

    let cue_dir = if cue_sheet.cue_path.is_dir() {
        cue_sheet.cue_path.as_path()
    } else {
        cue_sheet
            .cue_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    };
    let image_path = resolve_image(&cue_sheet.audio_path, cfg.image.as_deref(), cue_dir)
        .context("No se encontró imagen para renderizar (indicá una con --image o colocá una portada en la carpeta)")?;

    fs::create_dir_all(&cfg.output)?;

    if full_album {
        let out_mp4 = full_album_output_path(&cfg.output, cue_sheet, album_meta);
        let alb_title = album_meta
            .map(|a| a.album_title.as_str())
            .or(cue_sheet.album_title.as_deref());
        let started = Instant::now();

        let plan = if out_mp4.exists() && !cfg.overwrite {
            println!(
                "Omitido por ya existir: {} (usá --overwrite para regenerarlo)",
                out_mp4.display()
            );
            cue::compute_timeline(&cue_sheet.tracks, &cue_sheet.audio_path, Some(ffmpeg))
        } else {
            let p = render_full_album_to_mp4(
                ffmpeg,
                &cue_sheet.tracks,
                &cue_sheet.audio_path,
                alb_title,
                &image_path,
                &out_mp4,
                cfg,
            )?;
            println!(
                "\nVideo completo generado en {:.1}s: {}",
                started.elapsed().as_secs_f64(),
                out_mp4.display()
            );
            p
        };

        let timestamps = cue::format_youtube_timestamps(&plan);
        println!("\nTracklist:\n{}", timestamps);

        let item = build_full_album_upload_item(
            &out_mp4,
            cue_sheet,
            album_meta,
            &timestamps,
            desc_tpl,
        );
        let _ = write_video_sidecar_txt(&item);
        let _ = write_master_tracklist_txt(&cfg.output, std::slice::from_ref(&item));
        println!("Metadata YouTube guardada en: {}", cfg.output.join("_youtube_tracklist.txt").display());

        if youtube_upload {
            let pl_title_opt = playlist_meta_tuple(youtube_playlist, album_meta);
            let summary = youtube::upload_videos_batch(
                &[item],
                &cfg.output,
                client_secret,
                yt_privacy,
                pl_title_opt.as_ref().map(|(t, d)| (t.as_str(), d.as_str())),
                None,
                |line| println!("{}", line),
                |done, total, msg| println!("YouTube [{}/{}] {}", done, total, msg),
            )?;
            println!(
                "\nResumen YouTube: {} subido(s), {} omitido(s), {} error(es).",
                summary.uploaded_count, summary.skipped_count, summary.err_count
            );
            if summary.err_count > 0 {
                bail!("Finalizó con {} error(es).", summary.err_count);
            }
        }
        return Ok(());
    }

    let mut all_pairs = Vec::with_capacity(cue_sheet.tracks.len());
    let mut pending = Vec::new();
    let mut skipped_count = 0usize;

    for tr in &cue_sheet.tracks {
        let stem = cue::cue_track_output_stem(tr);
        let out_mp4 = cfg.output.join(format!("{}.mp4", stem));
        all_pairs.push((tr.clone(), out_mp4.clone()));
        if out_mp4.exists() && !cfg.overwrite {
            skipped_count += 1;
            continue;
        }
        pending.push((tr.clone(), out_mp4));
    }

    let mut err_count = 0usize;
    if !pending.is_empty() {
        let started = Instant::now();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(cfg.jobs).build()?;
        let ok_count = AtomicUsize::new(0);
        let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

        pool.install(|| {
            pending.par_iter().for_each(|(tr, out_mp4)| {
                match render_cue_track_to_mp4(
                    ffmpeg,
                    tr.source_audio.as_deref().unwrap_or(&cue_sheet.audio_path),
                    tr,
                    &image_path,
                    out_mp4,
                    cfg,
                ) {
                    Ok(()) => {
                        ok_count.fetch_add(1, Ordering::Relaxed);
                        println!("[OK] {:02} - {}", tr.track_number, tr.title);
                    }
                    Err(e) => {
                        if let Ok(mut g) = errors.lock() {
                            g.push(format!("{:02} - {}: {:#}", tr.track_number, tr.title, e));
                        }
                    }
                }
            });
        });

        let err_list = errors.into_inner().unwrap_or_default();
        err_count = err_list.len();
        println!(
            "\nResumen CUE -> MP4 en {:.1}s: {} renderizado(s), {} omitido(s), {} error(es).",
            started.elapsed().as_secs_f64(),
            ok_count.load(Ordering::Relaxed),
            skipped_count,
            err_count
        );
        for e in err_list {
            eprintln!("  - {}", e);
        }
    }

    let upload_items = build_upload_items_for_cue(
        &all_pairs,
        &cue_sheet,
        album_meta,
        title_tpl,
        desc_tpl,
    );
    for item in &upload_items {
        let _ = write_video_sidecar_txt(item);
    }
    if let Ok(master) = write_master_tracklist_txt(&cfg.output, &upload_items) {
        println!("Metadata YouTube guardada en: {}", master.display());
    }

    if youtube_upload {
        let pl_title_opt = playlist_meta_tuple(youtube_playlist, album_meta);
        let summary = youtube::upload_videos_batch(
            &upload_items,
            &cfg.output,
            client_secret,
            yt_privacy,
            pl_title_opt.as_ref().map(|(t, d)| (t.as_str(), d.as_str())),
            None,
            |line| println!("{}", line),
            |done, total, msg| println!("YouTube [{}/{}] {}", done, total, msg),
        )?;
        println!(
            "\nResumen YouTube: {} subido(s), {} omitido(s), {} error(es).",
            summary.uploaded_count, summary.skipped_count, summary.err_count
        );
        err_count += summary.err_count;
    }

    if err_count > 0 {
        bail!("Finalizó con {} error(es).", err_count);
    }

    Ok(())
}

#[cfg(windows)]
fn attach_parent_console_if_needed() {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
    };

    unsafe {
        let stdout_handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if stdout_handle.is_null() || stdout_handle == INVALID_HANDLE_VALUE {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn main() {
    if std::env::args_os().len() > 1 {
        #[cfg(windows)]
        attach_parent_console_if_needed();

        if let Err(err) = run_cli() {
            eprintln!("Error: {:#}", err);
            std::process::exit(1);
        }
    } else if let Err(err) = gui::run_gui() {
        gui::show_fatal_error(&format!("{:#}", err));
        std::process::exit(1);
    }
}
