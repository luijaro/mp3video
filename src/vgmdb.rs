use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{find_ffprobe, new_hidden_command};

const MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org/ws/2";
const DISCOGS_BASE_URL: &str = "https://api.discogs.com";
const USER_AGENT: &str = "mp3video/0.1.0 (https://github.com/luijaro/mp3video)";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TrackInfo {
    pub disc_number: u32,
    pub track_number: u32,
    pub title: String,
    pub duration: String,
    pub composers: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct AlbumMetadata {
    pub source: String,
    pub album_title: String,
    pub catalog: String,
    pub release_date: String,
    pub publisher: String,
    pub composers: Vec<String>,
    pub arrangers: Vec<String>,
    pub performers: Vec<String>,
    pub lyricists: Vec<String>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Mp3Id3Tags {
    pub title: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub artist: Option<String>,
    pub composer: Option<String>,
    pub album: Option<String>,
    pub date: Option<String>,
}

pub(crate) fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

fn curl_get_json(url: &str, timeout_secs: u32) -> Result<Value> {
    let mut cmd = new_hidden_command(Path::new("curl.exe"));
    cmd.arg("-s")
        .arg("-S")
        .arg("-L")
        .arg("--fail-with-body")
        .arg("-m")
        .arg(timeout_secs.to_string())
        .arg("-A")
        .arg(USER_AGENT)
        .arg("-H")
        .arg("Accept: application/json")
        .arg(url);

    let output = cmd
        .output()
        .with_context(|| format!("No se pudo ejecutar curl.exe para consultar {}", url))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        bail!("Error HTTP consultando {}: {}", url, msg);
    }

    let body = String::from_utf8_lossy(&output.stdout);
    let val: Value = serde_json::from_str(&body)
        .with_context(|| format!("La respuesta de {} no es un JSON válido", url))?;

    if let Some(err_msg) = val.get("error").and_then(Value::as_str) {
        bail!("Respuesta de API con error: {}", err_msg);
    }

    Ok(val)
}

fn curl_get_json_retry(url: &str, timeout_secs: u32, retries: u32) -> Result<Value> {
    let mut last_err = None;
    for attempt in 0..=retries {
        if attempt > 0 {
            let wait_ms = 1800 * (attempt + 1) as u64;
            std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        }
        match curl_get_json(url, timeout_secs) {
            Ok(val) => return Ok(val),
            Err(err) => {
                let msg = format!("{:#}", err);
                let retryable = msg.contains("503")
                    || msg.contains("429")
                    || msg.contains("busy")
                    || msg.contains("rate limit")
                    || msg.contains("timed out")
                    || msg.contains("Connection refused");
                last_err = Some(err);
                if !retryable {
                    break;
                }
            }
        }
    }
    Err(last_err.unwrap())
}

fn pick_preferred_name(val: &Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        return None;
    }

    let obj = val.as_object()?;
    for key in ["en", "English", "ja-latn", "romaji", "Romaji", "ja", "Japanese"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    obj.values()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn extract_vgmdb_people(arr: Option<&Value>) -> Vec<String> {
    let Some(items) = arr.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();

    for item in items {
        let name = item
            .get("names")
            .and_then(pick_preferred_name)
            .or_else(|| pick_preferred_name(item));
        if let Some(n) = name {
            if seen.insert(n.clone()) {
                result.push(n);
            }
        }
    }
    result
}

pub(crate) fn parse_vgmdb_album_json(data: &Value, fallback_catalog: &str) -> Result<AlbumMetadata> {
    let album_title = data
        .get("names")
        .and_then(pick_preferred_name)
        .or_else(|| {
            data.get("name")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .context("El JSON de VGMdb no contiene título de álbum")?;

    let catalog = data
        .get("catalog")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_catalog)
        .to_string();

    let release_date = data
        .get("release_date")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let publisher = data
        .get("publisher")
        .and_then(|p| {
            p.get("names")
                .and_then(pick_preferred_name)
                .or_else(|| pick_preferred_name(p))
        })
        .or_else(|| {
            data.get("organizations")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|o| o.get("names").and_then(pick_preferred_name))
        })
        .unwrap_or_default();

    let composers = extract_vgmdb_people(data.get("composers"));
    let arrangers = extract_vgmdb_people(data.get("arrangers"));
    let performers = extract_vgmdb_people(data.get("performers"));
    let lyricists = extract_vgmdb_people(data.get("lyricists"));

    let mut tracks = Vec::new();
    if let Some(discs) = data.get("discs").and_then(Value::as_array) {
        for (disc_idx, disc) in discs.iter().enumerate() {
            let disc_number = (disc_idx + 1) as u32;
            if let Some(track_arr) = disc.get("tracks").and_then(Value::as_array) {
                for (t_idx, tr) in track_arr.iter().enumerate() {
                    let track_number = tr
                        .get("track")
                        .and_then(|v| {
                            v.as_u64()
                                .map(|n| n as u32)
                                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u32>().ok()))
                        })
                        .unwrap_or((t_idx + 1) as u32);
                    let title = tr
                        .get("names")
                        .and_then(pick_preferred_name)
                        .or_else(|| {
                            tr.get("name")
                                .and_then(Value::as_str)
                                .map(|s| s.trim().to_string())
                        })
                        .unwrap_or_else(|| format!("Pista {}", track_number));
                    let duration = tr
                        .get("track_length")
                        .or_else(|| tr.get("length"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    tracks.push(TrackInfo {
                        disc_number,
                        track_number,
                        title,
                        duration,
                        composers: composers.clone(),
                    });
                }
            }
        }
    }

    Ok(AlbumMetadata {
        source: "VGMdb".to_string(),
        album_title,
        catalog,
        release_date,
        publisher,
        composers,
        arrangers,
        performers,
        lyricists,
        tracks,
    })
}

pub(crate) fn normalize_catalog(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogVariants {
    pub raw: String,
    pub is_range: bool,
    pub base_catalog: String,
    pub individual_discs: Vec<String>,
    pub normalized_set: Vec<String>,
}

impl CatalogVariants {
    pub fn matches(&self, candidate: &str) -> bool {
        let cand_norm = normalize_catalog(candidate);
        if cand_norm.is_empty() {
            return false;
        }

        // Coincidencia directa contra cualquiera de las variantes normalizadas
        if self.normalized_set.iter().any(|v| v == &cand_norm) {
            return true;
        }

        // Expandir también el candidato por si contiene rango (ej: candidato KECH-1346~7 y búsqueda KECH-1346)
        let cand_vars = expand_catalog_variants(candidate);
        if cand_vars
            .normalized_set
            .iter()
            .any(|c| self.normalized_set.iter().any(|v| v == c))
        {
            return true;
        }

        // Si ambos comparten el mismo catálogo base no vacío
        if !self.base_catalog.is_empty() && !cand_vars.base_catalog.is_empty() {
            let base_self_norm = normalize_catalog(&self.base_catalog);
            let base_cand_norm = normalize_catalog(&cand_vars.base_catalog);
            if base_self_norm == base_cand_norm {
                return true;
            }
        }

        false
    }
}

pub(crate) fn parse_catalog_range(raw: &str) -> Option<(String, u32, u32, usize)> {
    let normalized = raw.replace('\u{ff5e}', "~"); // normalizar tilde fullwidth ～ a ~

    let (left, right) = if let Some((l, r)) = normalized.split_once('~') {
        (l.trim(), r.trim())
    } else if let Some((l, r)) = normalized.split_once('/') {
        (l.trim(), r.trim())
    } else if let Some(last_dash_idx) = normalized.rfind('-') {
        // Solo tratar '-' como separador de rango si existe un guión anterior de prefijo (ej: PCCG-00123-4)
        if normalized[..last_dash_idx].contains('-') {
            let l = normalized[..last_dash_idx].trim();
            let r = normalized[last_dash_idx + 1..].trim();
            (l, r)
        } else {
            return None;
        }
    } else {
        return None;
    };

    if left.is_empty() || right.is_empty() {
        return None;
    }

    // Buscar dígitos numéricos al final de `left`
    let num_start = left
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    if num_start >= left.len() {
        return None;
    }

    let prefix = &left[..num_start];
    let start_digits = &left[num_start..];
    let start_num: u32 = start_digits.parse().ok()?;
    let digit_len = start_digits.len();

    if !right.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let end_num: u32 = if right.len() < digit_len {
        let keep_len = digit_len - right.len();
        let prefix_digits = &start_digits[..keep_len];
        let end_str = format!("{}{}", prefix_digits, right);
        end_str.parse().ok()?
    } else if right.len() == digit_len {
        right.parse().ok()?
    } else {
        return None;
    };

    if end_num >= start_num && (end_num - start_num) <= 30 {
        Some((prefix.to_string(), start_num, end_num, digit_len))
    } else {
        None
    }
}

pub(crate) fn expand_catalog_variants(raw: &str) -> CatalogVariants {
    let trimmed = raw.trim();
    if let Some((prefix, start_num, end_num, digit_len)) = parse_catalog_range(trimmed) {
        let base_catalog = format!("{}{:0width$}", prefix, start_num, width = digit_len);
        let mut individual_discs = Vec::new();
        for n in start_num..=end_num {
            individual_discs.push(format!("{}{:0width$}", prefix, n, width = digit_len));
        }

        let mut normalized_set = Vec::new();
        let norm_raw = normalize_catalog(trimmed);
        if !norm_raw.is_empty() {
            normalized_set.push(norm_raw);
        }
        let norm_base = normalize_catalog(&base_catalog);
        if !normalized_set.contains(&norm_base) {
            normalized_set.push(norm_base);
        }
        for disc in &individual_discs {
            let nd = normalize_catalog(disc);
            if !normalized_set.contains(&nd) {
                normalized_set.push(nd);
            }
        }
        let full_range = format!(
            "{}{:0width$}{:0width$}",
            prefix,
            start_num,
            end_num,
            width = digit_len
        );
        let nd_full = normalize_catalog(&full_range);
        if !normalized_set.contains(&nd_full) {
            normalized_set.push(nd_full);
        }

        CatalogVariants {
            raw: trimmed.to_string(),
            is_range: true,
            base_catalog,
            individual_discs,
            normalized_set,
        }
    } else {
        let norm = normalize_catalog(trimmed);
        CatalogVariants {
            raw: trimmed.to_string(),
            is_range: false,
            base_catalog: trimmed.to_string(),
            individual_discs: vec![trimmed.to_string()],
            normalized_set: if norm.is_empty() {
                Vec::new()
            } else {
                vec![norm]
            },
        }
    }
}

pub(crate) fn extract_vgmdb_album_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Si ya son solo dígitos (ej: "10200")
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Some(trimmed.to_string());
    }

    // Si es una URL o ruta con "album/<id>" (ej: "https://vgmdb.net/album/10200", "vgmdb.net/album/10200" o "album/10200")
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("discogs.com") {
        return None;
    }
    if let Some(pos) = lower.find("album/") {
        let after = &trimmed[pos + "album/".len()..];
        let id: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !id.is_empty() {
            return Some(id);
        }
    }

    None
}

pub(crate) fn build_vgmdb_browser_url(query: &str) -> String {
    if let Some(id) = extract_vgmdb_album_id(query) {
        return format!("https://vgmdb.net/album/{}", id);
    }
    let trimmed = query.trim();
    if !trimmed.is_empty() && !trimmed.contains('\n') && !trimmed.contains('\t') {
        return format!("https://vgmdb.net/search?q={}", percent_encode(trimmed));
    }
    "https://vgmdb.net/".to_string()
}

pub(crate) fn extract_discogs_release_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.contains("discogs.com") {
        return None;
    }
    if let Some(pos) = lower.find("/release/") {
        let after = &trimmed[pos + "/release/".len()..];
        let id: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

pub(crate) fn looks_like_vgmdb_html(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("<html")
        || lower.contains("<!doctype html")
        || lower.contains("id=\"album_infobit_large\"")
        || lower.contains("id=\"tracklist\"")
        || lower.contains("class=\"albumtitle\"")
}

pub(crate) fn looks_like_vgmdb_copied_text(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() < 3 {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    let numbered_lines = lines
        .iter()
        .filter(|l| parse_copied_track_line(l, &[]).is_some() || parse_copied_track_line_raw(l).is_some())
        .count();

    (lower.contains("catalog number")
        && (lower.contains("tracklist") || lower.contains("release date") || lower.contains("disc 1")))
        || (lower.contains("tracklist") && numbered_lines >= 2)
        || (lower.contains("disc 1") && numbered_lines >= 2)
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() && !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&#x27;", "'")
}

fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
    }
    decode_html_entities(&out)
}

fn extract_between<'a>(input: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start_idx = input.find(start)? + start.len();
    let rest = &input[start_idx..];
    let end_idx = rest.find(end)?;
    Some(&rest[..end_idx])
}

