use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::new_hidden_command;
use crate::vgmdb::percent_encode;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const YOUTUBE_UPLOAD_URL: &str =
    "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status";
const YOUTUBE_PLAYLISTS_URL: &str =
    "https://www.googleapis.com/youtube/v3/playlists?part=snippet,status";
const YOUTUBE_PLAYLIST_ITEMS_URL: &str =
    "https://www.googleapis.com/youtube/v3/playlistItems?part=snippet";
const YOUTUBE_SCOPES: &str =
    "https://www.googleapis.com/auth/youtube.upload https://www.googleapis.com/auth/youtube";

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum PrivacyStatus {
    Unlisted,
    Private,
    Public,
}

impl PrivacyStatus {
    pub fn as_api_str(self) -> &'static str {
        match self {
            PrivacyStatus::Unlisted => "unlisted",
            PrivacyStatus::Private => "private",
            PrivacyStatus::Public => "public",
        }
    }

    pub fn from_str_lossy(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "unlisted" | "oculto" | "nolistado" => Ok(PrivacyStatus::Unlisted),
            "private" | "privado" => Ok(PrivacyStatus::Private),
            "public" | "publico" | "público" => Ok(PrivacyStatus::Public),
            other => bail!(
                "Privacidad inválida '{}'. Usá 'unlisted', 'private' o 'public'.",
                other
            ),
        }
    }
}

#[derive(Deserialize)]
struct ClientSecretOuter {
    installed: Option<ClientCredentials>,
    web: Option<ClientCredentials>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TokenStore {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct UploadedVideoRecord {
    pub video_id: String,
    pub title: String,
    pub privacy: String,
    pub uploaded_at_epoch: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct UploadHistory {
    pub videos: BTreeMap<String, UploadedVideoRecord>,
}

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn make_temp_path(prefix: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("mp3video_{}_{}_{}.{}", prefix, std::process::id(), nanos, ext))
}

pub(crate) fn default_token_store_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("youtube_tokens.json");
        }
    }
    PathBuf::from("youtube_tokens.json")
}

pub(crate) fn load_client_credentials(path: &Path) -> Result<ClientCredentials> {
    if !path.is_file() {
        bail!(
            "No se encontró el archivo client_secret.json en: {}",
            path.display()
        );
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("No se pudo leer {}", path.display()))?;
    let parsed: ClientSecretOuter = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .with_context(|| format!("Formato inválido en {}", path.display()))?;
    parsed
        .installed
        .or(parsed.web)
        .context("El archivo client_secret.json debe contener un objeto 'installed' o 'web'.")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn open_url_in_browser(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let op_w: Vec<u16> = "open\0".encode_utf16().collect();
        let mut url_w: Vec<u16> = url.encode_utf16().collect();
        url_w.push(0);
        let res = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                op_w.as_ptr(),
                url_w.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            )
        };
        if (res as isize) <= 32 {
            bail!("Windows no pudo abrir el navegador predeterminado para autorizar Google.");
        }
    }
    Ok(())
}

