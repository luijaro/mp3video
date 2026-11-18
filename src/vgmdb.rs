use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{find_ffprobe, new_hidden_command};

const DEFAULT_VGMDB_BASE_URL: &str = "https://vgmdb.info";
const MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org/ws/2";
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

fn parse_vgmdb_album_json(data: &Value, fallback_catalog: &str) -> Result<AlbumMetadata> {
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

fn lookup_via_vgmdb_api(query: &str, base_url: &str) -> Result<AlbumMetadata> {
    let base = base_url.trim_end_matches('/');
    let trimmed = query.trim();

    if let Some(id) = trimmed
        .strip_prefix("album/")
        .or_else(|| trimmed.chars().all(|c| c.is_ascii_digit()).then_some(trimmed))
    {
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
        bail!("Ingresá un código de catálogo (ej: SQEX-10001) o ID de VGMdb.");
    }

    let as_path = Path::new(trimmed);
    if as_path.is_file() && as_path.extension().and_then(OsStr::to_str).is_some_and(|e| e.eq_ignore_ascii_case("json")) {
        let raw = fs::read_to_string(as_path)
            .with_context(|| format!("No se pudo leer el JSON {}", as_path.display()))?;
        let val: Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
            .with_context(|| format!("El archivo {} no es un JSON válido", as_path.display()))?;
        return parse_vgmdb_album_json(&val, "");
    }

    let base_url = vgmdb_base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_VGMDB_BASE_URL);

    match lookup_via_vgmdb_api(trimmed, base_url) {
        Ok(album) => Ok(album),
        Err(vgmdb_err) => {
            if trimmed.chars().all(|c| c.is_ascii_digit()) {
                bail!(
                    "No se pudo consultar VGMdb para el ID {} ({:#}). Probá con el código de catálogo (ej: SQEX-10001).",
                    trimmed,
                    vgmdb_err
                );
            }
            lookup_via_musicbrainz(trimmed).map_err(|mb_err| {
                anyhow::anyhow!(
                    "No se encontró '{}' ni en VGMdb ({:#}) ni en MusicBrainz ({:#}).",
                    trimmed,
                    vgmdb_err,
                    mb_err
                )
            })
        }
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
}