fn remove_html_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        if let Some(end) = rest[start + 4..].find("-->") {
            rest = &rest[start + 4 + end + 3..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

fn remove_tag_blocks(html: &str, tag: &str) -> String {
    let open_pat = format!("<{}", tag.to_ascii_lowercase());
    let close_pat = format!("</{}", tag.to_ascii_lowercase());
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0usize;

    while let Some(rel_start) = lower[cursor..].find(&open_pat) {
        let start = cursor + rel_start;
        // Verify tag boundary
        let after_name = start + open_pat.len();
        if after_name < lower.len() {
            let next_ch = lower.as_bytes()[after_name];
            if !matches!(next_ch, b'>' | b' ' | b'\t' | b'\r' | b'\n' | b'/') {
                out.push_str(&html[cursor..after_name]);
                cursor = after_name;
                continue;
            }
        }
        out.push_str(&html[cursor..start]);
        if let Some(rel_close) = lower[after_name..].find(&close_pat) {
            let close_start = after_name + rel_close;
            if let Some(rel_end) = lower[close_start..].find('>') {
                cursor = close_start + rel_end + 1;
                continue;
            }
        }
        return out;
    }
    out.push_str(&html[cursor..]);
    out
}

fn remove_elements_matching(html: &str, tag: &str, attr_substr: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open_pat = format!("<{}", tag.to_ascii_lowercase());
    let close_pat = format!("</{}", tag.to_ascii_lowercase());
    let attr_lower = attr_substr.to_ascii_lowercase();

    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while let Some(rel_start) = lower[cursor..].find(&open_pat) {
        let start = cursor + rel_start;
        let after_name = start + open_pat.len();
        if after_name < lower.len() {
            let next_ch = lower.as_bytes()[after_name];
            if !matches!(next_ch, b'>' | b' ' | b'\t' | b'\r' | b'\n' | b'/') {
                out.push_str(&html[cursor..after_name]);
                cursor = after_name;
                continue;
            }
        }
        let Some(rel_gt) = lower[start..].find('>') else {
            break;
        };
        let tag_end = start + rel_gt + 1;
        let open_tag_lower = &lower[start..tag_end];
        if !open_tag_lower.contains(&attr_lower) {
            out.push_str(&html[cursor..tag_end]);
            cursor = tag_end;
            continue;
        }

        out.push_str(&html[cursor..start]);
        // Self-closing tag check
        if lower[start..tag_end - 1].trim_end().ends_with('/') {
            cursor = tag_end;
            continue;
        }

        // Skip nested <tag> ... </tag> matching depth
        let mut depth = 1usize;
        let mut scan = tag_end;
        while depth > 0 && scan < lower.len() {
            let next_open = lower[scan..].find(&open_pat).map(|o| scan + o);
            let next_close = lower[scan..].find(&close_pat).map(|c| scan + c);
            match (next_open, next_close) {
                (_, None) => {
                    scan = lower.len();
                    break;
                }
                (Some(o), Some(c)) if o < c => {
                    let after = o + open_pat.len();
                    let is_boundary = after >= lower.len()
                        || matches!(
                            lower.as_bytes()[after],
                            b'>' | b' ' | b'\t' | b'\r' | b'\n' | b'/'
                        );
                    if let Some(rel_end) = lower[o..].find('>') {
                        let full_end = o + rel_end + 1;
                        let self_closing = lower[o..full_end - 1].trim_end().ends_with('/');
                        if is_boundary && !self_closing {
                            depth += 1;
                        }
                        scan = full_end;
                    } else {
                        scan = lower.len();
                    }
                }
                (_, Some(c)) => {
                    let after = c + close_pat.len();
                    let is_boundary = after >= lower.len()
                        || matches!(lower.as_bytes()[after], b'>' | b' ' | b'\t' | b'\r' | b'\n');
                    if let Some(rel_end) = lower[c..].find('>') {
                        if is_boundary {
                            depth = depth.saturating_sub(1);
                        }
                        scan = c + rel_end + 1;
                    } else {
                        scan = lower.len();
                    }
                }
            }
        }
        cursor = scan;
    }

    out.push_str(&html[cursor..]);
    out
}

fn strip_vgmdb_hidden_markup(html: &str) -> String {
    let s = remove_elements_matching(html, "div", "vbmenu_popup");
    let s = remove_elements_matching(&s, "span", "display:none");
    remove_elements_matching(&s, "span", "display: none")
}

fn parse_attr_value(tag_lower: &str, tag_orig: &str, attr_name: &str) -> Option<String> {
    let eq_pat = format!("{}=", attr_name.to_ascii_lowercase());
    let pos = tag_lower.find(&eq_pat)? + eq_pat.len();
    let rest = &tag_orig[pos..];
    let rest_trim = rest.trim_start();
    let first = rest_trim.chars().next()?;
    if first == '"' || first == '\'' {
        let inner = &rest_trim[1..];
        let end = inner.find(first)?;
        Some(inner[..end].trim().to_string())
    } else {
        let val: String = rest_trim
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>')
            .collect();
        (!val.is_empty()).then_some(val)
    }
}

fn select_vgmdb_tracklist_block<'a>(html: &'a str) -> &'a str {
    let lower = html.to_ascii_lowercase();

    // Parse #tlnav language tabs: <ul id="tlnav">...<a ... rel='tl1033'>English</a>...</ul>
    let mut tabs: Vec<(String, String)> = Vec::new();
    if let Some(nav_pos) = lower.find("id=\"tlnav\"").or_else(|| lower.find("id='tlnav'")) {
        if let Some(rel_ul_end) = lower[nav_pos..].find("</ul>") {
            let nav_orig = &html[nav_pos..nav_pos + rel_ul_end];
            let nav_lower = &lower[nav_pos..nav_pos + rel_ul_end];
            let mut scan = 0usize;
            while let Some(rel_a) = nav_lower[scan..].find("<a ") {
                let a_start = scan + rel_a;
                let Some(rel_gt) = nav_lower[a_start..].find('>') else {
                    break;
                };
                let tag_end = a_start + rel_gt + 1;
                let Some(rel_close) = nav_lower[tag_end..].find("</a>") else {
                    break;
                };
                let rel_id = parse_attr_value(
                    &nav_lower[a_start..tag_end],
                    &nav_orig[a_start..tag_end],
                    "rel",
                );
                let label = collapse_whitespace(&strip_html_tags(&nav_orig[tag_end..tag_end + rel_close]));
                if let Some(id) = rel_id {
                    tabs.push((id, label));
                }
                scan = tag_end + rel_close + 4;
            }
        }
    }

    let preferred_tab_id = tabs
        .iter()
        .max_by_key(|(_, label)| {
            let l = label.to_ascii_lowercase();
            if l.contains("english") || l == "en" {
                4
            } else if l.contains("romaji") || l.contains("ja-latn") {
                3
            } else if l.contains("japanese") || l == "ja" {
                2
            } else {
                1
            }
        })
        .map(|(id, _)| id.clone());

    let tl_start = lower
        .find("id=\"tracklist\"")
        .or_else(|| lower.find("id='tracklist'"));

    let region = if let Some(start) = tl_start {
        let tail_lower = &lower[start..];
        let end_rel = tail_lower
            .find("<h3>notes")
            .or_else(|| tail_lower.find("id=\"collapse_notes\""))
            .or_else(|| tail_lower.find("id=\"rightcolumn\""))
            .unwrap_or(tail_lower.len());
        &html[start..start + end_rel]
    } else {
        html
    };

    let region_lower = region.to_ascii_lowercase();
    if let Some(tab_id) = preferred_tab_id {
        let id_dq = format!("id=\"{}\"", tab_id.to_ascii_lowercase());
        let id_sq = format!("id='{}'", tab_id.to_ascii_lowercase());
        if let Some(tab_pos) = region_lower.find(&id_dq).or_else(|| region_lower.find(&id_sq)) {
            let after_tab = tab_pos + id_dq.len().min(region.len() - tab_pos);
            let next_tab_rel = region_lower[after_tab..]
                .find("<span class=\"tl\"")
                .or_else(|| region_lower[after_tab..].find("<span class='tl'"));
            return if let Some(rel_end) = next_tab_rel {
                &region[tab_pos..after_tab + rel_end]
            } else {
                &region[tab_pos..]
            };
        }
    }

    // Si hay múltiples <span class="tl" sin #tlnav, tomar solo el primero
    if let Some(first_tl) = region_lower
        .find("<span class=\"tl\"")
        .or_else(|| region_lower.find("<span class='tl'"))
    {
        let after = first_tl + 16;
        if let Some(second_rel) = region_lower[after..]
            .find("<span class=\"tl\"")
            .or_else(|| region_lower[after..].find("<span class='tl'"))
        {
            return &region[first_tl..after + second_rel];
        }
        return &region[first_tl..];
    }

    region
}

fn extract_td_cells_in_tr(tr_html: &str) -> Vec<String> {
    let lower = tr_html.to_ascii_lowercase();
    let mut cells = Vec::new();
    let mut scan = 0usize;
    while let Some(rel_td) = lower[scan..].find("<td") {
        let td_start = scan + rel_td;
        let after_td = td_start + 3;
        if after_td < lower.len()
            && !matches!(
                lower.as_bytes()[after_td],
                b'>' | b' ' | b'\t' | b'\r' | b'\n'
            )
        {
            scan = after_td;
            continue;
        }
        let Some(rel_gt) = lower[td_start..].find('>') else {
            break;
        };
        let content_start = td_start + rel_gt + 1;
        let Some(rel_close) = lower[content_start..].find("</td>") else {
            break;
        };
        let raw_cell = &tr_html[content_start..content_start + rel_close];
        cells.push(collapse_whitespace(&strip_html_tags(raw_cell)));
        scan = content_start + rel_close + 5;
    }
    cells
}

fn parse_disc_header_number(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let after = if lower.starts_with("disc ") {
        trimmed[5..].trim_start()
    } else if lower.starts_with("cd ") {
        trimmed[3..].trim_start()
    } else if lower.starts_with("cd") && trimmed[2..].starts_with(|c: char| c.is_ascii_digit()) {
        trimmed[2..].trim_start()
    } else {
        return None;
    };
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    let num = digits.parse::<u32>().ok()?;
    (num > 0 && num <= 99).then_some(num)
}