fn send_http_response(stream: &mut std::net::TcpStream, status: &str, body_html: &str) {
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body_html.len(),
        body_html
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn wait_for_oauth_code(listener: &TcpListener, cancel_flag: Option<&AtomicBool>) -> Result<String> {
    listener
        .set_nonblocking(true)
        .context("No se pudo configurar el socket OAuth")?;
    let deadline = Instant::now() + Duration::from_secs(180);

    loop {
        if let Some(flag) = cancel_flag {
            if flag.load(Ordering::Relaxed) {
                bail!("Autorización cancelada por el usuario.");
            }
        }
        if Instant::now() > deadline {
            bail!("Tiempo de espera agotado (180s) esperando autorización en el navegador.");
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let target = first_line.split_whitespace().nth(1).unwrap_or("/");

                if target.starts_with("/favicon.ico") {
                    send_http_response(&mut stream, "404 Not Found", "");
                    continue;
                }

                let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
                let mut code = None;
                let mut error = None;
                for pair in query.split('&') {
                    if let Some((k, v)) = pair.split_once('=') {
                        if k == "code" {
                            code = Some(percent_decode(v));
                        } else if k == "error" {
                            error = Some(percent_decode(v));
                        }
                    }
                }

                if let Some(err) = error {
                    send_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "<html><body><h2>Autorización denegada</h2><p>Podés cerrar esta ventana.</p></body></html>",
                    );
                    bail!("Google devolvió un error de autorización: {}", err);
                }

                if let Some(c) = code {
                    send_http_response(
                        &mut stream,
                        "200 OK",
                        "<html><body style='font-family:Segoe UI,sans-serif;padding:24px'><h2>Autorización completada</h2><p>Ya podés volver a <b>MP3 a Video</b> y cerrar esta pestaña.</p></body></html>",
                    );
                    return Ok(c);
                }

                send_http_response(
                    &mut stream,
                    "400 Bad Request",
                    "<html><body><p>Solicitud OAuth inválida.</p></body></html>",
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(err) => {
                bail!("Error escuchando callback OAuth: {}", err);
            }
        }
    }
}

fn curl_post_form(url: &str, params: &[(&str, &str)]) -> Result<Value> {
    let encoded_body: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let mut cmd = new_hidden_command(Path::new("curl.exe"));
    cmd.arg("-s")
        .arg("-S")
        .arg("-X")
        .arg("POST")
        .arg("-m")
        .arg("20")
        .arg("-H")
        .arg("Content-Type: application/x-www-form-urlencoded")
        .arg("--data-raw")
        .arg(&encoded_body)
        .arg("--write-out")
        .arg("\n%{http_code}")
        .arg(url);

    let output = cmd.output().context("No se pudo ejecutar curl.exe")?;
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let (body, code) = split_http_code(&raw);

    if !output.status.success() || code >= 400 {
        bail!("Error OAuth HTTP {}: {}", code, body);
    }

    serde_json::from_str(body).context("Respuesta JSON inválida desde Google OAuth")
}

fn split_http_code(raw: &str) -> (&str, u16) {
    let trimmed = raw.trim_end_matches(['\r', '\n']);
    if let Some((body, code_str)) = trimmed.rsplit_once('\n') {
        if let Ok(code) = code_str.trim().parse::<u16>() {
            return (body.trim(), code);
        }
    }
    if let Ok(code) = trimmed.parse::<u16>() {
        return ("", code);
    }
    (trimmed, 0)
}

fn save_tokens(token_path: &Path, tokens: &TokenStore) -> Result<()> {
    if let Some(parent) = token_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(tokens)?;
    fs::write(token_path, json)
        .with_context(|| format!("No se pudo guardar {}", token_path.display()))
}

pub(crate) fn has_saved_tokens(token_path: &Path) -> bool {
    token_path.is_file()
}

pub(crate) fn authorize_interactive(
    client_secret_path: &Path,
    token_store_path: &Path,
    cancel_flag: Option<&AtomicBool>,
) -> Result<TokenStore> {
    let creds = load_client_credentials(client_secret_path)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .context("No se pudo abrir puerto local para recibir el código OAuth")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{}", port);

    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent",
        GOOGLE_AUTH_URL,
        percent_encode(&creds.client_id),
        percent_encode(&redirect_uri),
        percent_encode(YOUTUBE_SCOPES),
    );

    open_url_in_browser(&auth_url)?;
    let code = wait_for_oauth_code(&listener, cancel_flag)?;

    let resp = curl_post_form(
        GOOGLE_TOKEN_URL,
        &[
            ("client_id", &creds.client_id),
            ("client_secret", &creds.client_secret),
            ("code", &code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", &redirect_uri),
        ],
    )?;

    let access_token = resp
        .get("access_token")
        .and_then(Value::as_str)
        .context("Google no devolvió access_token")?
        .to_string();
    let refresh_token = resp
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expires_in = resp.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);

    let store = TokenStore {
        access_token,
        refresh_token,
        expires_at_epoch: now_epoch() + expires_in,
    };
    save_tokens(token_store_path, &store)?;
    Ok(store)
}