fn parse_vgmdb_tracklist_html(tab_html: &str, composers: &[String]) -> Vec<TrackInfo> {
    let cleaned = strip_vgmdb_hidden_markup(tab_html);
    let lower = cleaned.to_ascii_lowercase();
    let mut tracks = Vec::new();
    let mut current_disc = 1u32;
    let mut current_track = 1u32;

    let mut scan = 0usize;
    while scan < lower.len() {
        let next_b = lower[scan..].find("<b>").or_else(|| lower[scan..].find("<b "));
        let next_tr = lower[scan..].find("<tr");

        let b_rel_opt = match (next_b, next_tr) {
            (None, None) => break,
            (Some(b), Some(t)) if b < t => Some(b),
            (Some(b), None) => Some(b),
            _ => None,
        };

        if let Some(b_rel) = b_rel_opt {
            let b_start = scan + b_rel;
            let Some(rel_gt) = lower[b_start..].find('>') else {
                break;
            };
            let content_start = b_start + rel_gt + 1;
            if let Some(rel_close) = lower[content_start..].find("</b>") {
                let b_text = collapse_whitespace(&strip_html_tags(
                    &cleaned[content_start..content_start + rel_close],
                ));
                if let Some(d) = parse_disc_header_number(&b_text) {
                    current_disc = d;
                    current_track = 1;
                }
                scan = content_start + rel_close + 4;
            } else {
                scan = content_start;
            }
        } else if let Some(tr_rel) = next_tr {
            let tr_start = scan + tr_rel;
            let Some(rel_gt) = lower[tr_start..].find('>') else {
                break;
            };
            let tr_tag_end = tr_start + rel_gt + 1;
            let tr_open_lower = &lower[tr_start..tr_tag_end];
            let Some(rel_close) = lower[tr_tag_end..].find("</tr>") else {
                break;
            };
            let tr_inner = &cleaned[tr_tag_end..tr_tag_end + rel_close];
            scan = tr_tag_end + rel_close + 5;

            if !tr_open_lower.contains("rolebit") {
                continue;
            }

            let cells = extract_td_cells_in_tr(tr_inner);
            if cells.len() >= 2 {
                let num_digits: String =
                    cells[0].chars().take_while(|c| c.is_ascii_digit()).collect();
                let title = cells[1].trim().to_string();
                if title.is_empty() {
                    continue;
                }
                let track_number = num_digits
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .unwrap_or(current_track);
                let raw_dur = cells.get(2).map(String::as_str).unwrap_or("").trim();
                let duration = if raw_dur.eq_ignore_ascii_case("unknown") || raw_dur == "-" {
                    String::new()
                } else {
                    raw_dur.to_string()
                };

                tracks.push(TrackInfo {
                    disc_number: current_disc,
                    track_number,
                    title,
                    duration,
                    composers: composers.to_vec(),
                });
                current_track = track_number + 1;
            }
        }
    }

    if !tracks.is_empty() {
        return tracks;
    }

    // Fallback 1: <tr> con 3 celdas donde la celda 0 es número de pista
    let mut fallback_disc = 1u32;
    let mut fallback_scan = 0usize;
    while let Some(tr_rel) = lower[fallback_scan..].find("<tr") {
        let tr_start = fallback_scan + tr_rel;
        let Some(rel_gt) = lower[tr_start..].find('>') else {
            break;
        };
        let tr_tag_end = tr_start + rel_gt + 1;
        let Some(rel_close) = lower[tr_tag_end..].find("</tr>") else {
            break;
        };
        let tr_inner = &cleaned[tr_tag_end..tr_tag_end + rel_close];
        fallback_scan = tr_tag_end + rel_close + 5;
        let cells = extract_td_cells_in_tr(tr_inner);
        if cells.len() >= 3
            && !cells[0].is_empty()
            && cells[0].chars().all(|c| c.is_ascii_digit())
            && !cells[1].is_empty()
        {
            if let Ok(n) = cells[0].parse::<u32>() {
                let raw_dur = cells[2].trim();
                let duration = if raw_dur.eq_ignore_ascii_case("unknown") || raw_dur == "-" {
                    String::new()
                } else {
                    raw_dur.to_string()
                };
                tracks.push(TrackInfo {
                    disc_number: fallback_disc,
                    track_number: n,
                    title: cells[1].clone(),
                    duration,
                    composers: composers.to_vec(),
                });
            }
        } else if cells.len() == 1 {
            if let Some(d) = parse_disc_header_number(&cells[0]) {
                fallback_disc = d;
            }
        }
    }
    if !tracks.is_empty() {
        return tracks;
    }

    // Fallback 2: formato simple con class="trackname" y class="time"
    let mut legacy_disc = 1u32;
    let mut legacy_track = 1u32;
    for line in cleaned.lines() {
        let trimmed_line = line.trim();
        if (trimmed_line.contains("Disc ") || trimmed_line.contains("CD "))
            && (trimmed_line.contains("<span")
                || trimmed_line.contains("<td")
                || trimmed_line.contains("<b"))
        {
            if let Some(pos) = trimmed_line.find("Disc ").or_else(|| trimmed_line.find("CD ")) {
                let after = trimmed_line[pos + 4..].trim_start();
                let num_digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(d) = num_digits.parse::<u32>() {
                    if d > 0 {
                        legacy_disc = d;
                        legacy_track = 1;
                    }
                }
            }
        }

        if trimmed_line.contains("class=\"trackname\"") || trimmed_line.contains("class='trackname'")
        {
            let title = extract_between(trimmed_line, "class=\"trackname\"", "</span>")
                .or_else(|| extract_between(trimmed_line, "class='trackname'", "</span>"))
                .map(|s| {
                    let inside = s.find('>').map(|i| &s[i + 1..]).unwrap_or(s);
                    collapse_whitespace(&strip_html_tags(inside))
                })
                .unwrap_or_default();

            if !title.is_empty() {
                let duration = extract_between(trimmed_line, "class=\"time\"", "</span>")
                    .or_else(|| extract_between(trimmed_line, "class='time'", "</span>"))
                    .map(|s| {
                        let inside = s.find('>').map(|i| &s[i + 1..]).unwrap_or(s);
                        collapse_whitespace(&strip_html_tags(inside))
                    })
                    .unwrap_or_default();

                tracks.push(TrackInfo {
                    disc_number: legacy_disc,
                    track_number: legacy_track,
                    title,
                    duration,
                    composers: composers.to_vec(),
                });
                legacy_track += 1;
            }
        }
    }

    tracks
}

fn split_people_list(raw: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0usize;

    for ch in raw.chars() {
        match ch {
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(ch);
            }
            ',' | '/' if paren_depth == 0 => {
                let clean = current.trim().to_string();
                if !clean.is_empty() {
                    push_unique(&mut result, clean);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let clean = current.trim().to_string();
    if !clean.is_empty() {
        push_unique(&mut result, clean);
    }
    result
}

fn clean_vgmdb_publisher(raw: &str) -> String {
    let trimmed = collapse_whitespace(raw);
    if trimmed.is_empty() {
        return String::new();
    }
    // Ej: "KOEI Co., Ltd. (Publisher), Universal Music (Distributor)" -> "KOEI Co., Ltd."
    let first = trimmed.split("),").next().unwrap_or(&trimmed).trim();
    if let Some(paren_idx) = first.rfind(" (") {
        let role_hint = first[paren_idx..].to_ascii_lowercase();
        if role_hint.contains("publish")
            || role_hint.contains("label")
            || role_hint.contains("distribut")
            || role_hint.contains("manufactur")
            || role_hint.contains("retail")
        {
            return first[..paren_idx].trim().to_string();
        }
    }
    first.trim_end_matches('/').trim().to_string()
}

pub(crate) fn parse_vgmdb_html(html: &str, fallback_catalog: &str) -> Result<AlbumMetadata> {
    let no_scripts = remove_html_comments(&remove_tag_blocks(&remove_tag_blocks(html, "script"), "style"));

    // 1. Extraer tracklist ANTES de eliminar spans display:none (en algunas páginas la pestaña English puede estar oculta)
    let tracklist_region = select_vgmdb_tracklist_block(&no_scripts);

    // 2. Limpiar elementos ocultos y popups para extraer metadatos visibles en inglés
    let visible = strip_vgmdb_hidden_markup(&no_scripts);
    let visible_lower = visible.to_ascii_lowercase();

    // 3. Título del álbum
    let mut album_title = None;
    if let Some(h1_start) = visible_lower.find("<h1") {
        if let Some(h1_end_rel) = visible_lower[h1_start..].find("</h1>") {
            let h1_html = &visible[h1_start..h1_start + h1_end_rel];
            let h1_text = collapse_whitespace(&strip_html_tags(h1_html));
            let clean = h1_text.trim_matches(['/', ' ']).trim().to_string();
            if !clean.is_empty() {
                album_title = Some(clean);
            }
        }
    }
    if album_title.is_none() {
        if let Some(span) = extract_between(&visible, "class=\"albumtitle\"", "</span>") {
            let after_tag = span.find('>').map(|i| &span[i + 1..]).unwrap_or(span);
            let clean = collapse_whitespace(&strip_html_tags(after_tag));
            if !clean.is_empty() {
                album_title = Some(clean);
            }
        }
    }
    if album_title.is_none() {
        if let Some(title_tag) = extract_between(&visible, "<title>", "</title>") {
            let clean = collapse_whitespace(&strip_html_tags(title_tag));
            let without_vgmdb = clean.trim_end_matches("- VGMdb").trim();
            if let Some((_, right)) = without_vgmdb.split_once('|') {
                let t = right.trim().to_string();
                if !t.is_empty() {
                    album_title = Some(t);
                }
            } else if !without_vgmdb.is_empty() {
                album_title = Some(without_vgmdb.to_string());
            }
        }
    }
    let album_title = album_title
        .context("No se pudo encontrar el título del álbum en el HTML de VGMdb")?;

    // 4. Escanear filas <tr> del bloque de información y créditos (antes del tracklist)
    let meta_end = visible_lower
        .find("id=\"tracklist\"")
        .or_else(|| visible_lower.find("id='tracklist'"))
        .unwrap_or(visible.len());
    let meta_html = &visible[..meta_end];
    let meta_lower = &visible_lower[..meta_end];

    let mut catalog = String::new();
    let mut release_date = String::new();
    let mut publisher = String::new();
    let mut label_fallback = String::new();
    let mut composers = Vec::new();
    let mut arrangers = Vec::new();
    let mut performers = Vec::new();
    let mut lyricists = Vec::new();

    let mut scan = 0usize;
    while let Some(tr_rel) = meta_lower[scan..].find("<tr") {
        let tr_start = scan + tr_rel;
        let Some(rel_gt) = meta_lower[tr_start..].find('>') else {
            break;
        };
        let tr_inner_start = tr_start + rel_gt + 1;
        let Some(rel_close) = meta_lower[tr_inner_start..].find("</tr>") else {
            break;
        };
        let tr_inner = &meta_html[tr_inner_start..tr_inner_start + rel_close];
        scan = tr_inner_start + rel_close + 5;

        let cells = extract_td_cells_in_tr(tr_inner);
        if cells.len() < 2 {
            continue;
        }
        let key = cells[0].trim().to_ascii_lowercase();
        let val = cells[1].trim();
        if val.is_empty() {
            continue;
        }

        if key == "catalog number" && catalog.is_empty() {
            let main_cat = val.split('(').next().unwrap_or(val).trim();
            if !main_cat.is_empty() && !main_cat.eq_ignore_ascii_case("n/a") {
                catalog = main_cat.to_string();
            }
        } else if key == "release date" && release_date.is_empty() {
            release_date = val.to_string();
        } else if (key == "publisher" || key == "publish organization" || key == "published by")
            && publisher.is_empty()
        {
            publisher = clean_vgmdb_publisher(val);
        } else if (key == "label" || key == "organizations") && label_fallback.is_empty() {
            label_fallback = clean_vgmdb_publisher(val);
        } else if matches!(
            key.as_str(),
            "composer"
                | "composed by"
                | "all composed by"
                | "all music composed by"
                | "music"
        ) {
            for person in split_people_list(val) {
                push_unique(&mut composers, person);
            }
        } else if matches!(
            key.as_str(),
            "arranger" | "arranged by" | "all arranged by" | "all music arranged by"
        ) {
            for person in split_people_list(val) {
                push_unique(&mut arrangers, person);
            }
        } else if matches!(key.as_str(), "performer" | "performed by" | "vocals") {
            for person in split_people_list(val) {
                push_unique(&mut performers, person);
            }
        } else if matches!(key.as_str(), "lyricist" | "lyrics by") {
            for person in split_people_list(val) {
                push_unique(&mut lyricists, person);
            }
        }
    }

    if publisher.is_empty() {
        publisher = label_fallback;
    }

    if catalog.is_empty() {
        if let Some(title_tag) = extract_between(&visible, "<title>", "</title>") {
            let clean = collapse_whitespace(&strip_html_tags(title_tag));
            if let Some((left, _)) = clean.split_once('|') {
                let cat = left.trim();
                if !cat.is_empty() {
                    catalog = cat.to_string();
                }
            }
        }
    }
    if catalog.is_empty() {
        catalog = fallback_catalog.trim().to_string();
    }

    // 5. Pistas desde la pestaña elegida
    let tracks = parse_vgmdb_tracklist_html(tracklist_region, &composers);

    Ok(AlbumMetadata {
        source: "VGMdb (HTML)".to_string(),
        album_title,
        catalog,
        release_date,
        publisher,
        composers,
        arrangers,
        performers,
        lyricists,
        tracks,
    })
}

fn is_valid_duration_token(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() || t.len() > 10 {
        return false;
    }
    let parts: Vec<&str> = t.split(':').collect();
    if parts.len() != 2 && parts.len() != 3 {
        return false;
    }
    let last_ok = parts.last().is_some_and(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_digit()));
    let others_ok = parts[..parts.len() - 1]
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    last_ok && others_ok
}

fn parse_copied_track_line_raw(line: &str) -> Option<(u32, String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Formato 1: separado por tabulaciones ("01\tOpening\t2:14" o "01\tOpening")
    if trimmed.contains('\t') {
        let cols: Vec<&str> = trimmed.split('\t').map(str::trim).collect();
        if cols.len() >= 2 && !cols[0].is_empty() && cols[0].len() <= 3 && cols[0].chars().all(|c| c.is_ascii_digit()) {
            let track_num: u32 = cols[0].parse().ok()?;
            if track_num == 0 {
                return None;
            }
            let last = *cols.last().unwrap_or(&"");
            let (title_cols, duration) = if cols.len() >= 3 && (is_valid_duration_token(last) || last.eq_ignore_ascii_case("unknown")) {
                let dur = if last.eq_ignore_ascii_case("unknown") { String::new() } else { last.to_string() };
                (&cols[1..cols.len() - 1], dur)
            } else {
                (&cols[1..], String::new())
            };
            let title = collapse_whitespace(&title_cols.join(" "));
            if !title.is_empty() {
                return Some((track_num, title, duration));
            }
        }
    }

    // Formato 2: "01 Opening 2:14" o "01. Opening 2:14"
    let num_len = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if (1..=3).contains(&num_len) && trimmed.len() > num_len {
        let rest_after_num = &trimmed[num_len..];
        let sep_len = if rest_after_num.starts_with(". ") || rest_after_num.starts_with(") ") || rest_after_num.starts_with(".\t") {
            2
        } else if rest_after_num.starts_with(' ') || rest_after_num.starts_with('\t') {
            1
        } else {
            0
        };
        if sep_len > 0 {
            let track_num: u32 = trimmed[..num_len].parse().ok()?;
            if track_num == 0 {
                return None;
            }
            let body = rest_after_num[sep_len..].trim();
            if let Some(last_ws) = body.rfind(char::is_whitespace) {
                let candidate_dur = body[last_ws..].trim();
                let candidate_title = body[..last_ws].trim();
                if !candidate_title.is_empty() && is_valid_duration_token(candidate_dur) {
                    return Some((track_num, collapse_whitespace(candidate_title), candidate_dur.to_string()));
                }
            }
            // Formato 3: dos o más espacios tras el número sin duración ("01  Opening")
            if rest_after_num.starts_with("  ") {
                let clean_title = collapse_whitespace(body);
                if !clean_title.is_empty() {
                    return Some((track_num, clean_title, String::new()));
                }
            }
        }
    }

    None
}

fn parse_copied_track_line(line: &str, composers: &[String]) -> Option<TrackInfo> {
    parse_copied_track_line_raw(line).map(|(track_number, title, duration)| TrackInfo {
        disc_number: 1,
        track_number,
        title,
        duration,
        composers: composers.to_vec(),
    })
}

fn is_vgmdb_nav_or_chrome_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "vgmdb"
            | "albums"
            | "artists"
            | "labels"
            | "products"
            | "events"
            | "forums"
            | "radio"
            | "login"
            | "register"
            | "search"
            | "print"
            | "edit"
            | "bookmark"
            | "report"
            | "covers"
            | "album"
            | "add to collection"
            | "credits"
            | "tracklist"
            | "notes"
            | "album stats"
            | "related albums"
            | "available at"
            | "websites"
            | "entry stats"
            | "/"
            | "|"
            | "-"
    ) || lower.starts_with("logged in")
        || lower.starts_with("forgot password")
}

pub(crate) fn parse_vgmdb_copied_text(raw: &str) -> Result<AlbumMetadata> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.lines().map(str::trim).collect();

    let mut catalog = String::new();
    let mut release_date = String::new();
    let mut publisher = String::new();
    let mut label_fallback = String::new();
    let mut composers = Vec::new();
    let mut arrangers = Vec::new();
    let mut performers = Vec::new();
    let mut lyricists = Vec::new();

    let mut catalog_line_idx: Option<usize> = None;
    let mut tracklist_line_idx: Option<usize> = None;

    for (idx, &line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let lower_line = line.to_ascii_lowercase();
        if lower_line == "tracklist" || lower_line.starts_with("tracklist\t") {
            if tracklist_line_idx.is_none() {
                tracklist_line_idx = Some(idx);
            }
            continue;
        }

        let kv = line
            .split_once('\t')
            .or_else(|| line.split_once(": "));
        if let Some((raw_k, raw_v)) = kv {
            let k = raw_k.trim().to_ascii_lowercase();
            let v = raw_v.trim();
            if v.is_empty() {
                continue;
            }
            if k == "catalog number" {
                if catalog_line_idx.is_none() {
                    catalog_line_idx = Some(idx);
                }
                if catalog.is_empty() {
                    let main_cat = v
                        .split('\t')
                        .next()
                        .unwrap_or(v)
                        .split('(')
                        .next()
                        .unwrap_or(v)
                        .trim();
                    if !main_cat.is_empty() && !main_cat.eq_ignore_ascii_case("n/a") {
                        catalog = main_cat.to_string();
                    }
                }
            } else if k == "release date" {
                if catalog_line_idx.is_none() {
                    catalog_line_idx = Some(idx);
                }
                if release_date.is_empty() {
                    release_date = v.to_string();
                }
            } else if k == "barcode" && catalog_line_idx.is_none() {
                catalog_line_idx = Some(idx);
            } else if (k == "publisher" || k == "publish organization" || k == "published by")
                && publisher.is_empty()
            {
                publisher = clean_vgmdb_publisher(v);
            } else if (k == "label" || k == "organizations") && label_fallback.is_empty() {
                label_fallback = clean_vgmdb_publisher(v);
            } else if matches!(
                k.as_str(),
                "composer"
                    | "composed by"
                    | "all composed by"
                    | "all music composed by"
                    | "music"
            ) {
                for person in split_people_list(v) {
                    push_unique(&mut composers, person);
                }
            } else if matches!(
                k.as_str(),
                "arranger" | "arranged by" | "all arranged by" | "all music arranged by"
            ) {
                for person in split_people_list(v) {
                    push_unique(&mut arrangers, person);
                }
            } else if matches!(k.as_str(), "performer" | "performed by" | "vocals") {
                for person in split_people_list(v) {
                    push_unique(&mut performers, person);
                }
            } else if matches!(k.as_str(), "lyricist" | "lyrics by") {
                for person in split_people_list(v) {
                    push_unique(&mut lyricists, person);
                }
            }
        }
    }

    if publisher.is_empty() {
        publisher = label_fallback;
    }

    // Buscar título del álbum hacia atrás desde "Catalog Number" / "Release Date"
    let mut album_title = String::new();
    if let Some(meta_idx) = catalog_line_idx {
        let mut candidates: Vec<&str> = Vec::new();
        for i in (0..meta_idx).rev() {
            let line = lines[i].trim();
            if line.is_empty() {
                continue;
            }
            if is_vgmdb_nav_or_chrome_line(line) {
                if !candidates.is_empty() {
                    break;
                }
                continue;
            }
            if !catalog.is_empty() && line.eq_ignore_ascii_case(&catalog) {
                continue;
            }
            if line.contains('\t') {
                if !candidates.is_empty() {
                    break;
                }
                continue;
            }
            candidates.push(line);
            if candidates.len() >= 3 {
                break;
            }
        }
        // candidates está en orden inverso (de abajo hacia arriba); preferir la primera línea latina de arriba
        candidates.reverse();
        if let Some(latin) = candidates.iter().find(|c| !has_non_ascii(c)) {
            album_title = (*latin).to_string();
        } else if let Some(first) = candidates.first() {
            album_title = (*first).to_string();
        }
    }

    if album_title.is_empty() {
        if let Some(fallback_line) = lines.iter().find(|&&l| {
            !l.is_empty()
                && !is_vgmdb_nav_or_chrome_line(l)
                && !l.contains('\t')
                && parse_disc_header_number(l).is_none()
                && parse_copied_track_line_raw(l).is_none()
        }) {
            album_title = (*fallback_line).to_string();
        }
    }

    // Parsear pistas desde Tracklist (o desde el primer Disc 1 / pista numerada)
    let track_scan_start = tracklist_line_idx.unwrap_or(0);
    let mut tracks = Vec::new();
    let mut current_disc = 1u32;

    for &line in &lines[track_scan_start..] {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if !tracks.is_empty()
            && matches!(
                lower.as_str(),
                "notes"
                    | "album stats"
                    | "related albums"
                    | "available at"
                    | "websites"
                    | "covers"
                    | "entry stats"
                    | "products represented"
                    | "discussion"
                    | "user comments"
            )
        {
            break;
        }
        if lower.starts_with("disc length") || lower.starts_with("total length") {
            continue;
        }
        if matches!(
            lower.as_str(),
            "english" | "romaji" | "japanese" | "tracklist" | "english (official)"
        ) {
            continue;
        }
        if let Some(d) = parse_disc_header_number(line) {
            current_disc = d;
            continue;
        }
        if let Some(mut tr) = parse_copied_track_line(line, &composers) {
            tr.disc_number = current_disc;
            tracks.push(tr);
        }
    }

    if album_title.is_empty() && !catalog.is_empty() {
        album_title = catalog.clone();
    }
    if album_title.is_empty() && tracks.is_empty() {
        bail!("El texto pegado no parece contener una ficha ni lista de pistas válida de VGMdb.");
    }

    Ok(AlbumMetadata {
        source: "VGMdb (Texto)".to_string(),
        album_title,
        catalog,
        release_date,
        publisher,
        composers,
        arrangers,
        performers,
        lyricists,
        tracks,
    })
}