pub(crate) fn get_valid_access_token(
    client_secret_path: &Path,
    token_store_path: &Path,
) -> Result<String> {
    if !token_store_path.is_file() {
        bail!(
            "No hay sesión de YouTube conectada. Presioná 'Conectar YouTube' primero (se guardará en {}).",
            token_store_path.display()
        );
    }

    let raw = fs::read_to_string(token_store_path)
        .with_context(|| format!("No se pudo leer {}", token_store_path.display()))?;
    let mut store: TokenStore = serde_json::from_str(&raw)
        .with_context(|| format!("Formato inválido en {}", token_store_path.display()))?;

    if store.expires_at_epoch > now_epoch() + 60 && !store.access_token.is_empty() {
        return Ok(store.access_token);
    }

    if store.refresh_token.is_empty() {
        bail!("El token expiró y no hay refresh_token. Volvé a presionar 'Conectar YouTube'.");
    }

    let creds = load_client_credentials(client_secret_path)?;
    let resp = curl_post_form(
        GOOGLE_TOKEN_URL,
        &[
            ("client_id", &creds.client_id),
            ("client_secret", &creds.client_secret),
            ("refresh_token", &store.refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )?;

    let new_access = resp
        .get("access_token")
        .and_then(Value::as_str)
        .context("No se pudo renovar el access_token de Google")?
        .to_string();
    let expires_in = resp.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);

    store.access_token = new_access.clone();
    store.expires_at_epoch = now_epoch() + expires_in;
    save_tokens(token_store_path, &store)?;
    Ok(new_access)
}

fn format_youtube_api_error(http_code: u16, body: &str) -> String {
    if body.contains("quotaExceeded") {
        return format!(
            "Cuota diaria de YouTube API agotada (HTTP {}). Podés reanudar mañana sin perder el progreso.",
            http_code
        );
    }
    if body.contains("uploadLimitExceeded") {
        return format!(
            "Se alcanzó el límite diario de subidas del canal en YouTube (HTTP {}).",
            http_code
        );
    }
    if let Ok(val) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = val
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return format!("YouTube API HTTP {}: {}", http_code, msg);
        }
    }
    format!("YouTube API HTTP {}: {}", http_code, body)
}

fn curl_post_json(url: &str, access_token: &str, payload: &Value) -> Result<Value> {
    let json_path = make_temp_path("yt_json", "json");
    let _guard = TempFileGuard(json_path.clone());
    fs::write(&json_path, serde_json::to_vec(payload)?)?;

    let mut cmd = new_hidden_command(Path::new("curl.exe"));
    cmd.arg("-s")
        .arg("-S")
        .arg("-X")
        .arg("POST")
        .arg("-m")
        .arg("30")
        .arg("-H")
        .arg(format!("Authorization: Bearer {}", access_token))
        .arg("-H")
        .arg("Content-Type: application/json; charset=UTF-8")
        .arg("--data-binary")
        .arg(format!("@{}", json_path.display()))
        .arg("--write-out")
        .arg("\n%{http_code}")
        .arg(url);

    let output = cmd.output().context("No se pudo ejecutar curl.exe")?;
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let (body, code) = split_http_code(&raw);

    if !output.status.success() || code >= 400 {
        bail!("{}", format_youtube_api_error(code, body));
    }

    serde_json::from_str(body).context("Respuesta JSON inválida desde YouTube API")
}