fn strip_discogs_disambiguation(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.ends_with(')') {
        if let Some(open_idx) = trimmed.rfind(" (") {
            let inner = &trimmed[open_idx + 2..trimmed.len() - 1];
            if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                return trimmed[..open_idx].trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

fn prefer_english_discogs_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((left, right)) = trimmed.split_once(" = ") {
        let r = right.trim();
        if has_non_ascii(left) && !r.is_empty() && !has_non_ascii(r) {
            return r.to_string();
        }
    }
    trimmed.to_string()
}

fn parse_discogs_position(pos: &str, current_disc: u32, fallback_track: u32) -> (u32, u32) {
    let trimmed = pos.trim();
    if trimmed.is_empty() {
        return (current_disc, fallback_track);
    }
    if let Some((left, right)) = trimmed.split_once('-').or_else(|| trimmed.split_once('.')) {
        let d_digits: String = left.chars().filter(|c| c.is_ascii_digit()).collect();
        let t_digits: String = right.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let (Ok(d), Ok(t)) = (d_digits.parse::<u32>(), t_digits.parse::<u32>()) {
            if d > 0 && t > 0 {
                return (d, t);
            }
        }
    }
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    let track_num = digits
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(fallback_track);
    (current_disc, track_num)
}

fn collect_discogs_extraartists(
    arr: Option<&Value>,
    composers: &mut Vec<String>,
    arrangers: &mut Vec<String>,
    performers: &mut Vec<String>,
    lyricists: &mut Vec<String>,
    track_composers_only: Option<&mut Vec<String>>,
) {
    let Some(items) = arr.and_then(Value::as_array) else {
        return;
    };
    let mut track_comp = track_composers_only;
    for ea in items {
        let Some(raw_name) = ea.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = strip_discogs_disambiguation(raw_name);
        if name.is_empty() {
            continue;
        }
        let role = ea
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();

        let is_composer = role.contains("composed")
            || role.contains("composer")
            || role.contains("music by")
            || role.contains("written-by")
            || role.contains("written by")
            || role.contains("songwriter")
            || role == "music";
        let is_arranger = role.contains("arranged") || role.contains("orchestrated");
        let is_lyricist = role.contains("lyrics") || role.contains("lyricist");
        let is_performer = role.contains("vocal")
            || role.contains("perform")
            || role.contains("piano")
            || role.contains("guitar")
            || role.contains("violin")
            || role.contains("drums")
            || role.contains("bass")
            || role.contains("synth")
            || role.contains("chorus")
            || role.contains("featuring");

        if is_composer {
            push_unique(composers, name.clone());
            if let Some(ref mut tc) = track_comp {
                push_unique(tc, name.clone());
            }
        }
        if is_arranger {
            push_unique(arrangers, name.clone());
        }
        if is_lyricist {
            push_unique(lyricists, name.clone());
        }
        if is_performer {
            push_unique(performers, name.clone());
        }
    }
}

pub(crate) fn parse_discogs_release_json(
    detail: &Value,
    requested_catalog: Option<&str>,
    preserve_range: bool,
) -> Result<AlbumMetadata> {
    let raw_title = detail
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("La respuesta de Discogs no contiene título de lanzamiento")?;
    let album_title = prefer_english_discogs_title(raw_title);

    let release_date = detail
        .get("released")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            detail
                .get("year")
                .and_then(Value::as_u64)
                .filter(|y| *y > 0)
                .map(|y| y.to_string())
        })
        .unwrap_or_default();

    let mut publisher = String::new();
    let mut label_catnos = Vec::new();
    if let Some(labels) = detail.get("labels").and_then(Value::as_array) {
        if let Some(first) = labels.first() {
            if let Some(name) = first.get("name").and_then(Value::as_str) {
                publisher = strip_discogs_disambiguation(name);
            }
        }
        for lbl in labels {
            if let Some(c) = lbl.get("catno").and_then(Value::as_str) {
                let c_trim = c.trim();
                if !c_trim.is_empty()
                    && !c_trim.eq_ignore_ascii_case("none")
                    && !label_catnos.iter().any(|x: &String| x == c_trim)
                {
                    label_catnos.push(c_trim.to_string());
                }
            }
        }
    }

    let catalog = if preserve_range {
        requested_catalog
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| label_catnos.join(" / "))
    } else if !label_catnos.is_empty() {
        label_catnos.join(" / ")
    } else {
        requested_catalog.unwrap_or("").trim().to_string()
    };

    let mut composers = Vec::new();
    let mut arrangers = Vec::new();
    let mut performers = Vec::new();
    let mut lyricists = Vec::new();

    collect_discogs_extraartists(
        detail.get("extraartists"),
        &mut composers,
        &mut arrangers,
        &mut performers,
        &mut lyricists,
        None,
    );

    if composers.is_empty() {
        if let Some(artists) = detail.get("artists").and_then(Value::as_array) {
            for art in artists {
                if let Some(raw_name) = art.get("name").and_then(Value::as_str) {
                    let clean = strip_discogs_disambiguation(raw_name);
                    if !clean.is_empty()
                        && !clean.eq_ignore_ascii_case("various")
                        && !clean.eq_ignore_ascii_case("various artists")
                    {
                        push_unique(&mut composers, clean);
                    }
                }
            }
        }
    }

    let mut tracks = Vec::new();
    let mut current_disc = 1u32;
    let mut track_counter = 1u32;

    if let Some(tracklist) = detail.get("tracklist").and_then(Value::as_array) {
        for tr in tracklist {
            let tr_type = tr
                .get("type_")
                .and_then(Value::as_str)
                .unwrap_or("track")
                .trim();

            if tr_type.eq_ignore_ascii_case("heading") {
                if let Some(heading_title) = tr.get("title").and_then(Value::as_str) {
                    if let Some(d) = parse_disc_header_number(heading_title) {
                        current_disc = d;
                        track_counter = 1;
                    }
                }
                continue;
            }

            if tr_type.eq_ignore_ascii_case("index") {
                continue;
            }

            let raw_pos = tr.get("position").and_then(Value::as_str).unwrap_or("");
            let (disc_number, track_number) =
                parse_discogs_position(raw_pos, current_disc, track_counter);
            if disc_number != current_disc {
                current_disc = disc_number;
            }

            let title = tr
                .get("title")
                .and_then(Value::as_str)
                .map(prefer_english_discogs_title)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("Pista {}", track_number));

            let duration = tr
                .get("duration")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();

            let mut track_composers = Vec::new();
            collect_discogs_extraartists(
                tr.get("extraartists"),
                &mut composers,
                &mut arrangers,
                &mut performers,
                &mut lyricists,
                Some(&mut track_composers),
            );

            tracks.push(TrackInfo {
                disc_number,
                track_number,
                title,
                duration,
                composers: track_composers,
            });
            track_counter = track_number + 1;
        }
    }

    for tr in &mut tracks {
        if tr.composers.is_empty() {
            tr.composers = composers.clone();
        }
    }

    Ok(AlbumMetadata {
        source: "Discogs".to_string(),
        album_title,
        catalog,
        release_date,
        publisher,
        composers,
        arrangers,
        performers,
        lyricists,
        tracks,
    })
}

fn lookup_via_discogs_release_id(release_id: &str) -> Result<AlbumMetadata> {
    let url = format!("{}/releases/{}", DISCOGS_BASE_URL, release_id.trim());
    let detail = curl_get_json_retry(&url, 12, 3)?;
    parse_discogs_release_json(&detail, None, false)
}

fn lookup_via_discogs(catalog_query: &str) -> Result<AlbumMetadata> {
    let trimmed = catalog_query.trim();
    let variants = expand_catalog_variants(trimmed);

    let find_matching_release = |results: &[Value]| -> Option<u64> {
        results.iter().find_map(|item| {
            let catno = item.get("catno").and_then(Value::as_str)?;
            if variants.matches(catno) {
                item.get("id").and_then(Value::as_u64)
            } else {
                None
            }
        })
    };

    let search_url = format!(
        "{}/database/search?catno={}&type=release",
        DISCOGS_BASE_URL,
        percent_encode(trimmed)
    );
    let search_json = curl_get_json_retry(&search_url, 12, 3)?;
    let mut chosen_id = search_json
        .get("results")
        .and_then(Value::as_array)
        .and_then(|arr| find_matching_release(arr));

    if chosen_id.is_none() && variants.is_range {
        std::thread::sleep(std::time::Duration::from_millis(600));
        let base_url = format!(
            "{}/database/search?catno={}&type=release",
            DISCOGS_BASE_URL,
            percent_encode(&variants.base_catalog)
        );
        if let Ok(base_json) = curl_get_json_retry(&base_url, 12, 2) {
            chosen_id = base_json
                .get("results")
                .and_then(Value::as_array)
                .and_then(|arr| find_matching_release(arr));
        }
    }

    let release_id = chosen_id.with_context(|| {
        format!(
            "No se encontró el catálogo '{}' en Discogs",
            trimmed
        )
    })?;

    std::thread::sleep(std::time::Duration::from_millis(500));
    let detail_url = format!("{}/releases/{}", DISCOGS_BASE_URL, release_id);
    let detail = curl_get_json_retry(&detail_url, 12, 3)?;
    parse_discogs_release_json(&detail, Some(trimmed), variants.is_range)
}

fn lookup_via_vgmdb_api(query: &str, base_url: &str) -> Result<AlbumMetadata> {
    let base = base_url.trim_end_matches('/');
    let trimmed = query.trim();

    if let Some(id) = extract_vgmdb_album_id(trimmed) {
        let url = format!("{}/album/{}?format=json", base, id);
        let data = curl_get_json(&url, 8)?;
        return parse_vgmdb_album_json(&data, trimmed);
    }

    let search_url = format!("{}/search/albums/{}?format=json", base, percent_encode(trimmed));
    let search_json = curl_get_json(&search_url, 8)?;
    let albums = search_json
        .get("results")
        .and_then(|r| r.get("albums"))
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
        .with_context(|| format!("VGMdb no devolvió resultados para '{}'", trimmed))?;

    let target_variants = expand_catalog_variants(trimmed);
    let chosen = albums
        .iter()
        .find(|item| {
            item.get("catalog")
                .and_then(Value::as_str)
                .is_some_and(|c| target_variants.matches(c))
        });

    let chosen = match chosen {
        Some(alb) => alb,
        None => {
            bail!(
                "VGMdb devolvió {} resultado(s) pero ninguno coincide con el catálogo '{}'.",
                albums.len(),
                trimmed
            );
        }
    };

    let link = chosen
        .get("link")
        .and_then(Value::as_str)
        .map(|s| s.trim_start_matches('/'))
        .context("Resultado de búsqueda en VGMdb sin campo 'link'")?;

    let album_url = format!("{}/{}?format=json", base, link);
    let album_json = curl_get_json(&album_url, 8)?;
    parse_vgmdb_album_json(&album_json, trimmed)
}

fn has_non_ascii(s: &str) -> bool {
    s.chars().any(|c| !c.is_ascii())
}

fn flip_sort_name(sort_name: &str) -> String {
    if let Some((last, first)) = sort_name.split_once(',') {
        let first = first.trim();
        let last = last.trim();
        if !first.is_empty() && !last.is_empty() {
            return format!("{} {}", first, last);
        }
    }
    sort_name.trim().to_string()
}

fn format_mb_artist_name(artist_obj: &Value, credit_name: Option<&str>) -> Option<String> {
    let raw_name = artist_obj
        .get("name")
        .and_then(Value::as_str)
        .or(credit_name)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let sort_name = artist_obj
        .get("sort-name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if has_non_ascii(raw_name) {
        if let Some(sn) = sort_name {
            let romaji = flip_sort_name(sn);
            if !romaji.is_empty() {
                return Some(romaji);
            }
        }
    }

    Some(raw_name.to_string())
}

fn push_unique(vec: &mut Vec<String>, name: String) {
    if !name.is_empty() && !vec.iter().any(|existing| existing == &name) {
        vec.push(name);
    }
}

fn collect_mb_relations(
    relations: Option<&Value>,
    composers: &mut Vec<String>,
    arrangers: &mut Vec<String>,
    performers: &mut Vec<String>,
    lyricists: &mut Vec<String>,
) {
    let Some(arr) = relations.and_then(Value::as_array) else {
        return;
    };

    for rel in arr {
        let rel_type = rel
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();

        if let Some(artist) = rel.get("artist") {
            if let Some(name) = format_mb_artist_name(artist, None) {
                if rel_type.contains("composer") || rel_type == "writer" {
                    push_unique(composers, name);
                } else if rel_type.contains("arranger") || rel_type.contains("orchestrator") {
                    push_unique(arrangers, name);
                } else if rel_type.contains("lyricist") {
                    push_unique(lyricists, name);
                } else if rel_type.contains("performer")
                    || rel_type.contains("instrument")
                    || rel_type.contains("vocal")
                {
                    push_unique(performers, name);
                }
            }
        }

        if let Some(work_rels) = rel.get("work").and_then(|w| w.get("relations")) {
            collect_mb_relations(
                Some(work_rels),
                composers,
                arrangers,
                performers,
                lyricists,
            );
        }
    }
}

fn format_ms_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{}:{:02}", mins, secs)
}

fn build_musicbrainz_catalog_query(variants: &CatalogVariants) -> String {
    if variants.is_range {
        format!(
            "catno:\"{}\" OR catno:\"{}\"",
            variants.raw, variants.base_catalog
        )
    } else {
        format!("catno:\"{}\"", variants.raw)
    }
}