pub(crate) fn upload_video_resumable(
    access_token: &str,
    mp4_path: &Path,
    title: &str,
    description: &str,
    tags: &[String],
    privacy: PrivacyStatus,
) -> Result<String> {
    let meta = fs::metadata(mp4_path)
        .with_context(|| format!("No se pudo leer {}", mp4_path.display()))?;
    let file_len = meta.len();

    let snippet_body = json!({
        "snippet": {
            "title": title,
            "description": description,
            "tags": tags,
            "categoryId": "10"
        },
        "status": {
            "privacyStatus": privacy.as_api_str(),
            "selfDeclaredMadeForKids": false
        }
    });

    let meta_path = make_temp_path("yt_meta", "json");
    let headers_path = make_temp_path("yt_hdr", "txt");
    let _meta_guard = TempFileGuard(meta_path.clone());
    let _hdr_guard = TempFileGuard(headers_path.clone());
    fs::write(&meta_path, serde_json::to_vec(&snippet_body)?)?;

    // Paso 1: iniciar sesión resumable
    let mut init_cmd = new_hidden_command(Path::new("curl.exe"));
    init_cmd
        .arg("-s")
        .arg("-S")
        .arg("-D")
        .arg(&headers_path)
        .arg("-X")
        .arg("POST")
        .arg("-m")
        .arg("30")
        .arg("-H")
        .arg(format!("Authorization: Bearer {}", access_token))
        .arg("-H")
        .arg("Content-Type: application/json; charset=UTF-8")
        .arg("-H")
        .arg("X-Upload-Content-Type: video/mp4")
        .arg("-H")
        .arg(format!("X-Upload-Content-Length: {}", file_len))
        .arg("--data-binary")
        .arg(format!("@{}", meta_path.display()))
        .arg("--write-out")
        .arg("\n%{http_code}")
        .arg(YOUTUBE_UPLOAD_URL);

    let init_out = init_cmd
        .output()
        .context("No se pudo ejecutar curl.exe para iniciar subida a YouTube")?;
    let init_raw = String::from_utf8_lossy(&init_out.stdout).to_string();
    let (init_body, init_code) = split_http_code(&init_raw);

    if !init_out.status.success() || init_code >= 400 {
        bail!("{}", format_youtube_api_error(init_code, init_body));
    }

    let headers_text = fs::read_to_string(&headers_path).unwrap_or_default();
    let upload_url = headers_text
        .lines()
        .find_map(|line| {
            let (name, val) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("location") {
                Some(val.trim().to_string())
            } else {
                None
            }
        })
        .context("YouTube no devolvió el encabezado Location para la subida resumable")?;

    // Paso 2: subir el archivo MP4 en streaming (PUT)
    let mut put_cmd = new_hidden_command(Path::new("curl.exe"));
    put_cmd
        .arg("-s")
        .arg("-S")
        .arg("-H")
        .arg(format!("Authorization: Bearer {}", access_token))
        .arg("-H")
        .arg("Content-Type: video/mp4")
        .arg("-T")
        .arg(mp4_path)
        .arg("--write-out")
        .arg("\n%{http_code}")
        .arg(&upload_url);

    let put_out = put_cmd
        .output()
        .context("No se pudo ejecutar curl.exe para subir el archivo MP4")?;
    let put_raw = String::from_utf8_lossy(&put_out.stdout).to_string();
    let (put_body, put_code) = split_http_code(&put_raw);

    if !put_out.status.success() || put_code >= 400 {
        bail!("{}", format_youtube_api_error(put_code, put_body));
    }

    let json: Value = serde_json::from_str(put_body)
        .context("La respuesta final de YouTube no contiene JSON válido")?;
    let video_id = json
        .get("id")
        .and_then(Value::as_str)
        .context("La respuesta de YouTube no incluyó el ID del video")?
        .to_string();

    Ok(video_id)
}

pub(crate) fn create_playlist(
    access_token: &str,
    title: &str,
    description: &str,
    privacy: PrivacyStatus,
) -> Result<String> {
    let payload = json!({
        "snippet": {
            "title": title,
            "description": description
        },
        "status": {
            "privacyStatus": privacy.as_api_str()
        }
    });
    let resp = curl_post_json(YOUTUBE_PLAYLISTS_URL, access_token, &payload)?;
    let id = resp
        .get("id")
        .and_then(Value::as_str)
        .context("YouTube no devolvió ID de playlist")?
        .to_string();
    Ok(id)
}

pub(crate) fn add_video_to_playlist(
    access_token: &str,
    playlist_id: &str,
    video_id: &str,
) -> Result<()> {
    let payload = json!({
        "snippet": {
            "playlistId": playlist_id,
            "resourceId": {
                "kind": "youtube#video",
                "videoId": video_id
            }
        }
    });
    let _ = curl_post_json(YOUTUBE_PLAYLIST_ITEMS_URL, access_token, &payload)?;
    Ok(())
}

pub(crate) fn history_path(output_dir: &Path) -> PathBuf {
    output_dir.join("uploaded_youtube.json")
}

pub(crate) fn load_upload_history(output_dir: &Path) -> UploadHistory {
    let path = history_path(output_dir);
    let Ok(raw) = fs::read_to_string(&path) else {
        return UploadHistory::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub(crate) fn save_upload_history(output_dir: &Path, history: &UploadHistory) -> Result<()> {
    let path = history_path(output_dir);
    let json = serde_json::to_string_pretty(history)?;
    fs::write(&path, json).with_context(|| format!("No se pudo guardar {}", path.display()))
}

fn history_key(output_dir: &Path, mp4_path: &Path) -> String {
    if let Ok(rel) = mp4_path.strip_prefix(output_dir) {
        let s = rel.to_string_lossy().replace('\\', "/");
        if !s.is_empty() {
            return s;
        }
    }
    mp4_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| mp4_path.to_string_lossy().to_string())
}

pub(crate) fn record_uploaded_video(
    output_dir: &Path,
    history: &mut UploadHistory,
    mp4_path: &Path,
    video_id: &str,
    title: &str,
    privacy: PrivacyStatus,
) -> Result<()> {
    let key = history_key(output_dir, mp4_path);
    history.videos.insert(
        key,
        UploadedVideoRecord {
            video_id: video_id.to_string(),
            title: title.to_string(),
            privacy: privacy.as_api_str().to_string(),
            uploaded_at_epoch: now_epoch(),
        },
    );
    save_upload_history(output_dir, history)
}

pub(crate) fn find_uploaded_record<'a>(
    history: &'a UploadHistory,
    output_dir: &Path,
    mp4_path: &Path,
) -> Option<&'a UploadedVideoRecord> {
    let key = history_key(output_dir, mp4_path);
    history.videos.get(&key)
}