fn lookup_via_musicbrainz(catalog_query: &str) -> Result<AlbumMetadata> {
    let trimmed = catalog_query.trim();
    let variants = expand_catalog_variants(trimmed);
    let mb_query = build_musicbrainz_catalog_query(&variants);
    let search_url = format!(
        "{}/release/?query={}&fmt=json",
        MUSICBRAINZ_BASE_URL,
        percent_encode(&mb_query)
    );
    let search_json = curl_get_json_retry(&search_url, 12, 4)?;
    let releases = search_json
        .get("releases")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
        .with_context(|| format!("No se encontró el catálogo '{}' en MusicBrainz", trimmed))?;

    let chosen = releases.iter().find(|rel| {
        rel.get("label-info")
            .and_then(Value::as_array)
            .is_some_and(|labels| {
                labels.iter().any(|li| {
                    li.get("catalog-number")
                        .and_then(Value::as_str)
                        .is_some_and(|c| variants.matches(c))
                })
            })
    });

    let chosen = match chosen {
        Some(rel) => rel,
        None => {
            bail!(
                "MusicBrainz devolvió {} resultado(s) pero ninguno coincide con el catálogo '{}'.",
                releases.len(),
                trimmed
            );
        }
    };

    let release_id = chosen
        .get("id")
        .and_then(Value::as_str)
        .context("Resultado de MusicBrainz sin ID de lanzamiento")?;

    // Respetar el límite de 1 petición/segundo de MusicBrainz
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let detail_url = format!(
        "{}/release/{}?inc=recordings+artist-credits+artist-rels+work-rels+work-level-rels+labels+release-groups&fmt=json",
        MUSICBRAINZ_BASE_URL, release_id
    );
    let detail = curl_get_json_retry(&detail_url, 12, 4)?;

    let mut album_title = detail
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(trimmed)
        .trim()
        .to_string();

    let release_date = detail
        .get("date")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let mut catalog = trimmed.to_string();
    let mut publisher = String::new();
    if let Some(labels) = detail.get("label-info").and_then(Value::as_array) {
        if let Some(first_label) = labels.first() {
            if let Some(name) = first_label
                .get("label")
                .and_then(|l| l.get("name"))
                .and_then(Value::as_str)
            {
                publisher = name.trim().to_string();
            }
            let cats: Vec<&str> = labels
                .iter()
                .filter_map(|li| li.get("catalog-number").and_then(Value::as_str))
                .collect();
            if !cats.is_empty() {
                if variants.is_range {
                    catalog = trimmed.to_string();
                } else {
                    catalog = cats.join(" / ");
                }
            }
        }
    }

    let mut composers = Vec::new();
    let mut arrangers = Vec::new();
    let mut performers = Vec::new();
    let mut lyricists = Vec::new();

    collect_mb_relations(
        detail.get("relations"),
        &mut composers,
        &mut arrangers,
        &mut performers,
        &mut lyricists,
    );

    let mut tracks = Vec::new();
    if let Some(media) = detail.get("media").and_then(Value::as_array) {
        for (m_idx, medium) in media.iter().enumerate() {
            let disc_number = medium
                .get("position")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or((m_idx + 1) as u32);

            if let Some(track_arr) = medium.get("tracks").and_then(Value::as_array) {
                for (t_idx, tr) in track_arr.iter().enumerate() {
                    let track_number = tr
                        .get("position")
                        .and_then(Value::as_u64)
                        .map(|n| n as u32)
                        .unwrap_or((t_idx + 1) as u32);

                    let title = tr
                        .get("title")
                        .or_else(|| tr.get("recording").and_then(|r| r.get("title")))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();

                    let duration = tr
                        .get("length")
                        .or_else(|| tr.get("recording").and_then(|r| r.get("length")))
                        .and_then(Value::as_u64)
                        .map(format_ms_duration)
                        .unwrap_or_default();

                    let mut track_composers = Vec::new();
                    let mut track_arrangers = Vec::new();
                    let mut track_performers = Vec::new();
                    let mut track_lyricists = Vec::new();

                    if let Some(rec) = tr.get("recording") {
                        collect_mb_relations(
                            rec.get("relations"),
                            &mut track_composers,
                            &mut track_arrangers,
                            &mut track_performers,
                            &mut track_lyricists,
                        );
                    }
                    for c in &track_composers {
                        push_unique(&mut composers, c.clone());
                    }
                    for a in &track_arrangers {
                        push_unique(&mut arrangers, a.clone());
                    }
                    for p in &track_performers {
                        push_unique(&mut performers, p.clone());
                    }
                    for l in &track_lyricists {
                        push_unique(&mut lyricists, l.clone());
                    }

                    tracks.push(TrackInfo {
                        disc_number,
                        track_number,
                        title: if title.is_empty() {
                            format!("Pista {}", track_number)
                        } else {
                            title
                        },
                        duration,
                        composers: track_composers,
                    });
                }
            }
        }
    }

    // Muchos soundtracks en MusicBrainz tienen al compositor principal en artist-credit del álbum
    if composers.is_empty() {
        if let Some(credits) = detail.get("artist-credit").and_then(Value::as_array) {
            for credit in credits {
                let credit_name = credit.get("name").and_then(Value::as_str);
                if let Some(artist) = credit.get("artist") {
                    if let Some(formatted) = format_mb_artist_name(artist, credit_name) {
                        push_unique(&mut composers, formatted);
                    }
                } else if let Some(cn) = credit_name {
                    push_unique(&mut composers, cn.trim().to_string());
                }
            }
        }
    }

    for tr in &mut tracks {
        if tr.composers.is_empty() {
            tr.composers = composers.clone();
        }
    }

    // Si el lanzamiento encontrado por catálogo está en alfabeto no latino (ej. Jpan),
    // buscar una edición oficial/pseudo-release en inglés (Latn) dentro del mismo release-group.
    let script = detail
        .get("text-representation")
        .and_then(|t| t.get("script"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let rg_id = detail
        .get("release-group")
        .and_then(|rg| rg.get("id"))
        .and_then(Value::as_str)
        .or_else(|| {
            chosen
                .get("release-group")
                .and_then(|rg| rg.get("id"))
                .and_then(Value::as_str)
        });

    if !script.eq_ignore_ascii_case("Latn") {
        if let Some(rg_id) = rg_id {
            std::thread::sleep(std::time::Duration::from_millis(1100));
            let rg_url = format!(
                "{}/release?release-group={}&inc=recordings&limit=100&fmt=json",
                MUSICBRAINZ_BASE_URL, rg_id
            );
            if let Ok(rg_json) = curl_get_json_retry(&rg_url, 12, 2) {
                if let Some(rg_releases) = rg_json.get("releases").and_then(Value::as_array) {
                    let expected_tracks = tracks.len();
                    let best_latn = rg_releases
                        .iter()
                        .filter_map(|rel| {
                            let rep = rel.get("text-representation")?;
                            let rel_script = rep.get("script").and_then(Value::as_str)?;
                            if !rel_script.eq_ignore_ascii_case("Latn") {
                                return None;
                            }
                            let media = rel.get("media").and_then(Value::as_array)?;
                            let titles: Vec<String> = media
                                .iter()
                                .filter_map(|m| m.get("tracks").and_then(Value::as_array))
                                .flatten()
                                .filter_map(|t| {
                                    t.get("title")
                                        .or_else(|| t.get("recording").and_then(|r| r.get("title")))
                                        .and_then(Value::as_str)
                                        .map(|s| s.trim().to_string())
                                })
                                .collect();
                            if titles.len() != expected_tracks {
                                return None;
                            }
                            let lang = rep.get("language").and_then(Value::as_str).unwrap_or("");
                            let status = rel.get("status").and_then(Value::as_str).unwrap_or("");
                            let score = (if lang.eq_ignore_ascii_case("eng") { 10 } else { 0 })
                                + (if status.eq_ignore_ascii_case("Official") {
                                    4
                                } else if status.eq_ignore_ascii_case("Pseudo-Release") {
                                    3
                                } else {
                                    1
                                });
                            Some((score, rel, titles))
                        })
                        .max_by_key(|(score, _, _)| *score);

                    if let Some((_, rel, eng_titles)) = best_latn {
                        if let Some(eng_album) = rel
                            .get("title")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            album_title = eng_album.to_string();
                        }
                        for (tr, eng_title) in tracks.iter_mut().zip(eng_titles.into_iter()) {
                            if !eng_title.is_empty() {
                                tr.title = eng_title;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(AlbumMetadata {
        source: "MusicBrainz".to_string(),
        album_title,
        catalog,
        release_date,
        publisher,
        composers,
        arrangers,
        performers,
        lyricists,
        tracks,
    })
}

pub(crate) fn lookup_album(query: &str, vgmdb_base_url: Option<&str>) -> Result<AlbumMetadata> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        bail!("Ingresá un código de catálogo (ej: SQEX-10001), URL/ID de VGMdb o URL de Discogs.");
    }

    // 1. Archivo local (.json, .html/.htm, .txt)
    let as_path = Path::new(trimmed);
    if as_path.is_file() {
        if let Some(ext) = as_path.extension().and_then(OsStr::to_str) {
            if ext.eq_ignore_ascii_case("json") {
                let raw = fs::read_to_string(as_path)
                    .with_context(|| format!("No se pudo leer el JSON {}", as_path.display()))?;
                let val: Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
                    .with_context(|| format!("El archivo {} no es un JSON válido", as_path.display()))?;
                return parse_vgmdb_album_json(&val, "");
            } else if ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm") {
                let raw = fs::read_to_string(as_path)
                    .with_context(|| format!("No se pudo leer el archivo HTML {}", as_path.display()))?;
                return parse_vgmdb_html(&raw, "");
            } else if ext.eq_ignore_ascii_case("txt") {
                let raw = fs::read_to_string(as_path)
                    .with_context(|| format!("No se pudo leer el archivo de texto {}", as_path.display()))?;
                return parse_vgmdb_copied_text(&raw);
            }
        }
    }

    // 2. Contenido pegado directamente (HTML, JSON o texto copiado de VGMdb)
    if looks_like_vgmdb_html(trimmed) {
        return parse_vgmdb_html(trimmed, "");
    }
    if trimmed.starts_with('{') {
        let val: Value = serde_json::from_str(trimmed.trim_start_matches('\u{feff}'))
            .context("El texto JSON pegado no es válido")?;
        return parse_vgmdb_album_json(&val, "");
    }
    if looks_like_vgmdb_copied_text(trimmed) {
        return parse_vgmdb_copied_text(trimmed);
    }

    // 3. URL directa de Discogs (ej: https://www.discogs.com/release/1830265-...)
    if let Some(discogs_id) = extract_discogs_release_id(trimmed) {
        return lookup_via_discogs_release_id(&discogs_id);
    }

    // 4. Mirror explícito de API VGMdb (solo si el usuario pasó --vgmdb-url)
    if let Some(custom_base) = vgmdb_base_url.map(str::trim).filter(|s| !s.is_empty()) {
        match lookup_via_vgmdb_api(trimmed, custom_base) {
            Ok(album) => return Ok(album),
            Err(err) if extract_vgmdb_album_id(trimmed).is_some() => {
                bail!("No se pudo consultar el servidor VGMdb '{}': {:#}", custom_base, err);
            }
            Err(_) => {}
        }
    }

    // 5. URL o ID directo de VGMdb sin servidor mirror activo
    if let Some(id) = extract_vgmdb_album_id(trimmed) {
        bail!(
            "VGMdb ID {} detected (https://vgmdb.net/album/{}). \
             La API externa vgmdb.info está caída de forma permanente y vgmdb.net requiere un navegador real (Cloudflare).\n\
             Abrí https://vgmdb.net/album/{} en tu navegador, presioná Ctrl+A y Ctrl+C, y luego tocá '📋 Pegar VGMdb' en la app.",
            id,
            id,
            id
        );
    }

    // 6. Búsqueda por código de catálogo en APIs abiertas y activas: MusicBrainz -> Discogs
    match lookup_via_musicbrainz(trimmed) {
        Ok(album) => Ok(album),
        Err(mb_err) => match lookup_via_discogs(trimmed) {
            Ok(album) => Ok(album),
            Err(dg_err) => {
                bail!(
                    "No se encontró '{}' ni en MusicBrainz ({:#}) ni en Discogs ({:#}).",
                    trimmed,
                    mb_err,
                    dg_err
                );
            }
        },
    }
}

fn parse_track_number_tag(raw: &str) -> Option<u32> {
    let head = raw.split('/').next()?.trim();
    head.parse::<u32>().ok()
}

pub(crate) fn read_mp3_tags(mp3_path: &Path, ffmpeg_path: Option<&Path>) -> Mp3Id3Tags {
    let Some(ffprobe) = find_ffprobe(ffmpeg_path) else {
        return Mp3Id3Tags::default();
    };

    let output = new_hidden_command(&ffprobe)
        .arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg(mp3_path)
        .output();

    let Ok(out) = output else {
        return Mp3Id3Tags::default();
    };
    if !out.status.success() {
        return Mp3Id3Tags::default();
    }

    let Ok(json) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Mp3Id3Tags::default();
    };
    let Some(tags) = json
        .get("format")
        .and_then(|f| f.get("tags"))
        .and_then(Value::as_object)
    else {
        return Mp3Id3Tags::default();
    };

    let get_tag = |keys: &[&str]| -> Option<String> {
        for key in keys {
            for (k, v) in tags {
                if k.eq_ignore_ascii_case(key) {
                    if let Some(s) = v.as_str() {
                        let t = s.trim();
                        if !t.is_empty() {
                            return Some(t.to_string());
                        }
                    }
                }
            }
        }
        None
    };

    Mp3Id3Tags {
        title: get_tag(&["title", "TIT2"]),
        track: get_tag(&["track", "TRCK"]).and_then(|s| parse_track_number_tag(&s)),
        disc: get_tag(&["disc", "TPOS"]).and_then(|s| parse_track_number_tag(&s)),
        artist: get_tag(&["artist", "TPE1"]),
        composer: get_tag(&["composer", "TCOM"]),
        album: get_tag(&["album", "TALB"]),
        date: get_tag(&["date", "year", "TDRC", "TYER"]),
    }
}

fn extract_leading_numbers(stem: &str) -> (Option<u32>, Option<u32>) {
    // Soporta formatos comunes: "01 - Tema", "1-02 Tema", "102 Tema", "2.03 Tema"
    let bytes = stem.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == 0 {
        return (None, None);
    }

    let first_num = stem[..idx].parse::<u32>().ok();
    if idx + 1 < bytes.len() && (bytes[idx] == b'-' || bytes[idx] == b'.') && bytes[idx + 1].is_ascii_digit() {
        let mut end2 = idx + 1;
        while end2 < bytes.len() && bytes[end2].is_ascii_digit() {
            end2 += 1;
        }
        if let Some(track) = stem[idx + 1..end2].parse::<u32>().ok() {
            return (first_num, Some(track));
        }
    }

    if idx == 3 {
        if let Some(n) = first_num {
            let disc = n / 100;
            let track = n % 100;
            if disc >= 1 && track >= 1 {
                return (Some(disc), Some(track));
            }
        }
    }

    (None, first_num)
}

pub(crate) struct ResolvedTrackMetadata {
    pub album_title: String,
    pub catalog: String,
    pub disc_number: u32,
    pub track_number: u32,
    pub track_title: String,
    pub composers: String,
    pub arrangers: String,
    pub performers: String,
    pub publisher: String,
    pub release_date: String,
}

pub(crate) fn match_track_for_file(
    file_path: &Path,
    index: usize,
    album: Option<&AlbumMetadata>,
    id3: &Mp3Id3Tags,
) -> ResolvedTrackMetadata {
    let stem = file_path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("Track"))
        .to_string_lossy()
        .to_string();

    let (file_disc, file_track) = extract_leading_numbers(&stem);
    let target_disc = id3.disc.or(file_disc);
    let target_track = id3.track.or(file_track);

    let matched = album.and_then(|alb| {
        if alb.tracks.is_empty() {
            return None;
        }
        if let (Some(d), Some(t)) = (target_disc, target_track) {
            if let Some(found) = alb
                .tracks
                .iter()
                .find(|tr| tr.disc_number == d && tr.track_number == t)
            {
                return Some(found);
            }
        }
        if let Some(t) = target_track {
            if let Some(found) = alb.tracks.iter().find(|tr| tr.track_number == t) {
                return Some(found);
            }
        }
        alb.tracks.get(index)
    });

    let album_title = album
        .map(|a| a.album_title.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| id3.album.clone())
        .unwrap_or_default();

    let catalog = album.map(|a| a.catalog.clone()).unwrap_or_default();
    let publisher = album.map(|a| a.publisher.clone()).unwrap_or_default();
    let release_date = album
        .map(|a| a.release_date.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| id3.date.clone())
        .unwrap_or_default();

    let disc_number = matched
        .map(|m| m.disc_number)
        .or(target_disc)
        .unwrap_or(1);
    let track_number = matched
        .map(|m| m.track_number)
        .or(target_track)
        .unwrap_or((index + 1) as u32);

    let track_title = matched
        .map(|m| m.title.clone())
        .or_else(|| id3.title.clone())
        .unwrap_or(stem);

    let composers = matched
        .map(|m| m.composers.join(", "))
        .filter(|s| !s.is_empty())
        .or_else(|| album.map(|a| a.composers.join(", ")).filter(|s| !s.is_empty()))
        .or_else(|| id3.composer.clone())
        .or_else(|| id3.artist.clone())
        .unwrap_or_default();

    let arrangers = album.map(|a| a.arrangers.join(", ")).unwrap_or_default();
    let performers = album.map(|a| a.performers.join(", ")).unwrap_or_default();

    ResolvedTrackMetadata {
        album_title,
        catalog,
        disc_number,
        track_number,
        track_title,
        composers,
        arrangers,
        performers,
        publisher,
        release_date,
    }
}

pub(crate) fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}

pub(crate) fn apply_template(template: &str, meta: &ResolvedTrackMetadata) -> String {
    let track_num = format!("{:02}", meta.track_number);
    let disc_track = format!("{}-{:02}", meta.disc_number, meta.track_number);

    let replaced = template
        .replace("{album}", &meta.album_title)
        .replace("{catalog}", &meta.catalog)
        .replace("{disc}", &meta.disc_number.to_string())
        .replace("{track_num}", &track_num)
        .replace("{disc_track}", &disc_track)
        .replace("{track_title}", &meta.track_title)
        .replace("{composers}", &meta.composers)
        .replace("{arrangers}", &meta.arrangers)
        .replace("{performers}", &meta.performers)
        .replace("{publisher}", &meta.publisher)
        .replace("{release_date}", &meta.release_date);

    // Limpiar líneas de metadatos vacíos tipo "Arreglador(es): " en descripciones
    let cleaned_lines: Vec<String> = replaced
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if let Some((_, rhs)) = trimmed.split_once(':') {
                !rhs.trim().is_empty()
            } else {
                true
            }
        })
        .map(str::to_string)
        .collect();

    cleaned_lines.join("\n").trim().to_string()
}