pub(crate) struct UploadItem {
    pub mp4_path: PathBuf,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
}

pub(crate) struct UploadBatchSummary {
    pub uploaded_count: usize,
    pub skipped_count: usize,
    pub err_count: usize,
}

pub(crate) fn upload_videos_batch<FLog, FProg>(
    items: &[UploadItem],
    output_dir: &Path,
    client_secret_path: &Path,
    privacy: PrivacyStatus,
    playlist_title: Option<(&str, &str)>,
    cancel_flag: Option<&AtomicBool>,
    mut on_log: FLog,
    mut on_progress: FProg,
) -> Result<UploadBatchSummary>
where
    FLog: FnMut(&str),
    FProg: FnMut(usize, usize, &str),
{
    if items.is_empty() {
        return Ok(UploadBatchSummary {
            uploaded_count: 0,
            skipped_count: 0,
            err_count: 0,
        });
    }

    let token_path = default_token_store_path();
    let access_token = get_valid_access_token(client_secret_path, &token_path)?;
    let mut history = load_upload_history(output_dir);

    let mut playlist_id: Option<String> = None;
    if let Some((pl_title, pl_desc)) = playlist_title {
        if !pl_title.trim().is_empty() {
            match create_playlist(&access_token, pl_title.trim(), pl_desc, privacy) {
                Ok(id) => {
                    on_log(&format!(
                        "[YOUTUBE] Playlist creada: '{}' (https://www.youtube.com/playlist?list={})",
                        pl_title.trim(),
                        id
                    ));
                    playlist_id = Some(id);
                }
                Err(err) => {
                    on_log(&format!(
                        "[YOUTUBE AVISO] No se pudo crear la playlist: {:#}",
                        err
                    ));
                }
            }
        }
    }

    let total = items.len();
    let mut uploaded_count = 0usize;
    let mut skipped_count = 0usize;
    let mut err_count = 0usize;

    for (idx, item) in items.iter().enumerate() {
        if let Some(flag) = cancel_flag {
            if flag.load(Ordering::Relaxed) {
                break;
            }
        }

        let name = item
            .mp4_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| item.mp4_path.display().to_string());

        if let Some(existing) = find_uploaded_record(&history, output_dir, &item.mp4_path) {
            skipped_count += 1;
            on_log(&format!(
                "[YOUTUBE OMITIDO] {} ya subido (https://youtu.be/{})",
                name, existing.video_id
            ));
            on_progress(idx + 1, total, &name);
            continue;
        }

        on_progress(idx, total, &format!("Subiendo: {}", item.title));
        match upload_video_resumable(
            &access_token,
            &item.mp4_path,
            &item.title,
            &item.description,
            &item.tags,
            privacy,
        ) {
            Ok(video_id) => {
                uploaded_count += 1;
                let _ = record_uploaded_video(
                    output_dir,
                    &mut history,
                    &item.mp4_path,
                    &video_id,
                    &item.title,
                    privacy,
                );
                on_log(&format!(
                    "[YOUTUBE OK] {} -> '{}' (https://youtu.be/{})",
                    name, item.title, video_id
                ));
                if let Some(ref pl_id) = playlist_id {
                    if let Err(pl_err) = add_video_to_playlist(&access_token, pl_id, &video_id) {
                        on_log(&format!(
                            "[YOUTUBE AVISO] Subido pero no agregado a playlist: {:#}",
                            pl_err
                        ));
                    }
                }
            }
            Err(err) => {
                err_count += 1;
                let msg = format!("{:#}", err);
                on_log(&format!("[YOUTUBE ERROR] {}: {}", name, msg));
                if msg.contains("Cuota diaria") || msg.contains("límite diario") {
                    break;
                }
            }
        }
        on_progress(idx + 1, total, &name);
    }

    Ok(UploadBatchSummary {
        uploaded_count,
        skipped_count,
        err_count,
    })
}