pub(crate) fn format_title(template: &str, meta: &ResolvedTrackMetadata) -> String {
    let raw = apply_template(template, meta)
        .replace('\n', " ")
        .trim()
        .to_string();
    let cleaned = raw
        .trim_start_matches(['-', '.', ' '])
        .trim()
        .to_string();
    let fallback = if cleaned.is_empty() {
        meta.track_title.clone()
    } else {
        cleaned
    };
    truncate_chars(&fallback, 100)
}

pub(crate) fn format_description(template: &str, meta: &ResolvedTrackMetadata) -> String {
    let text = apply_template(template, meta);
    truncate_chars(&text, 4900)
}

pub(crate) fn build_tags(meta: &ResolvedTrackMetadata) -> Vec<String> {
    let mut tags = Vec::new();
    let mut push_tag = |t: &str| {
        let trimmed = t.trim();
        if !trimmed.is_empty() && !tags.iter().any(|x: &String| x.eq_ignore_ascii_case(trimmed)) {
            tags.push(trimmed.to_string());
        }
    };

    push_tag(&meta.album_title);
    push_tag(&meta.catalog);
    for comp in meta.composers.split(',') {
        push_tag(comp);
    }
    push_tag("Soundtrack");
    push_tag("OST");
    push_tag("VGM");
    tags.truncate(15);
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_encode() {
        assert_eq!(percent_encode("SQEX-10001"), "SQEX-10001");
        assert_eq!(percent_encode("KECH-1346~7"), "KECH-1346%7E7");
        assert_eq!(percent_encode("catno:\"KECH-1346~7\""), "catno%3A%22KECH-1346%7E7%22");
    }

    #[test]
    fn test_expand_catalog_variants_tilde_single_digit() {
        let v = expand_catalog_variants("KECH-1346~7");
        assert!(v.is_range);
        assert_eq!(v.base_catalog, "KECH-1346");
        assert_eq!(v.individual_discs, vec!["KECH-1346", "KECH-1347"]);
        assert!(v.matches("KECH-1346"));
        assert!(v.matches("KECH-1347"));
        assert!(v.matches("KECH-1346~7"));
        assert!(v.matches("KECH-1346/7"));
        // No debe coincidir con un catálogo diferente como KECH-1436
        assert!(!v.matches("KECH-1436"));
        assert!(!v.matches("SQEX-10001"));
    }

    #[test]
    fn test_expand_catalog_variants_fullwidth_tilde() {
        let v = expand_catalog_variants("KECH-1346～7");
        assert!(v.is_range);
        assert_eq!(v.base_catalog, "KECH-1346");
        assert_eq!(v.individual_discs, vec!["KECH-1346", "KECH-1347"]);
        assert!(v.matches("KECH-1346"));
        assert!(v.matches("KECH-1347"));
    }

    #[test]
    fn test_expand_catalog_variants_multi_disc_box() {
        let v = expand_catalog_variants("SQEX-10001~4");
        assert!(v.is_range);
        assert_eq!(v.base_catalog, "SQEX-10001");
        assert_eq!(
            v.individual_discs,
            vec!["SQEX-10001", "SQEX-10002", "SQEX-10003", "SQEX-10004"]
        );
        assert!(v.matches("SQEX-10001"));
        assert!(v.matches("SQEX-10002"));
        assert!(v.matches("SQEX-10003"));
        assert!(v.matches("SQEX-10004"));
        assert!(v.matches("SQEX-10001~4"));
        assert!(!v.matches("SQEX-10005"));
    }

    #[test]
    fn test_expand_catalog_variants_two_digit_suffix() {
        let v = expand_catalog_variants("CPCA-10128~30");
        assert!(v.is_range);
        assert_eq!(v.base_catalog, "CPCA-10128");
        assert_eq!(
            v.individual_discs,
            vec!["CPCA-10128", "CPCA-10129", "CPCA-10130"]
        );
        assert!(v.matches("CPCA-10128"));
        assert!(v.matches("CPCA-10130"));
        assert!(!v.matches("CPCA-10131"));
    }

    #[test]
    fn test_expand_catalog_variants_full_numbers() {
        let v = expand_catalog_variants("SVWC-7001~7002");
        assert!(v.is_range);
        assert_eq!(v.base_catalog, "SVWC-7001");
        assert_eq!(v.individual_discs, vec!["SVWC-7001", "SVWC-7002"]);
        assert!(v.matches("SVWC-7001"));
        assert!(v.matches("SVWC-7002"));
    }

    #[test]
    fn test_expand_catalog_variants_slash() {
        let v = expand_catalog_variants("ABC-123/4");
        assert!(v.is_range);
        assert_eq!(v.base_catalog, "ABC-123");
        assert_eq!(v.individual_discs, vec!["ABC-123", "ABC-124"]);
        assert!(v.matches("ABC-123"));
        assert!(v.matches("ABC-124"));
    }

    #[test]
    fn test_single_catalog_matching() {
        let v = expand_catalog_variants("SQEX-10001");
        assert!(!v.is_range);
        assert_eq!(v.base_catalog, "SQEX-10001");
        assert!(v.matches("SQEX-10001"));
        assert!(v.matches("sqex 10001"));
        // Coincide también si el disco de la base de datos tiene la notación multi-disco completa
        assert!(v.matches("SQEX-10001~4"));
        assert!(!v.matches("SQEX-10002"));
    }

    #[test]
    fn test_build_mb_query() {
        let v_single = expand_catalog_variants("SQEX-10001");
        assert_eq!(build_musicbrainz_catalog_query(&v_single), "catno:\"SQEX-10001\"");

        let v_range = expand_catalog_variants("KECH-1346~7");
        assert_eq!(
            build_musicbrainz_catalog_query(&v_range),
            "catno:\"KECH-1346~7\" OR catno:\"KECH-1346\""
        );
    }

    #[test]
    fn test_extract_vgmdb_album_id() {
        assert_eq!(
            extract_vgmdb_album_id("https://vgmdb.net/album/10200"),
            Some("10200".to_string())
        );
        assert_eq!(
            extract_vgmdb_album_id("http://vgmdb.net/album/10200/"),
            Some("10200".to_string())
        );
        assert_eq!(
            extract_vgmdb_album_id("https://www.vgmdb.net/album/10200?format=json"),
            Some("10200".to_string())
        );
        assert_eq!(
            extract_vgmdb_album_id("vgmdb.net/album/10200"),
            Some("10200".to_string())
        );
        assert_eq!(
            extract_vgmdb_album_id("album/10200"),
            Some("10200".to_string())
        );
        assert_eq!(
            extract_vgmdb_album_id("10200"),
            Some("10200".to_string())
        );
        assert_eq!(extract_vgmdb_album_id("KECH-1346~7"), None);
        assert_eq!(extract_vgmdb_album_id("SQEX-10001"), None);
    }

    #[test]
    fn test_parse_vgmdb_html() {
        let sample_html = r#"
        <!DOCTYPE html>
        <html>
        <head>
            <title>KECH-1346~7 | Zill O'll ~infinite~ Original Sound Track &amp; Drama - VGMdb</title>
        </head>
        <body>
            <span class="albumtitle" lang="en">Zill O'll ~infinite~ Original Sound Track &amp; Drama</span>
            <table>
                <tr><td>Catalog Number</td><td>KECH-1346~7</td></tr>
                <tr><td>Release Date</td><td>Oct 26, 2005</td></tr>
                <tr><td>Publish Organization</td><td>KOEI</td></tr>
                <tr><td>Composed by</td><td>Kunihiro Kawano</td></tr>
            </table>
            <div id="tracklist">
                <span>Disc 1</span>
                <tr><span class="trackname">Prologue</span> <span class="time">1:45</span></tr>
                <tr><span class="trackname">Theme of Zill O'll</span> <span class="time">3:20</span></tr>
                <span>Disc 2</span>
                <tr><span class="trackname">Drama Chapter 1</span> <span class="time">12:30</span></tr>
            </div>
        </body>
        </html>
        "#;

        let meta = parse_vgmdb_html(sample_html, "").expect("Debe parsear HTML de VGMdb");
        assert_eq!(meta.album_title, "Zill O'll ~infinite~ Original Sound Track & Drama");
        assert_eq!(meta.catalog, "KECH-1346~7");
        assert_eq!(meta.release_date, "Oct 26, 2005");
        assert_eq!(meta.publisher, "KOEI");
        assert_eq!(meta.composers, vec!["Kunihiro Kawano"]);
        assert_eq!(meta.tracks.len(), 3);
        assert_eq!(meta.tracks[0].disc_number, 1);
        assert_eq!(meta.tracks[0].track_number, 1);
        assert_eq!(meta.tracks[0].title, "Prologue");
        assert_eq!(meta.tracks[0].duration, "1:45");
        assert_eq!(meta.tracks[1].disc_number, 1);
        assert_eq!(meta.tracks[1].track_number, 2);
        assert_eq!(meta.tracks[1].title, "Theme of Zill O'll");
        assert_eq!(meta.tracks[2].disc_number, 2);
        assert_eq!(meta.tracks[2].track_number, 1);
        assert_eq!(meta.tracks[2].title, "Drama Chapter 1");
    }

    #[test]
    fn test_parse_vgmdb_html_real_structure() {
        let real_like_html = r##"
        <!DOCTYPE html>
        <html>
        <head>
            <title>NECA-30048~9 | ZWEI!! Original Sound Track - VGMdb</title>
            <script type="text/javascript">
                var hidden = '<span class="albumtitle">Fake Title</span>';
            </script>
        </head>
        <body>
            <h1><span class="albumtitle" lang="en" style="display:inline">ZWEI!! Original Sound Track</span><span class="albumtitle" lang="ja" style="display:none"><em> / </em>ツヴァイ!! オリジナル・サウンドトラック</span><span class="albumtitle" lang="ja-Latn" style="display:none"><em> / </em>ZWEI!! Original Sound Track</span></h1>
            <table id="album_infobit_large" cellpadding="1" cellspacing="1">
                <tr><td nowrap="nowrap"><span class="label"><b>Catalog Number</b></span></td>
                <td width="100%"><span id="childbrowse"><a href="#">NECA-30048~9</a><script type="text/javascript">vBmenu.register('childbrowse');</script></span>
                <div class="vbmenu_popup" id="childbrowse_menu" style="display:none"><table><tr><td>NECA-30048 (Disc 1)</td></tr></table></div>
                </td></tr>
                <tr><td nowrap="nowrap"><span class="label"><b>Release Date</b></span></td>
                <td><a href="/db/calendar.php?year=2001&month=12#20011221">Dec 21, 2001</a></td></tr>
                <tr class="maincred"><td nowrap="nowrap"><span class="label"><b>Publisher</b></span></td><td><a href="/org/55"><span class="productname" lang="en" style="display:inline">Nihon Falcom Corporation</span><span style="display:none"><em> / </em></span><span class="productname" lang="ja" style="display:none">日本ファルコム株式会社</span></a></td></tr>
                <tr class="maincred"><td nowrap="nowrap"><span class="label"><b><span title="Composer" class="artistname" lang="en" style="display:inline">Composer</span><span style="display:none"><em> / </em></span><span title="Composer" class="artistname" lang="ja" style="display:none">Composer</span></b></span></td><td width="100%"><a href="/artist/293"><span title="ファルコム" class="artistname" lang="en" style="display:inline">Falcom Sound Team jdk</span><span style="display:none"><em> / </em></span><span class="artistname" lang="ja" style="display:none">ファルコム・サウンド・チーム・jdk</span></a> (<a href="/artist/703"><span class="artistname" lang="en" style="display:inline">Atsushi Shirakawa</span><span class="artistname" lang="ja" style="display:none">白川篤史</span></a>, <a href="/artist/2151"><span class="artistname" lang="en" style="display:inline">Hayato Sonoda</span></a>)</td></tr>
            </table>
            <div>
                <h3>Tracklist</h3>
                <ul id="tlnav" class="tabnav"><li><a class='smallfont' rel='tl12381' href='#'>Japanese</a></li><li class="active"><a class='smallfont' rel='tl1033' href='#'>English</a></li></ul>
            </div>
            <div style="padding: 6px 10px 10px 10px" id="tracklist">
                <span class="tl" id="tl12381" style="display: none">
                    <span style="font-size:8pt"><b>Disc 1</b></span>
                    <table cellpadding="1" cellspacing="0" border="0" class="role">
                        <tr class="rolebit"><td><span class="label">01</span></td><td>日本語タイトル</td><td><span class="time">2:00</span></td></tr>
                    </table>
                </span>
                <span class="tl" id="tl1033">
                    <span style="font-size:8pt"><b>Disc 1</b></span>
                    <table cellpadding="1" cellspacing="0" border="0" class="role">
                        <tr class="rolebit">
                            <td class="smallfont"><span class="label">01</span></td>
                            <td class="smallfont" width="100%" colspan="2">Prelude - To the Ancient World</td>
                            <td class="smallfont" align="right"><span class="time">2:00</span></td>
                        </tr>
                        <tr class="rolebit">
                            <td class="smallfont"><span class="label">02</span></td>
                            <td class="smallfont" width="100%" colspan="2">Puck Village</td>
                            <td class="smallfont" align="right"><span class="time">3:15</span></td>
                        </tr>
                    </table>
                    <span style="float: right;">
                        <span class="smallfont">Disc length</span>
                        <span class="time">53:21</span>
                    </span>
                    <span style="font-size:8pt"><b>Disc 2</b></span>
                    <table cellpadding="1" cellspacing="0" border="0" class="role">
                        <tr class="rolebit">
                            <td class="smallfont"><span class="label">01</span></td>
                            <td class="smallfont" width="100%" colspan="2">Ending Theme</td>
                            <td class="smallfont" align="right"><span class="time">4:10</span></td>
                        </tr>
                    </table>
                </span>
            </div>
            <h3>Notes</h3>
        </body>
        </html>
        "##;

        let meta = parse_vgmdb_html(real_like_html, "").expect("Debe parsear HTML real de VGMdb");
        assert_eq!(meta.album_title, "ZWEI!! Original Sound Track");
        assert_eq!(meta.catalog, "NECA-30048~9");
        assert_eq!(meta.release_date, "Dec 21, 2001");
        assert_eq!(meta.publisher, "Nihon Falcom Corporation");
        assert_eq!(
            meta.composers,
            vec!["Falcom Sound Team jdk (Atsushi Shirakawa, Hayato Sonoda)"]
        );
        assert_eq!(meta.tracks.len(), 3);
        assert_eq!(meta.tracks[0].disc_number, 1);
        assert_eq!(meta.tracks[0].track_number, 1);
        assert_eq!(meta.tracks[0].title, "Prelude - To the Ancient World");
        assert_eq!(meta.tracks[0].duration, "2:00");
        assert_eq!(meta.tracks[1].disc_number, 1);
        assert_eq!(meta.tracks[1].track_number, 2);
        assert_eq!(meta.tracks[1].title, "Puck Village");
        assert_eq!(meta.tracks[2].disc_number, 2);
        assert_eq!(meta.tracks[2].track_number, 1);
        assert_eq!(meta.tracks[2].title, "Ending Theme");
        assert_eq!(meta.tracks[2].duration, "4:10");
    }

    #[test]
    fn test_parse_vgmdb_copied_text_full_page() {
        let copied = "\
VGMdb
Albums
Artists
Labels
Search
Login
Zill O'll -infinite- ORIGINAL SOUND TRACK & DRAMA
ジルオール インフィニット オリジナル・サウンドトラック＆ドラマ
Catalog Number\tKECH-1346~7
Release Date\tOct 26, 2005
Publish Format\tCommercial
Media Format\t2 CD
Classification\tOriginal Soundtrack, Drama
Published by\tKOEI Co., Ltd.
Composed by\tHiromi Mizutani, Koei BGM Division

Credits
Composed by\tHiromi Mizutani, Koei BGM Division

Tracklist
English
Disc 1 [KECH-1346]
01\tOpening\t2:14
02\tAwakening\t1:45
Disc length\t45:12
Disc 2 [KECH-1347]
01\tSecrecy\t1:55
02\tEnding\t4:32
Disc length\t48:10

Notes
All tracks composed by Koei BGM Division.
Album Stats
";
        assert!(looks_like_vgmdb_copied_text(copied));
        let meta = parse_vgmdb_copied_text(copied).expect("Debe parsear texto copiado de VGMdb");
        assert_eq!(meta.source, "VGMdb (Texto)");
        assert_eq!(meta.album_title, "Zill O'll -infinite- ORIGINAL SOUND TRACK & DRAMA");
        assert_eq!(meta.catalog, "KECH-1346~7");
        assert_eq!(meta.release_date, "Oct 26, 2005");
        assert_eq!(meta.publisher, "KOEI Co., Ltd.");
        assert_eq!(meta.composers, vec!["Hiromi Mizutani", "Koei BGM Division"]);
        assert_eq!(meta.tracks.len(), 4);
        assert_eq!(meta.tracks[0].disc_number, 1);
        assert_eq!(meta.tracks[0].track_number, 1);
        assert_eq!(meta.tracks[0].title, "Opening");
        assert_eq!(meta.tracks[0].duration, "2:14");
        assert_eq!(meta.tracks[2].disc_number, 2);
        assert_eq!(meta.tracks[2].track_number, 1);
        assert_eq!(meta.tracks[2].title, "Secrecy");
        assert_eq!(meta.tracks[3].track_number, 2);
        assert_eq!(meta.tracks[3].title, "Ending");
        assert_eq!(meta.tracks[3].duration, "4:32");
    }

    #[test]
    fn test_extract_discogs_release_id_and_parse_json() {
        assert_eq!(
            extract_discogs_release_id("https://www.discogs.com/release/1830265-Nobuo-Uematsu-Final-Fantasy-VII"),
            Some("1830265".to_string())
        );
        assert_eq!(
            extract_discogs_release_id("https://api.discogs.com/releases/753387"),
            None
        );
        assert_eq!(
            extract_discogs_release_id("https://discogs.com/release/753387"),
            Some("753387".to_string())
        );
        assert_eq!(extract_vgmdb_album_id("https://www.discogs.com/release/1830265"), None);

        let sample_discogs = serde_json::json!({
            "id": 753387,
            "title": "トバル ナンバーワン = Tobal No.1 (Original Sound Track)",
            "released": "1996-08-21",
            "labels": [
                {"name": "DigiCube (2)", "catno": "SSCX-10001"}
            ],
            "extraartists": [
                {"name": "Yasunori Mitsuda (2)", "role": "Composed By, Arranged By"},
                {"name": "Kenji Ito", "role": "Composed By"},
                {"name": "Haruki Nakayama", "role": "Vocals"}
            ],
            "tracklist": [
                {"position": "", "type_": "heading", "title": "Disc 1"},
                {"position": "1-1", "type_": "track", "title": "Tobal No.1", "duration": "1:09"},
                {"position": "2-1", "type_": "track", "title": "Finale", "duration": "3:11"}
            ]
        });

        let meta = parse_discogs_release_json(&sample_discogs, Some("SSCX-10001~2"), true)
            .expect("Debe parsear JSON de Discogs");
        assert_eq!(meta.source, "Discogs");
        assert_eq!(meta.album_title, "Tobal No.1 (Original Sound Track)");
        assert_eq!(meta.catalog, "SSCX-10001~2");
        assert_eq!(meta.release_date, "1996-08-21");
        assert_eq!(meta.publisher, "DigiCube");
        assert_eq!(meta.composers, vec!["Yasunori Mitsuda", "Kenji Ito"]);
        assert_eq!(meta.arrangers, vec!["Yasunori Mitsuda"]);
        assert_eq!(meta.performers, vec!["Haruki Nakayama"]);
        assert_eq!(meta.tracks.len(), 2);
        assert_eq!(meta.tracks[0].disc_number, 1);
        assert_eq!(meta.tracks[0].track_number, 1);
        assert_eq!(meta.tracks[1].disc_number, 2);
        assert_eq!(meta.tracks[1].track_number, 1);
    }

    #[test]
    fn test_build_vgmdb_browser_url() {
        assert_eq!(
            build_vgmdb_browser_url("https://vgmdb.net/album/10200"),
            "https://vgmdb.net/album/10200"
        );
        assert_eq!(
            build_vgmdb_browser_url("10200"),
            "https://vgmdb.net/album/10200"
        );
        assert_eq!(
            build_vgmdb_browser_url("KECH-1346~7"),
            "https://vgmdb.net/search?q=KECH-1346%7E7"
        );
        assert_eq!(build_vgmdb_browser_url(""), "https://vgmdb.net/");
    }
}
