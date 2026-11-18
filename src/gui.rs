use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use windows_sys::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetDC, GetDeviceCaps, GetStockObject, ReleaseDC, SetBkColor,
    SetBkMode, COLOR_BTNFACE, HBRUSH, HFONT, HGDIOBJ, LOGPIXELSX, TRANSPARENT, WHITE_BRUSH,
};
use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
    ActivateActCtx, CreateActCtxW, ACTCTXW,
};
use windows_sys::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, EM_REPLACESEL, EM_SCROLLCARET, EM_SETLIMITTEXT, EM_SETSEL,
    ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, PBM_SETPOS, PBM_SETRANGE32,
};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, SHBrowseForFolderW, SHGetPathFromIDListW,
    ShellExecuteW, BFFM_INITIALIZED, BFFM_SETSELECTIONW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
    BROWSEINFOW, HDROP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    IsDialogMessageW, LoadCursorW, LoadIconW, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassExW, SendMessageW, SetProcessDPIAware, SetWindowLongPtrW, SetWindowTextW,
    ShowWindow, TranslateMessage, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON,
    BS_GROUPBOX, BS_PUSHBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL,
    CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY,
    GWLP_USERDATA, HMENU, IDC_ARROW, IDI_APPLICATION, IDYES, MB_ICONERROR, MB_ICONINFORMATION,
    MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DROPFILES,
    WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_EX_CLIENTEDGE,
    WS_GROUP, WS_HSCROLL, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    WS_VSCROLL,
};

use crate::cue::{
    build_cue_from_single_audio_file, build_tracks_from_directory, compute_timeline,
    cue_track_output_stem, format_tracks_for_review, format_youtube_timestamps, parse_cue_file,
    parse_tracks_from_review, resolve_cue_for_input, split_cue_to_mp3,
    try_extract_chapters_as_cue, try_extract_embedded_cuesheet, CueSheet, CueTrack,
};
use crate::vgmdb::{lookup_album, AlbumMetadata};
use crate::youtube::{
    authorize_interactive, default_token_store_path, has_saved_tokens, upload_videos_batch,
    PrivacyStatus,
};
use crate::{
    build_full_album_upload_item, build_upload_items, build_upload_items_for_cue,
    collect_audio_files, default_jobs, find_ffmpeg, full_album_output_path, has_extension,
    output_path_for, render_cue_track_to_mp4, render_full_album_to_mp4, render_single,
    resolve_image, write_master_tracklist_txt, write_video_sidecar_txt, FitMode, RenderConfig,
    RenderJob, AUDIO_EXTENSIONS, DEFAULT_DESC_TEMPLATE, DEFAULT_TITLE_TEMPLATE, IMAGE_EXTENSIONS,
};

const ACTCTX_FLAG_RESOURCE_NAME_VALID: u32 = 0x0000_0008;
const ACTCTX_FLAG_HMODULE_VALID: u32 = 0x0000_0080;

const WM_APP_LOG: u32 = WM_APP + 1;
const WM_APP_PROGRESS: u32 = WM_APP + 2;
const WM_APP_STATUS: u32 = WM_APP + 3;
const WM_APP_DONE: u32 = WM_APP + 4;
const WM_APP_VGMDB_DONE: u32 = WM_APP + 5;
const WM_APP_OAUTH_DONE: u32 = WM_APP + 6;

const ID_BTN_BROWSE_INPUT_DIR: usize = 101;
const ID_BTN_BROWSE_INPUT_FILE: usize = 102;
const ID_BTN_BROWSE_IMAGE: usize = 103;
const ID_BTN_CLEAR_IMAGE: usize = 104;
const ID_BTN_BROWSE_OUTPUT: usize = 105;
const ID_BTN_START: usize = 106;
const ID_BTN_CANCEL: usize = 107;
const ID_BTN_OPEN_OUTPUT: usize = 108;
const ID_BTN_VGMDB_SEARCH: usize = 109;
const ID_BTN_BROWSE_SECRET: usize = 110;
const ID_BTN_CONNECT_YT: usize = 111;
const ID_BTN_UPLOAD_ONLY: usize = 112;
const ID_BTN_SPLIT_CUE_MP3: usize = 113;
const ID_BTN_COPY_TIMESTAMPS: usize = 114;
const ID_CHK_FULL_ALBUM: usize = 115;

const RESOLUTIONS: &[(&str, (u32, u32))] = &[
    ("1920x1080 (Full HD 16:9)", (1920, 1080)),
    ("1280x720 (HD 16:9)", (1280, 720)),
    ("3840x2160 (4K UHD 16:9)", (3840, 2160)),
    ("1080x1080 (Cuadrado 1:1)", (1080, 1080)),
    ("1080x1920 (Vertical 9:16)", (1080, 1920)),
];

const FIT_MODES: &[(&str, FitMode)] = &[
    ("Fondo desenfocado (Blur - ideal portadas)", FitMode::Blur),
    ("Contener (Bordes negros)", FitMode::Contain),
    ("Cubrir (Recortar 16:9)", FitMode::Cover),
    ("Estirar", FitMode::Stretch),
];

const AUDIO_PRESETS: &[(&str, (&str, bool))] = &[
    ("AAC 192 kbps (Recomendado)", ("192k", false)),
    ("AAC 320 kbps (Alta calidad)", ("320k", false)),
    ("AAC 128 kbps", ("128k", false)),
    ("Copiar MP3 original (Ultra rápido)", ("192k", true)),
];

const JOBS_PRESETS: &[(&str, usize)] = &[
    ("1 hilo", 1),
    ("2 hilos", 2),
    ("3 hilos", 3),
    ("4 hilos", 4),
    ("6 hilos", 6),
    ("8 hilos", 8),
];

const FPS_PRESETS: &[(&str, u32)] = &[
    ("2 fps (Recomendado - muy rápido)", 2),
    ("1 fps (Máxima velocidad)", 1),
    ("5 fps", 5),
    ("30 fps", 30),
];

const CRF_PRESETS: &[(&str, u8)] = &[
    ("CRF 18 (Alta calidad)", 18),
    ("CRF 15 (Máxima)", 15),
    ("CRF 23 (Estándar)", 23),
    ("CRF 28 (Liviano)", 28),
];

const PRIVACY_PRESETS: &[(&str, PrivacyStatus)] = &[
    ("No listado (Oculto)", PrivacyStatus::Unlisted),
    ("Privado", PrivacyStatus::Private),
    ("Público", PrivacyStatus::Public),
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct AppSettings {
    input_path: String,
    image_path: String,
    output_path: String,
    client_secret_path: String,
    vgmdb_code: String,
    album_title: String,
    composers: String,
    title_template: String,
    desc_template: String,
    res_idx: usize,
    fit_idx: usize,
    audio_idx: usize,
    jobs_idx: usize,
    fps_idx: usize,
    crf_idx: usize,
    privacy_idx: usize,
    recursive: bool,
    overwrite: bool,
    yt_upload_after: bool,
    yt_playlist: bool,
    full_album: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        let def_jobs = default_jobs();
        let jobs_idx = JOBS_PRESETS
            .iter()
            .position(|(_, j)| *j == def_jobs)
            .unwrap_or(1);
        Self {
            input_path: String::new(),
            image_path: String::new(),
            output_path: String::new(),
            client_secret_path: String::new(),
            vgmdb_code: String::new(),
            album_title: String::new(),
            composers: String::new(),
            title_template: DEFAULT_TITLE_TEMPLATE.to_string(),
            desc_template: DEFAULT_DESC_TEMPLATE.to_string(),
            res_idx: 0,
            fit_idx: 0,
            audio_idx: 0,
            jobs_idx,
            fps_idx: 0,
            crf_idx: 0,
            privacy_idx: 0,
            recursive: false,
            overwrite: false,
            yt_upload_after: false,
            yt_playlist: false,
            full_album: false,
        }
    }
}

struct BatchSummary {
    headline: String,
    err_count: usize,
    error_message: Option<String>,
}

struct UploadSettings {
    upload_after_render: bool,
    create_playlist: bool,
    client_secret_path: Option<PathBuf>,
    privacy: PrivacyStatus,
    album_meta: Option<AlbumMetadata>,
    title_template: String,
    desc_template: String,
}

struct GuiState {
    dpi: i32,
    font_regular: HFONT,
    font_bold: HFONT,
    font_small: HFONT,
    edit_input: HWND,
    btn_browse_input_dir: HWND,
    btn_browse_input_file: HWND,
    edit_image: HWND,
    btn_browse_image: HWND,
    btn_clear_image: HWND,
    edit_output: HWND,
    btn_browse_output: HWND,
    combo_res: HWND,
    combo_fit: HWND,
    combo_audio: HWND,
    combo_jobs: HWND,
    combo_fps: HWND,
    combo_crf: HWND,
    chk_recursive: HWND,
    chk_overwrite: HWND,
    edit_vgmdb_code: HWND,
    btn_vgmdb_search: HWND,
    lbl_vgmdb_info: HWND,
    edit_album: HWND,
    edit_composers: HWND,
    edit_title_tpl: HWND,
    edit_desc_tpl: HWND,
    chk_full_album: HWND,
    btn_copy_timestamps: HWND,
    lbl_cue_info: HWND,
    edit_cue_tracks: HWND,
    edit_client_secret: HWND,
    btn_browse_secret: HWND,
    btn_connect_yt: HWND,
    combo_privacy: HWND,
    chk_yt_upload_after: HWND,
    chk_yt_playlist: HWND,
    lbl_yt_auth: HWND,
    btn_start: HWND,
    btn_split_cue: HWND,
    btn_upload_only: HWND,
    btn_cancel: HWND,
    btn_open_out: HWND,
    progress: HWND,
    lbl_status: HWND,
    edit_log: HWND,
    album_meta: Option<AlbumMetadata>,
    cue_sheet: Option<CueSheet>,
    is_running: bool,
    is_searching: bool,
    is_authing: bool,
    cancel_flag: Arc<AtomicBool>,
}

impl GuiState {
    fn new(dpi: i32) -> Self {
        Self {
            dpi,
            font_regular: null_mut(),
            font_bold: null_mut(),
            font_small: null_mut(),
            edit_input: null_mut(),
            btn_browse_input_dir: null_mut(),
            btn_browse_input_file: null_mut(),
            edit_image: null_mut(),
            btn_browse_image: null_mut(),
            btn_clear_image: null_mut(),
            edit_output: null_mut(),
            btn_browse_output: null_mut(),
            combo_res: null_mut(),
            combo_fit: null_mut(),
            combo_audio: null_mut(),
            combo_jobs: null_mut(),
            combo_fps: null_mut(),
            combo_crf: null_mut(),
            chk_recursive: null_mut(),
            chk_overwrite: null_mut(),
            edit_vgmdb_code: null_mut(),
            btn_vgmdb_search: null_mut(),
            lbl_vgmdb_info: null_mut(),
            edit_album: null_mut(),
            edit_composers: null_mut(),
            edit_title_tpl: null_mut(),
            edit_desc_tpl: null_mut(),
            chk_full_album: null_mut(),
            btn_copy_timestamps: null_mut(),
            lbl_cue_info: null_mut(),
            edit_cue_tracks: null_mut(),
            edit_client_secret: null_mut(),
            btn_browse_secret: null_mut(),
            btn_connect_yt: null_mut(),
            combo_privacy: null_mut(),
            chk_yt_upload_after: null_mut(),
            chk_yt_playlist: null_mut(),
            lbl_yt_auth: null_mut(),
            btn_start: null_mut(),
            btn_split_cue: null_mut(),
            btn_upload_only: null_mut(),
            btn_cancel: null_mut(),
            btn_open_out: null_mut(),
            progress: null_mut(),
            lbl_status: null_mut(),
            edit_log: null_mut(),
            album_meta: None,
            cue_sheet: None,
            is_running: false,
            is_searching: false,
            is_authing: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    fn px(&self, logical_px: i32) -> i32 {
        (logical_px * self.dpi + 48) / 96
    }

    fn editable_controls(&self) -> [HWND; 33] {
        [
            self.edit_input,
            self.btn_browse_input_dir,
            self.btn_browse_input_file,
            self.edit_image,
            self.btn_browse_image,
            self.btn_clear_image,
            self.edit_output,
            self.btn_browse_output,
            self.combo_res,
            self.combo_fit,
            self.combo_audio,
            self.combo_jobs,
            self.combo_fps,
            self.combo_crf,
            self.chk_recursive,
            self.chk_overwrite,
            self.edit_vgmdb_code,
            self.btn_vgmdb_search,
            self.edit_album,
            self.edit_composers,
            self.edit_title_tpl,
            self.edit_desc_tpl,
            self.chk_full_album,
            self.btn_copy_timestamps,
            self.edit_cue_tracks,
            self.edit_client_secret,
            self.btn_browse_secret,
            self.btn_connect_yt,
            self.combo_privacy,
            self.chk_yt_upload_after,
            self.chk_yt_playlist,
            self.btn_split_cue,
            self.btn_upload_only,
        ]
    }
}

fn settings_file_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("mp3video_settings.json");
        }
    }
    PathBuf::from("mp3video_settings.json")
}

fn load_settings() -> AppSettings {
    let path = settings_file_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return AppSettings::default();
    };
    serde_json::from_str::<AppSettings>(raw.trim_start_matches('\u{feff}')).unwrap_or_default()
}

fn save_settings(state: &GuiState) {
    let s = AppSettings {
        input_path: get_window_text(state.edit_input).trim().to_string(),
        image_path: get_window_text(state.edit_image).trim().to_string(),
        output_path: get_window_text(state.edit_output).trim().to_string(),
        client_secret_path: get_window_text(state.edit_client_secret).trim().to_string(),
        vgmdb_code: get_window_text(state.edit_vgmdb_code).trim().to_string(),
        album_title: get_window_text(state.edit_album).trim().to_string(),
        composers: get_window_text(state.edit_composers).trim().to_string(),
        title_template: get_window_text(state.edit_title_tpl),
        desc_template: get_window_text(state.edit_desc_tpl).replace("\r\n", "\n"),
        res_idx: combo_selected_idx(state.combo_res),
        fit_idx: combo_selected_idx(state.combo_fit),
        audio_idx: combo_selected_idx(state.combo_audio),
        jobs_idx: combo_selected_idx(state.combo_jobs),
        fps_idx: combo_selected_idx(state.combo_fps),
        crf_idx: combo_selected_idx(state.combo_crf),
        privacy_idx: combo_selected_idx(state.combo_privacy),
        recursive: is_checked(state.chk_recursive),
        overwrite: is_checked(state.chk_overwrite),
        yt_upload_after: is_checked(state.chk_yt_upload_after),
        yt_playlist: is_checked(state.chk_yt_playlist),
        full_album: is_checked(state.chk_full_album),
    };
    if let Ok(json) = serde_json::to_string_pretty(&s) {
        let _ = fs::write(settings_file_path(), json);
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    wide.push(0);
    wide
}

fn from_wide_slice(slice: &[u16]) -> String {
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..end])
}

fn send_msg(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { SendMessageW(hwnd, msg, wparam, lparam) }
}

fn post_boxed<T>(hwnd: HWND, msg: u32, wparam: WPARAM, payload: T) {
    let raw = Box::into_raw(Box::new(payload));
    let ok = unsafe { PostMessageW(hwnd, msg, wparam, raw as LPARAM) };
    if ok == 0 {
        unsafe {
            drop(Box::from_raw(raw));
        }
    }
}

fn set_window_text(hwnd: HWND, text: &str) {
    let w = to_wide(text);
    unsafe {
        SetWindowTextW(hwnd, w.as_ptr());
    }
}

fn get_window_text(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let read = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if read <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..read as usize])
}

fn append_log(edit_log: HWND, line: &str) {
    let current_len = unsafe { GetWindowTextLengthW(edit_log) };
    let text = if current_len == 0 {
        line.to_string()
    } else {
        format!("\r\n{}", line)
    };
    let wide = to_wide(&text);
    send_msg(edit_log, EM_SETSEL, current_len as WPARAM, current_len as LPARAM);
    send_msg(edit_log, EM_REPLACESEL, 0, wide.as_ptr() as LPARAM);
    send_msg(edit_log, EM_SCROLLCARET, 0, 0);
}

fn set_font(hwnd: HWND, font: HFONT) {
    if !hwnd.is_null() && !font.is_null() {
        send_msg(hwnd, WM_SETFONT, font as WPARAM, 1);
    }
}

fn create_font(dpi: i32, pt: i32, weight: i32) -> HFONT {
    let height = -((pt * dpi + 36) / 72);
    let face = to_wide("Segoe UI");
    unsafe {
        CreateFontW(
            height, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn create_child(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    ex_style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
    font: HFONT,
) -> HWND {
    let cls_w = to_wide(class);
    let txt_w = to_wide(text);
    let hinst = unsafe { GetModuleHandleW(null()) };
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            cls_w.as_ptr(),
            txt_w.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            w,
            h,
            parent,
            id as HMENU,
            hinst,
            null(),
        )
    };
    set_font(hwnd, font);
    hwnd
}

fn populate_combo<T>(combo: HWND, items: &[(&str, T)], default_idx: usize) {
    for (label, _) in items {
        let w = to_wide(label);
        send_msg(combo, CB_ADDSTRING, 0, w.as_ptr() as LPARAM);
    }
    let idx = default_idx.min(items.len().saturating_sub(1));
    send_msg(combo, CB_SETCURSEL, idx as WPARAM, 0);
}

fn combo_selected_idx(combo: HWND) -> usize {
    let idx = send_msg(combo, CB_GETCURSEL, 0, 0);
    if idx < 0 {
        0
    } else {
        idx as usize
    }
}

fn is_checked(chk: HWND) -> bool {
    send_msg(chk, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED
}

fn set_checked(chk: HWND, checked: bool) {
    send_msg(
        chk,
        BM_SETCHECK,
        if checked { BST_CHECKED as usize } else { 0 },
        0,
    );
}

fn show_msg_box(parent: HWND, title: &str, message: &str, flags: u32) -> i32 {
    let t = to_wide(title);
    let m = to_wide(message);
    unsafe { MessageBoxW(parent, m.as_ptr(), t.as_ptr(), flags) }
}

pub(crate) fn show_fatal_error(message: &str) {
    show_msg_box(
        null_mut(),
        "MP3 a Video - Error",
        message,
        MB_OK | MB_ICONERROR,
    );
}

unsafe extern "system" fn browse_callback(
    hwnd: HWND,
    umsg: u32,
    _lparam: LPARAM,
    lpdata: LPARAM,
) -> i32 {
    if umsg == BFFM_INITIALIZED && lpdata != 0 {
        SendMessageW(hwnd, BFFM_SETSELECTIONW, 1, lpdata);
    }
    0
}

fn pick_folder(owner: HWND, title: &str, initial_path: &str) -> Option<PathBuf> {
    let title_w = to_wide(title);
    let mut display_buf = [0u16; 260];
    let init_trim = initial_path.trim();
    let init_w = if !init_trim.is_empty() && Path::new(init_trim).is_dir() {
        Some(to_wide(init_trim))
    } else {
        None
    };

    let bi = BROWSEINFOW {
        hwndOwner: owner,
        pidlRoot: null_mut(),
        pszDisplayName: display_buf.as_mut_ptr(),
        lpszTitle: title_w.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        lpfn: Some(browse_callback),
        lParam: init_w.as_ref().map_or(0, |w| w.as_ptr() as LPARAM),
        iImage: 0,
    };

    let pidl = unsafe { SHBrowseForFolderW(&bi) };
    if pidl.is_null() {
        return None;
    }

    let mut path_buf = vec![0u16; 4096];
    let ok = unsafe { SHGetPathFromIDListW(pidl, path_buf.as_mut_ptr()) };
    unsafe {
        CoTaskMemFree(pidl as *const _);
    }
    if ok == 0 {
        return None;
    }

    let selected = from_wide_slice(&path_buf);
    if selected.is_empty() {
        None
    } else {
        Some(PathBuf::from(selected))
    }
}

fn pick_file(owner: HWND, title: &str, filter_pairs: &[(&str, &str)], initial: &str) -> Option<PathBuf> {
    let title_w = to_wide(title);
    let mut filter_w: Vec<u16> = Vec::new();
    for (desc, pat) in filter_pairs {
        filter_w.extend(desc.encode_utf16());
        filter_w.push(0);
        filter_w.extend(pat.encode_utf16());
        filter_w.push(0);
    }
    filter_w.push(0);

    let init_dir_w = {
        let p = Path::new(initial.trim());
        let dir = if p.is_dir() {
            Some(p)
        } else {
            p.parent().filter(|parent| parent.is_dir())
        };
        dir.map(|d| to_wide(&d.to_string_lossy()))
    };

    let mut file_buf = vec![0u16; 4096];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter_w.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = file_buf.len() as u32;
    ofn.lpstrInitialDir = init_dir_w.as_ref().map_or(null(), |w| w.as_ptr());
    ofn.lpstrTitle = title_w.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        return None;
    }

    let selected = from_wide_slice(&file_buf);
    if selected.is_empty() {
        None
    } else {
        Some(PathBuf::from(selected))
    }
}

fn enable_visual_styles() {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }
    }

    let shell32 = to_wide("shell32.dll");
    let h_shell = unsafe { LoadLibraryW(shell32.as_ptr()) };
    if !h_shell.is_null() {
        let mut actctx: ACTCTXW = unsafe { std::mem::zeroed() };
        actctx.cbSize = std::mem::size_of::<ACTCTXW>() as u32;
        actctx.dwFlags = ACTCTX_FLAG_RESOURCE_NAME_VALID | ACTCTX_FLAG_HMODULE_VALID;
        actctx.lpResourceName = 124usize as *const u16;
        actctx.hModule = h_shell;
        let h_ctx = unsafe { CreateActCtxW(&actctx) };
        if !h_ctx.is_null() && h_ctx != (-1isize as _) {
            let mut cookie = 0usize;
            unsafe {
                ActivateActCtx(h_ctx, &mut cookie);
            }
        }
    }

    let icc = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES | ICC_PROGRESS_CLASS,
    };
    unsafe {
        InitCommonControlsEx(&icc);
    }
}

fn detect_default_client_secret() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("client_secret.json"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("client_secret.json"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn update_yt_auth_label(state: &GuiState) {
    let connected = has_saved_tokens(&default_token_store_path());
    set_window_text(
        state.lbl_yt_auth,
        if connected {
            "Conectado ✓"
        } else {
            "No conectado"
        },
    );
}

fn clear_cue_ui(state: &mut GuiState) {
    state.cue_sheet = None;
    set_window_text(
        state.lbl_cue_info,
        "Sin pistas cargadas (elegí o arrastrá un .cue, .flac o carpeta con música)",
    );
    set_window_text(state.edit_cue_tracks, "");
}

fn copy_text_to_clipboard(hwnd: HWND, text: &str) -> Result<()> {
    let wide = to_wide(text);
    let byte_len = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        if OpenClipboard(hwnd) == 0 {
            bail!("No se pudo abrir el portapapeles de Windows.");
        }
    }

    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
    if hmem.is_null() {
        unsafe {
            CloseClipboard();
        }
        bail!("Sin memoria al copiar al portapapeles.");
    }

    let ptr = unsafe { GlobalLock(hmem) } as *mut u16;
    if ptr.is_null() {
        unsafe {
            CloseClipboard();
        }
        bail!("No se pudo bloquear la memoria del portapapeles.");
    }

    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        GlobalUnlock(hmem);
        EmptyClipboard();
        let res = SetClipboardData(CF_UNICODETEXT as u32, hmem as HANDLE);
        CloseClipboard();
        if res.is_null() {
            bail!("Windows rechazó los datos en el portapapeles.");
        }
    }

    Ok(())
}

fn apply_loaded_cue_to_ui(
    state: &mut GuiState,
    label: &str,
    cue: CueSheet,
    populate_album_fields: bool,
) {
    set_window_text(state.lbl_cue_info, label);
    set_window_text(state.edit_cue_tracks, &format_tracks_for_review(&cue.tracks));

    if populate_album_fields {
        if get_window_text(state.edit_album).trim().is_empty() {
            if let Some(ref alb) = cue.album_title {
                set_window_text(state.edit_album, alb);
            }
        }
        if get_window_text(state.edit_composers).trim().is_empty() {
            if let Some(ref perf) = cue.performer {
                set_window_text(state.edit_composers, perf);
            }
        }
        if get_window_text(state.edit_vgmdb_code).trim().is_empty() {
            if let Some(ref cat) = cue.catalog {
                set_window_text(state.edit_vgmdb_code, cat);
            }
        }
    }

    state.cue_sheet = Some(cue);
}

fn try_load_cue_for_path(state: &mut GuiState, path: &Path, populate_album_fields: bool) {
    if let Some(cue_path) = resolve_cue_for_input(path) {
        match parse_cue_file(&cue_path) {
            Ok(cue) => {
                let audio_name = cue
                    .audio_path
                    .file_name()
                    .unwrap_or_else(|| OsStr::new("?"))
                    .to_string_lossy()
                    .to_string();
                let label = format!(
                    "CUE: {} ({} pistas) — revisá o editá los títulos abajo:",
                    audio_name,
                    cue.tracks.len()
                );
                append_log(
                    state.edit_log,
                    &format!(
                        "[CUE] Cargado {} -> Audio: {} ({} pistas)",
                        cue.cue_path.display(),
                        audio_name,
                        cue.tracks.len()
                    ),
                );
                apply_loaded_cue_to_ui(state, &label, cue, populate_album_fields);
                return;
            }
            Err(err) => {
                clear_cue_ui(state);
                append_log(state.edit_log, &format!("[CUE AVISO] {:#}", err));
                return;
            }
        }
    }

    let ffmpeg = find_ffmpeg(None).ok();
    if path.is_file() {
        if let Some(cue) = try_extract_embedded_cuesheet(path, ffmpeg.as_deref())
            .or_else(|| try_extract_chapters_as_cue(path, ffmpeg.as_deref()))
        {
            let audio_name = path
                .file_name()
                .unwrap_or_else(|| OsStr::new("?"))
                .to_string_lossy()
                .to_string();
            let label = format!(
                "CUE embebido en {}: ({} pistas) — revisá o editá los títulos abajo:",
                audio_name,
                cue.tracks.len()
            );
            append_log(
                state.edit_log,
                &format!(
                    "[CUE EMBEBIDO] Detectadas {} pistas en '{}'",
                    cue.tracks.len(),
                    path.display()
                ),
            );
            apply_loaded_cue_to_ui(state, &label, cue, populate_album_fields);
            return;
        }

        if is_checked(state.chk_full_album) && has_extension(path, AUDIO_EXTENSIONS) {
            if let Ok(cue) = build_cue_from_single_audio_file(
                path,
                state.album_meta.as_ref(),
                ffmpeg.as_deref(),
            ) {
                let label = format!(
                    "Pista individual: {} — revisá o editá los títulos abajo:",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                apply_loaded_cue_to_ui(state, &label, cue, populate_album_fields);
                return;
            }
        }
    }

    clear_cue_ui(state);
}

fn try_load_directory_into_review(
    state: &mut GuiState,
    dir: &Path,
    populate_album_fields: bool,
    log_errors: bool,
) {
    let out_raw = get_window_text(state.edit_output).trim().to_string();
    let output_dir = if out_raw.is_empty() {
        dir.join("output")
    } else {
        PathBuf::from(out_raw)
    };
    let recursive = is_checked(state.chk_recursive);
    let ffmpeg = find_ffmpeg(None).ok();

    match build_tracks_from_directory(
        dir,
        &output_dir,
        recursive,
        state.album_meta.as_ref(),
        ffmpeg.as_deref(),
    ) {
        Ok(cue) => {
            let count = cue.tracks.len();
            let label = format!(
                "Álbum completo en carpeta: {} pista(s) — revisá o editá los títulos abajo:",
                count
            );
            append_log(
                state.edit_log,
                &format!(
                    "[ÁLBUM] Cargadas {} pista(s) desde '{}'.",
                    count,
                    dir.display()
                ),
            );
            apply_loaded_cue_to_ui(state, &label, cue, populate_album_fields);
        }
        Err(err) => {
            clear_cue_ui(state);
            if log_errors {
                append_log(state.edit_log, &format!("[ÁLBUM AVISO] {:#}", err));
            }
        }
    }
}

fn build_controls(hwnd: HWND, state: &mut GuiState) {
    let saved = load_settings();
    let f_reg = state.font_regular;
    let f_bold = state.font_bold;
    let f_small = state.font_small;

    // Encabezado
    create_child(
        hwnd,
        "STATIC",
        "MP3 / CUE+FLAC a Video + VGMdb + YouTube",
        0,
        0,
        state.px(16),
        state.px(6),
        state.px(520),
        state.px(18),
        0,
        f_bold,
    );
    create_child(
        hwnd,
        "STATIC",
        "Arrastrá una carpeta, .mp3, .cue, .flac, imagen o client_secret.json sobre la ventana.",
        0,
        0,
        state.px(16),
        state.px(24),
        state.px(868),
        state.px(15),
        0,
        f_small,
    );

    // Entradas y salida
    create_child(
        hwnd,
        "STATIC",
        "Entrada:",
        0,
        0,
        state.px(16),
        state.px(45),
        state.px(110),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_input = create_child(
        hwnd,
        "EDIT",
        "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(130),
        state.px(42),
        state.px(506),
        state.px(23),
        0,
        f_reg,
    );
    state.btn_browse_input_dir = create_child(
        hwnd,
        "BUTTON",
        "Carpeta...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(642),
        state.px(42),
        state.px(106),
        state.px(23),
        ID_BTN_BROWSE_INPUT_DIR,
        f_reg,
    );
    state.btn_browse_input_file = create_child(
        hwnd,
        "BUTTON",
        "MP3/CUE/FLAC...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(754),
        state.px(42),
        state.px(130),
        state.px(23),
        ID_BTN_BROWSE_INPUT_FILE,
        f_reg,
    );

    create_child(
        hwnd,
        "STATIC",
        "Imagen (opcional):",
        0,
        0,
        state.px(16),
        state.px(73),
        state.px(110),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_image = create_child(
        hwnd,
        "EDIT",
        "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(130),
        state.px(70),
        state.px(506),
        state.px(23),
        0,
        f_reg,
    );
    state.btn_browse_image = create_child(
        hwnd,
        "BUTTON",
        "Imagen...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(642),
        state.px(70),
        state.px(106),
        state.px(23),
        ID_BTN_BROWSE_IMAGE,
        f_reg,
    );
    state.btn_clear_image = create_child(
        hwnd,
        "BUTTON",
        "Limpiar",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(754),
        state.px(70),
        state.px(130),
        state.px(23),
        ID_BTN_CLEAR_IMAGE,
        f_reg,
    );

    create_child(
        hwnd,
        "STATIC",
        "Carpeta salida:",
        0,
        0,
        state.px(16),
        state.px(101),
        state.px(110),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_output = create_child(
        hwnd,
        "EDIT",
        "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(130),
        state.px(98),
        state.px(618),
        state.px(23),
        0,
        f_reg,
    );
    state.btn_browse_output = create_child(
        hwnd,
        "BUTTON",
        "Carpeta...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(754),
        state.px(98),
        state.px(130),
        state.px(23),
        ID_BTN_BROWSE_OUTPUT,
        f_reg,
    );

    // Grupo Video y Audio
    create_child(
        hwnd,
        "BUTTON",
        "Configuración de Video y Audio",
        BS_GROUPBOX as u32 | WS_GROUP,
        0,
        state.px(16),
        state.px(128),
        state.px(868),
        state.px(104),
        0,
        f_bold,
    );

    create_child(
        hwnd,
        "STATIC",
        "Resolución:",
        0,
        0,
        state.px(28),
        state.px(151),
        state.px(76),
        state.px(18),
        0,
        f_reg,
    );
    state.combo_res = create_child(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
        0,
        state.px(108),
        state.px(148),
        state.px(204),
        state.px(200),
        0,
        f_reg,
    );
    populate_combo(state.combo_res, RESOLUTIONS, saved.res_idx);

    create_child(
        hwnd,
        "STATIC",
        "Ajuste:",
        0,
        0,
        state.px(324),
        state.px(151),
        state.px(48),
        state.px(18),
        0,
        f_reg,
    );
    state.combo_fit = create_child(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
        0,
        state.px(376),
        state.px(148),
        state.px(268),
        state.px(200),
        0,
        f_reg,
    );
    populate_combo(state.combo_fit, FIT_MODES, saved.fit_idx);

    create_child(
        hwnd,
        "STATIC",
        "FPS:",
        0,
        0,
        state.px(656),
        state.px(151),
        state.px(30),
        state.px(18),
        0,
        f_reg,
    );
    state.combo_fps = create_child(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
        0,
        state.px(690),
        state.px(148),
        state.px(182),
        state.px(200),
        0,
        f_reg,
    );
    populate_combo(state.combo_fps, FPS_PRESETS, saved.fps_idx);

    create_child(
        hwnd,
        "STATIC",
        "Audio:",
        0,
        0,
        state.px(28),
        state.px(180),
        state.px(76),
        state.px(18),
        0,
        f_reg,
    );
    state.combo_audio = create_child(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
        0,
        state.px(108),
        state.px(177),
        state.px(204),
        state.px(200),
        0,
        f_reg,
    );
    populate_combo(state.combo_audio, AUDIO_PRESETS, saved.audio_idx);

    create_child(
        hwnd,
        "STATIC",
        "Calidad:",
        0,
        0,
        state.px(324),
        state.px(180),
        state.px(50),
        state.px(18),
        0,
        f_reg,
    );
    state.combo_crf = create_child(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
        0,
        state.px(376),
        state.px(177),
        state.px(172),
        state.px(200),
        0,
        f_reg,
    );
    populate_combo(state.combo_crf, CRF_PRESETS, saved.crf_idx);

    create_child(
        hwnd,
        "STATIC",
        "Paralelo:",
        0,
        0,
        state.px(560),
        state.px(180),
        state.px(56),
        state.px(18),
        0,
        f_reg,
    );
    state.combo_jobs = create_child(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
        0,
        state.px(620),
        state.px(177),
        state.px(252),
        state.px(200),
        0,
        f_reg,
    );
    populate_combo(state.combo_jobs, JOBS_PRESETS, saved.jobs_idx);

    state.chk_recursive = create_child(
        hwnd,
        "BUTTON",
        "Buscar también en subcarpetas",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        0,
        state.px(108),
        state.px(205),
        state.px(220),
        state.px(20),
        0,
        f_reg,
    );
    set_checked(state.chk_recursive, saved.recursive);

    state.chk_overwrite = create_child(
        hwnd,
        "BUTTON",
        "Sobrescribir archivos existentes",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        0,
        state.px(376),
        state.px(205),
        state.px(260),
        state.px(20),
        0,
        f_reg,
    );
    set_checked(state.chk_overwrite, saved.overwrite);

    // Grupo VGMdb / MusicBrainz
    create_child(
        hwnd,
        "BUTTON",
        "Metadatos del Álbum (VGMdb / MusicBrainz)",
        BS_GROUPBOX as u32 | WS_GROUP,
        0,
        state.px(16),
        state.px(238),
        state.px(868),
        state.px(134),
        0,
        f_bold,
    );

    create_child(
        hwnd,
        "STATIC",
        "Código / ID:",
        0,
        0,
        state.px(28),
        state.px(261),
        state.px(88),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_vgmdb_code = create_child(
        hwnd,
        "EDIT",
        &saved.vgmdb_code,
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(120),
        state.px(258),
        state.px(184),
        state.px(23),
        0,
        f_reg,
    );
    state.btn_vgmdb_search = create_child(
        hwnd,
        "BUTTON",
        "🔍 Buscar Disco",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(310),
        state.px(258),
        state.px(140),
        state.px(23),
        ID_BTN_VGMDB_SEARCH,
        f_reg,
    );
    state.lbl_vgmdb_info = create_child(
        hwnd,
        "STATIC",
        "Sin disco cargado (ej: SQEX-10001)",
        0,
        0,
        state.px(460),
        state.px(261),
        state.px(412),
        state.px(16),
        0,
        f_small,
    );

    create_child(
        hwnd,
        "STATIC",
        "Álbum:",
        0,
        0,
        state.px(28),
        state.px(289),
        state.px(88),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_album = create_child(
        hwnd,
        "EDIT",
        &saved.album_title,
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(120),
        state.px(286),
        state.px(300),
        state.px(23),
        0,
        f_reg,
    );
    create_child(
        hwnd,
        "STATIC",
        "Compositores:",
        0,
        0,
        state.px(430),
        state.px(289),
        state.px(92),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_composers = create_child(
        hwnd,
        "EDIT",
        &saved.composers,
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(526),
        state.px(286),
        state.px(346),
        state.px(23),
        0,
        f_reg,
    );

    create_child(
        hwnd,
        "STATIC",
        "Plantilla Título:",
        0,
        0,
        state.px(28),
        state.px(317),
        state.px(88),
        state.px(18),
        0,
        f_reg,
    );
    let title_init = if saved.title_template.trim().is_empty() {
        DEFAULT_TITLE_TEMPLATE
    } else {
        &saved.title_template
    };
    state.edit_title_tpl = create_child(
        hwnd,
        "EDIT",
        title_init,
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(120),
        state.px(314),
        state.px(752),
        state.px(23),
        0,
        f_reg,
    );

    create_child(
        hwnd,
        "STATIC",
        "Descripción:",
        0,
        0,
        state.px(28),
        state.px(345),
        state.px(88),
        state.px(18),
        0,
        f_reg,
    );
    let desc_src = if saved.desc_template.trim().is_empty() {
        DEFAULT_DESC_TEMPLATE
    } else {
        &saved.desc_template
    };
    let desc_crlf = desc_src.replace("\r\n", "\n").replace('\n', "\r\n");
    state.edit_desc_tpl = create_child(
        hwnd,
        "EDIT",
        &desc_crlf,
        ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | WS_VSCROLL | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(120),
        state.px(341),
        state.px(752),
        state.px(25),
        0,
        f_small,
    );

    // Grupo Pistas CUE / FLAC / Álbum completo (revisión en pantalla)
    create_child(
        hwnd,
        "BUTTON",
        "Pistas CUE / FLAC o Álbum completo (revisar o editar títulos)",
        BS_GROUPBOX as u32 | WS_GROUP,
        0,
        state.px(16),
        state.px(378),
        state.px(868),
        state.px(122),
        0,
        f_bold,
    );
    state.chk_full_album = create_child(
        hwnd,
        "BUTTON",
        "Álbum completo (1 solo video con timestamps / capítulos)",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        0,
        state.px(28),
        state.px(395),
        state.px(420),
        state.px(20),
        ID_CHK_FULL_ALBUM,
        f_bold,
    );
    set_checked(state.chk_full_album, saved.full_album);

    state.btn_copy_timestamps = create_child(
        hwnd,
        "BUTTON",
        "📋 Copiar Timestamps",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(690),
        state.px(393),
        state.px(182),
        state.px(23),
        ID_BTN_COPY_TIMESTAMPS,
        f_reg,
    );

    state.lbl_cue_info = create_child(
        hwnd,
        "STATIC",
        "Sin pistas cargadas (elegí o arrastrá un .cue, .flac o carpeta con música)",
        0,
        0,
        state.px(28),
        state.px(418),
        state.px(844),
        state.px(16),
        0,
        f_small,
    );
    state.edit_cue_tracks = create_child(
        hwnd,
        "EDIT",
        "",
        ES_MULTILINE as u32
            | ES_AUTOVSCROLL as u32
            | ES_AUTOHSCROLL as u32
            | WS_VSCROLL
            | WS_HSCROLL
            | WS_TABSTOP
            | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(28),
        state.px(436),
        state.px(844),
        state.px(58),
        0,
        f_small,
    );

    // Grupo YouTube
    create_child(
        hwnd,
        "BUTTON",
        "Subida a YouTube",
        BS_GROUPBOX as u32 | WS_GROUP,
        0,
        state.px(16),
        state.px(506),
        state.px(868),
        state.px(80),
        0,
        f_bold,
    );

    create_child(
        hwnd,
        "STATIC",
        "client_secret:",
        0,
        0,
        state.px(28),
        state.px(528),
        state.px(88),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_client_secret = create_child(
        hwnd,
        "EDIT",
        "",
        ES_AUTOHSCROLL as u32 | WS_TABSTOP | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(120),
        state.px(525),
        state.px(460),
        state.px(23),
        0,
        f_reg,
    );
    state.btn_browse_secret = create_child(
        hwnd,
        "BUTTON",
        "Examinar...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(586),
        state.px(525),
        state.px(116),
        state.px(23),
        ID_BTN_BROWSE_SECRET,
        f_reg,
    );
    state.btn_connect_yt = create_child(
        hwnd,
        "BUTTON",
        "🔑 Conectar YouTube",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(708),
        state.px(525),
        state.px(164),
        state.px(23),
        ID_BTN_CONNECT_YT,
        f_reg,
    );

    create_child(
        hwnd,
        "STATIC",
        "Privacidad:",
        0,
        0,
        state.px(28),
        state.px(558),
        state.px(88),
        state.px(18),
        0,
        f_reg,
    );
    state.combo_privacy = create_child(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
        0,
        state.px(120),
        state.px(555),
        state.px(174),
        state.px(200),
        0,
        f_reg,
    );
    populate_combo(state.combo_privacy, PRIVACY_PRESETS, saved.privacy_idx);

    state.chk_yt_upload_after = create_child(
        hwnd,
        "BUTTON",
        "Subir a YouTube al terminar render",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        0,
        state.px(306),
        state.px(557),
        state.px(236),
        state.px(20),
        0,
        f_reg,
    );
    set_checked(state.chk_yt_upload_after, saved.yt_upload_after);

    state.chk_yt_playlist = create_child(
        hwnd,
        "BUTTON",
        "Crear Playlist del álbum",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        0,
        state.px(550),
        state.px(557),
        state.px(174),
        state.px(20),
        0,
        f_reg,
    );
    set_checked(state.chk_yt_playlist, saved.yt_playlist);

    state.lbl_yt_auth = create_child(
        hwnd,
        "STATIC",
        "No conectado",
        0,
        0,
        state.px(734),
        state.px(558),
        state.px(138),
        state.px(18),
        0,
        f_bold,
    );

    // Botones principales
    state.btn_start = create_child(
        hwnd,
        "BUTTON",
        "▶ Iniciar Renderizado",
        BS_DEFPUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(16),
        state.px(594),
        state.px(185),
        state.px(30),
        ID_BTN_START,
        f_bold,
    );
    state.btn_split_cue = create_child(
        hwnd,
        "BUTTON",
        "✂ Extraer MP3s del CUE",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(209),
        state.px(594),
        state.px(176),
        state.px(30),
        ID_BTN_SPLIT_CUE_MP3,
        f_reg,
    );
    state.btn_upload_only = create_child(
        hwnd,
        "BUTTON",
        "☁ Subir MP4s a YouTube",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(393),
        state.px(594),
        state.px(180),
        state.px(30),
        ID_BTN_UPLOAD_ONLY,
        f_reg,
    );
    state.btn_cancel = create_child(
        hwnd,
        "BUTTON",
        "⏹ Cancelar",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(581),
        state.px(594),
        state.px(102),
        state.px(30),
        ID_BTN_CANCEL,
        f_reg,
    );
    unsafe {
        EnableWindow(state.btn_cancel, 0);
    }

    state.btn_open_out = create_child(
        hwnd,
        "BUTTON",
        "📂 Abrir salida",
        BS_PUSHBUTTON as u32 | WS_TABSTOP,
        0,
        state.px(691),
        state.px(594),
        state.px(193),
        state.px(30),
        ID_BTN_OPEN_OUTPUT,
        f_reg,
    );

    // Progreso y log
    state.progress = create_child(
        hwnd,
        "msctls_progress32",
        "",
        0,
        0,
        state.px(16),
        state.px(632),
        state.px(868),
        state.px(16),
        0,
        f_reg,
    );
    state.lbl_status = create_child(
        hwnd,
        "STATIC",
        "Listo para renderizar, dividir CUE o buscar disco.",
        0,
        0,
        state.px(16),
        state.px(652),
        state.px(868),
        state.px(18),
        0,
        f_reg,
    );
    state.edit_log = create_child(
        hwnd,
        "EDIT",
        "",
        ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_READONLY as u32 | WS_VSCROLL | WS_BORDER,
        WS_EX_CLIENTEDGE,
        state.px(16),
        state.px(674),
        state.px(868),
        state.px(58),
        0,
        f_small,
    );
    send_msg(state.edit_log, EM_SETLIMITTEXT, 0, 0);

    // Restaurar rutas guardadas o usar valores por defecto
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let init_input = if !saved.input_path.is_empty() {
        saved.input_path.clone()
    } else {
        cwd.to_string_lossy().to_string()
    };
    let init_output = if !saved.output_path.is_empty() {
        saved.output_path.clone()
    } else {
        cwd.join("output").to_string_lossy().to_string()
    };

    set_window_text(state.edit_input, &init_input);
    set_window_text(state.edit_image, &saved.image_path);
    set_window_text(state.edit_output, &init_output);

    if !saved.client_secret_path.is_empty() {
        set_window_text(state.edit_client_secret, &saved.client_secret_path);
    } else if let Some(secret) = detect_default_client_secret() {
        set_window_text(state.edit_client_secret, &secret.to_string_lossy());
    }

    update_yt_auth_label(state);

    match find_ffmpeg(None) {
        Ok(ffmpeg) => {
            append_log(state.edit_log, &format!("FFmpeg listo: {}", ffmpeg.display()));
        }
        Err(err) => {
            append_log(state.edit_log, &format!("[AVISO] {:#}", err));
        }
    }

    let init_path = PathBuf::from(&init_input);
    if init_path.is_file() {
        try_load_cue_for_path(state, &init_path, false);
    } else if init_path.is_dir() && saved.full_album {
        try_load_directory_into_review(state, &init_path, false, false);
    }
}

fn set_running_ui(state: &mut GuiState, running: bool) {
    state.is_running = running;
    let enable_inputs: BOOL = if running { 0 } else { 1 };
    for ctrl in state.editable_controls() {
        unsafe {
            EnableWindow(ctrl, enable_inputs);
        }
    }
    unsafe {
        EnableWindow(state.btn_start, enable_inputs);
        EnableWindow(state.btn_cancel, if running { 1 } else { 0 });
    }
}

fn build_effective_album_meta(state: &GuiState) -> Option<AlbumMetadata> {
    let album_txt = get_window_text(state.edit_album).trim().to_string();
    let composers_txt = get_window_text(state.edit_composers).trim().to_string();
    let catalog_txt = get_window_text(state.edit_vgmdb_code).trim().to_string();

    if state.album_meta.is_none() && album_txt.is_empty() && composers_txt.is_empty() {
        return None;
    }

    let mut meta = state.album_meta.clone().unwrap_or(AlbumMetadata {
        source: "Manual".to_string(),
        album_title: String::new(),
        catalog: catalog_txt.clone(),
        release_date: String::new(),
        publisher: String::new(),
        composers: Vec::new(),
        arrangers: Vec::new(),
        performers: Vec::new(),
        lyricists: Vec::new(),
        tracks: Vec::new(),
    });

    if !album_txt.is_empty() {
        meta.album_title = album_txt;
    }
    if !composers_txt.is_empty() {
        let parsed: Vec<String> = composers_txt
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !parsed.is_empty() {
            meta.composers = parsed.clone();
            for tr in &mut meta.tracks {
                if tr.composers.is_empty() {
                    tr.composers = parsed.clone();
                }
            }
        }
    }
    if meta.catalog.is_empty() && !catalog_txt.is_empty() {
        meta.catalog = catalog_txt;
    }

    Some(meta)
}

fn read_upload_settings(state: &GuiState, require_yt_auth: bool) -> Result<UploadSettings> {
    let upload_after = is_checked(state.chk_yt_upload_after);
    let secret_raw = get_window_text(state.edit_client_secret).trim().to_string();
    let secret_path = if secret_raw.is_empty() {
        None
    } else {
        Some(PathBuf::from(secret_raw))
    };

    if require_yt_auth && !has_saved_tokens(&default_token_store_path()) {
        if let Some(ref path) = secret_path {
            if !path.is_file() {
                bail!("No se encontró el archivo client_secret.json en:\n{}", path.display());
            }
        }
        bail!("Tu cuenta de YouTube aún no está conectada. Presioná '🔑 Conectar YouTube' primero.");
    }

    let privacy = PRIVACY_PRESETS
        .get(combo_selected_idx(state.combo_privacy))
        .map(|(_, p)| *p)
        .unwrap_or(PrivacyStatus::Unlisted);

    let title_template = {
        let raw = get_window_text(state.edit_title_tpl).trim().to_string();
        if raw.is_empty() {
            DEFAULT_TITLE_TEMPLATE.to_string()
        } else {
            raw
        }
    };
    let desc_template = get_window_text(state.edit_desc_tpl).replace("\r\n", "\n");

    Ok(UploadSettings {
        upload_after_render: upload_after,
        create_playlist: is_checked(state.chk_yt_playlist),
        client_secret_path: secret_path,
        privacy,
        album_meta: build_effective_album_meta(state),
        title_template,
        desc_template,
    })
}

fn read_gui_config(state: &GuiState) -> Result<RenderConfig> {
    let input_raw = get_window_text(state.edit_input).trim().to_string();
    if input_raw.is_empty() {
        bail!("Indicá una carpeta de audio, un archivo de audio (.flac, .mp3, .wav, etc.) o un archivo .cue.");
    }
    let input = PathBuf::from(&input_raw);
    if !input.exists() {
        bail!("La ruta de entrada no existe:\n{}", input.display());
    }

    let image_raw = get_window_text(state.edit_image).trim().to_string();
    let image = if image_raw.is_empty() {
        None
    } else {
        let p = PathBuf::from(&image_raw);
        if !p.is_file() {
            bail!("La imagen seleccionada no existe:\n{}", p.display());
        }
        if !has_extension(&p, IMAGE_EXTENSIONS) {
            bail!(
                "La imagen '{}' no tiene una extensión soportada ({}).",
                p.display(),
                IMAGE_EXTENSIONS.join(", ")
            );
        }
        Some(p)
    };

    let output_raw = get_window_text(state.edit_output).trim().to_string();
    if output_raw.is_empty() {
        bail!("Indicá una carpeta de salida.");
    }
    let output = PathBuf::from(output_raw);

    let (width, height) = RESOLUTIONS
        .get(combo_selected_idx(state.combo_res))
        .map(|(_, dims)| *dims)
        .unwrap_or((1920, 1080));
    let fit = FIT_MODES
        .get(combo_selected_idx(state.combo_fit))
        .map(|(_, f)| *f)
        .unwrap_or(FitMode::Blur);
    let (bitrate, copy_audio) = AUDIO_PRESETS
        .get(combo_selected_idx(state.combo_audio))
        .map(|(_, a)| *a)
        .unwrap_or(("192k", false));
    let jobs = JOBS_PRESETS
        .get(combo_selected_idx(state.combo_jobs))
        .map(|(_, j)| *j)
        .unwrap_or_else(default_jobs);
    let fps = FPS_PRESETS
        .get(combo_selected_idx(state.combo_fps))
        .map(|(_, f)| *f)
        .unwrap_or(2);
    let crf = CRF_PRESETS
        .get(combo_selected_idx(state.combo_crf))
        .map(|(_, c)| *c)
        .unwrap_or(18);

    Ok(RenderConfig {
        input,
        image,
        output,
        recursive: is_checked(state.chk_recursive),
        jobs: jobs.max(1),
        width,
        height,
        fit,
        fps,
        crf,
        audio_bitrate: bitrate.to_string(),
        copy_audio,
        overwrite: is_checked(state.chk_overwrite),
        ffmpeg_path: None,
    })
}

fn parse_review_tracks(state: &GuiState, cue: &CueSheet) -> Result<Vec<CueTrack>> {
    let review_text = get_window_text(state.edit_cue_tracks);
    if review_text.trim().is_empty() {
        Ok(cue.tracks.clone())
    } else {
        parse_tracks_from_review(&review_text, &cue.tracks)
    }
}

fn active_cue_with_reviewed_tracks(
    state: &mut GuiState,
    input: &Path,
) -> Result<Option<(CueSheet, Vec<CueTrack>)>> {
    let full_album = is_checked(state.chk_full_album);

    if state.cue_sheet.is_none() {
        if input.is_file() {
            try_load_cue_for_path(state, input, true);
        } else if input.is_dir() && full_album {
            try_load_directory_into_review(state, input, true, false);
        }
    }

    let Some(cue) = state.cue_sheet.clone() else {
        return Ok(None);
    };

    if cue.cue_path.is_dir() && !full_album {
        return Ok(None);
    }

    let reviewed = parse_review_tracks(state, &cue)?;
    Ok(Some((cue, reviewed)))
}

fn start_vgmdb_search(hwnd: HWND, state: &mut GuiState) {
    if state.is_running || state.is_searching {
        return;
    }
    let code = get_window_text(state.edit_vgmdb_code).trim().to_string();
    if code.is_empty() {
        show_msg_box(
            hwnd,
            "Buscar en VGMdb",
            "Ingresá un código de catálogo (ej: SQEX-10001) o ID de álbum de VGMdb.",
            MB_OK | MB_ICONWARNING,
        );
        return;
    }

    save_settings(state);
    state.is_searching = true;
    unsafe {
        EnableWindow(state.btn_vgmdb_search, 0);
    }
    set_window_text(state.lbl_vgmdb_info, "Consultando VGMdb / MusicBrainz...");
    append_log(state.edit_log, &format!("Buscando álbum '{}'...", code));

    let hwnd_val = hwnd as usize;
    std::thread::spawn(move || {
        let hwnd = hwnd_val as HWND;
        let res = lookup_album(&code, None).map_err(|e| format!("{:#}", e));
        post_boxed(hwnd, WM_APP_VGMDB_DONE, 0, res);
    });
}

fn start_youtube_connect(hwnd: HWND, state: &mut GuiState) {
    if state.is_running || state.is_authing {
        return;
    }
    let secret_raw = get_window_text(state.edit_client_secret).trim().to_string();
    if secret_raw.is_empty() {
        show_msg_box(
            hwnd,
            "Conectar YouTube",
            "Seleccioná primero tu archivo client_secret.json (OAuth Desktop App de Google Cloud).",
            MB_OK | MB_ICONWARNING,
        );
        return;
    }
    let secret_path = PathBuf::from(secret_raw);
    if !secret_path.is_file() {
        show_msg_box(
            hwnd,
            "Archivo no encontrado",
            &format!("No existe:\n{}", secret_path.display()),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    save_settings(state);
    state.is_authing = true;
    unsafe {
        EnableWindow(state.btn_connect_yt, 0);
    }
    set_window_text(state.lbl_yt_auth, "Esperando...");
    append_log(
        state.edit_log,
        "Abriendo navegador para autorizar acceso a YouTube (OAuth 2.0)...",
    );

    let hwnd_val = hwnd as usize;
    std::thread::spawn(move || {
        let hwnd = hwnd_val as HWND;
        let token_path = default_token_store_path();
        let res = authorize_interactive(&secret_path, &token_path, None)
            .map(|_| ())
            .map_err(|e| format!("{:#}", e));
        post_boxed(hwnd, WM_APP_OAUTH_DONE, 0, res);
    });
}

fn start_render(hwnd: HWND, state: &mut GuiState) {
    if state.is_running {
        return;
    }

    let cfg = match read_gui_config(state) {
        Ok(c) => c,
        Err(err) => {
            show_msg_box(
                hwnd,
                "Datos inválidos",
                &format!("{:#}", err),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
    };

    let full_album = is_checked(state.chk_full_album);
    let cue_job = match active_cue_with_reviewed_tracks(state, &cfg.input) {
        Ok(c) => c,
        Err(err) => {
            show_msg_box(
                hwnd,
                "Error en lista de pistas",
                &format!("{:#}", err),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
    };

    if full_album && cue_job.is_none() {
        show_msg_box(
            hwnd,
            "Sin pistas para álbum completo",
            "Para renderizar 1 video completo con timestamps, elegí un archivo .cue/.flac o una carpeta con archivos MP3.",
            MB_OK | MB_ICONWARNING,
        );
        return;
    }

    let upload_after = is_checked(state.chk_yt_upload_after);
    let upload_cfg = match read_upload_settings(state, upload_after) {
        Ok(u) => u,
        Err(err) => {
            show_msg_box(
                hwnd,
                "Configuración de YouTube",
                &format!("{:#}", err),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
    };

    save_settings(state);
    state.cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&state.cancel_flag);
    set_running_ui(state, true);
    send_msg(state.progress, PBM_SETRANGE32, 0, 100);
    send_msg(state.progress, PBM_SETPOS, 0, 0);
    set_window_text(
        state.lbl_status,
        if full_album {
            "Preparando álbum completo..."
        } else {
            "Preparando lote..."
        },
    );
    set_window_text(state.edit_log, "");
    append_log(
        state.edit_log,
        &format!(
            "Iniciando: {}x{} | {:?} | {} fps | {} hilo(s){}",
            cfg.width,
            cfg.height,
            cfg.fit,
            cfg.fps,
            cfg.jobs,
            if full_album { " | MODO ÁLBUM COMPLETO" } else { "" }
        ),
    );

    let hwnd_val = hwnd as usize;
    std::thread::spawn(move || {
        let hwnd = hwnd_val as HWND;
        let summary = if let Some((cue, tracks)) = cue_job {
            if full_album {
                run_gui_full_album(hwnd, &cue, &tracks, &cfg, &upload_cfg, &cancel_flag)
            } else {
                run_gui_cue_render(hwnd, &cue, &tracks, &cfg, &upload_cfg, &cancel_flag)
            }
        } else {
            run_gui_batch(hwnd, &cfg, &upload_cfg, &cancel_flag)
        };
        post_boxed(hwnd_val as HWND, WM_APP_DONE, 0, summary);
    });
}

fn start_split_cue_mp3(hwnd: HWND, state: &mut GuiState) {
    if state.is_running {
        return;
    }

    let cfg = match read_gui_config(state) {
        Ok(c) => c,
        Err(err) => {
            show_msg_box(
                hwnd,
                "Datos inválidos",
                &format!("{:#}", err),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
    };

    let cue_job = match active_cue_with_reviewed_tracks(state, &cfg.input) {
        Ok(Some(c)) => c,
        Ok(None) => {
            show_msg_box(
                hwnd,
                "Sin archivo CUE",
                "Cargá o arrastrá un archivo .cue (o un .flac junto a su .cue) para extraer los MP3s.",
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
        Err(err) => {
            show_msg_box(
                hwnd,
                "Error en lista de pistas CUE",
                &format!("{:#}", err),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
    };

    if cue_job.0.cue_path.is_dir() || !cue_job.0.audio_path.is_file() {
        show_msg_box(
            hwnd,
            "Extraer MP3s requiere un CUE",
            "Para extraer MP3s individuales, cargá un archivo .cue (o .flac junto a su .cue).",
            MB_OK | MB_ICONWARNING,
        );
        return;
    }

    save_settings(state);
    let album_ovr = {
        let s = get_window_text(state.edit_album).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let artist_ovr = {
        let s = get_window_text(state.edit_composers).trim().to_string();
        (!s.is_empty()).then_some(s)
    };

    state.cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&state.cancel_flag);
    set_running_ui(state, true);
    send_msg(state.progress, PBM_SETRANGE32, 0, 100);
    send_msg(state.progress, PBM_SETPOS, 0, 0);
    set_window_text(state.lbl_status, "Extrayendo pistas a MP3...");
    append_log(
        state.edit_log,
        &format!("Dividiendo '{}' en {} MP3(s)...", cue_job.0.audio_path.display(), cue_job.1.len()),
    );

    let hwnd_val = hwnd as usize;
    std::thread::spawn(move || {
        let started = Instant::now();
        let summary = match find_ffmpeg(None) {
            Ok(ffmpeg) => match split_cue_to_mp3(
                &cue_job.0,
                &cue_job.1,
                &cfg.output,
                album_ovr.as_deref(),
                artist_ovr.as_deref(),
                &ffmpeg,
                cfg.jobs,
                cfg.overwrite,
                Some(&cancel_flag),
                move |line| post_boxed(hwnd_val as HWND, WM_APP_LOG, 0, line.to_string()),
                move |done, total, status| {
                    let h = hwnd_val as HWND;
                    unsafe {
                        PostMessageW(h, WM_APP_PROGRESS, done, total as LPARAM);
                    }
                    post_boxed(h, WM_APP_STATUS, 0, status.to_string());
                },
            ) {
                Ok((ok, skipped, errs)) => BatchSummary {
                    headline: format!(
                        "Extracción MP3 en {:.1}s: {} extraído(s), {} omitido(s), {} error(es).",
                        started.elapsed().as_secs_f64(),
                        ok,
                        skipped,
                        errs
                    ),
                    err_count: errs,
                    error_message: None,
                },
                Err(e) => BatchSummary {
                    headline: String::new(),
                    err_count: 1,
                    error_message: Some(format!("{:#}", e)),
                },
            },
            Err(e) => BatchSummary {
                headline: String::new(),
                err_count: 1,
                error_message: Some(format!("{:#}", e)),
            },
        };
        post_boxed(hwnd_val as HWND, WM_APP_DONE, 0, summary);
    });
}

fn start_upload_only(hwnd: HWND, state: &mut GuiState) {
    if state.is_running {
        return;
    }

    let output_raw = get_window_text(state.edit_output).trim().to_string();
    if output_raw.is_empty() {
        show_msg_box(
            hwnd,
            "Carpeta de salida vacía",
            "Indicá la carpeta donde están los archivos .mp4 renderizados.",
            MB_OK | MB_ICONWARNING,
        );
        return;
    }
    let output_dir = PathBuf::from(&output_raw);
    if !output_dir.is_dir() {
        show_msg_box(
            hwnd,
            "Carpeta inexistente",
            &format!("No existe la carpeta:\n{}", output_dir.display()),
            MB_OK | MB_ICONERROR,
        );
        return;
    }

    let upload_cfg = match read_upload_settings(state, true) {
        Ok(u) => u,
        Err(err) => {
            show_msg_box(
                hwnd,
                "Configuración de YouTube",
                &format!("{:#}", err),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }
    };

    let input_path = PathBuf::from(get_window_text(state.edit_input).trim());
    let recursive = is_checked(state.chk_recursive);
    let full_album = is_checked(state.chk_full_album);
    let cue_job = active_cue_with_reviewed_tracks(state, &input_path).ok().flatten();

    save_settings(state);
    state.cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&state.cancel_flag);
    set_running_ui(state, true);
    send_msg(state.progress, PBM_SETRANGE32, 0, 100);
    send_msg(state.progress, PBM_SETPOS, 0, 0);
    set_window_text(state.lbl_status, "Preparando subida a YouTube...");
    append_log(state.edit_log, "Buscando videos .mp4 para subir a YouTube...");

    let hwnd_val = hwnd as usize;
    std::thread::spawn(move || {
        let hwnd = hwnd_val as HWND;
        let summary = run_gui_upload_only(
            hwnd,
            &input_path,
            &output_dir,
            recursive,
            full_album,
            cue_job,
            &upload_cfg,
            &cancel_flag,
        );
        post_boxed(hwnd_val as HWND, WM_APP_DONE, 0, summary);
    });
}

fn collect_mp4_pairs_for_upload(
    input_path: &Path,
    output_dir: &Path,
    recursive: bool,
) -> Vec<(PathBuf, PathBuf)> {
    if input_path.exists() {
        if let Ok((input_base, audio_files)) =
            collect_audio_files(input_path, output_dir, recursive)
        {
            let mut pairs = Vec::new();
            for audio in audio_files {
                let mp4 = output_path_for(&audio, &input_base, output_dir, recursive);
                if mp4.is_file() {
                    pairs.push((audio, mp4));
                }
            }
            if !pairs.is_empty() {
                return pairs;
            }
        }
    }

    let max_depth = if recursive { usize::MAX } else { 1 };
    let mut mp4s = Vec::new();
    for entry in WalkDir::new(output_dir)
        .max_depth(max_depth)
        .into_iter()
        .flatten()
    {
        let p = entry.path();
        if p.is_file() && has_extension(p, &["mp4"]) {
            mp4s.push(p.to_path_buf());
        }
    }
    mp4s.sort();
    mp4s.into_iter().map(|p| (p.clone(), p)).collect()
}

fn playlist_pair_from_settings(upload_cfg: &UploadSettings) -> Option<(String, String)> {
    if !upload_cfg.create_playlist {
        return None;
    }
    upload_cfg
        .album_meta
        .as_ref()
        .filter(|a| !a.album_title.trim().is_empty())
        .map(|a| {
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

fn execute_youtube_upload_items(
    hwnd: HWND,
    items: &[crate::youtube::UploadItem],
    output_dir: &Path,
    upload_cfg: &UploadSettings,
    cancel_flag: &AtomicBool,
) -> Result<crate::youtube::UploadBatchSummary> {
    let pl_pair = playlist_pair_from_settings(upload_cfg);

    upload_videos_batch(
        items,
        output_dir,
        upload_cfg.client_secret_path.as_deref(),
        upload_cfg.privacy,
        pl_pair.as_ref().map(|(t, d)| (t.as_str(), d.as_str())),
        Some(cancel_flag),
        |line| {
            post_boxed(hwnd, WM_APP_LOG, 0, line.to_string());
        },
        |done, total, status| {
            unsafe {
                PostMessageW(hwnd, WM_APP_PROGRESS, done, total as LPARAM);
            }
            post_boxed(hwnd, WM_APP_STATUS, 0, status.to_string());
        },
    )
}

fn run_gui_upload_only(
    hwnd: HWND,
    input_path: &Path,
    output_dir: &Path,
    recursive: bool,
    full_album: bool,
    cue_job: Option<(CueSheet, Vec<CueTrack>)>,
    upload_cfg: &UploadSettings,
    cancel_flag: &AtomicBool,
) -> BatchSummary {
    let started = Instant::now();
    let items = if full_album {
        if let Some((ref cue, ref tracks)) = cue_job {
            let out_mp4 = full_album_output_path(output_dir, cue, upload_cfg.album_meta.as_ref());
            if out_mp4.is_file() {
                let ffmpeg = find_ffmpeg(None).ok();
                let plan = compute_timeline(tracks, &cue.audio_path, ffmpeg.as_deref());
                let ts_block = format_youtube_timestamps(&plan);
                vec![build_full_album_upload_item(
                    &out_mp4,
                    cue,
                    upload_cfg.album_meta.as_ref(),
                    &ts_block,
                    &upload_cfg.desc_template,
                )]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else if let Some((ref cue, ref tracks)) = cue_job {
        let cue_pairs: Vec<(CueTrack, PathBuf)> = tracks
            .iter()
            .map(|tr| {
                let out = output_dir.join(format!("{}.mp4", cue_track_output_stem(tr)));
                (tr.clone(), out)
            })
            .filter(|(_, p)| p.is_file())
            .collect();
        if cue_pairs.is_empty() {
            let fallback_pairs = collect_mp4_pairs_for_upload(input_path, output_dir, recursive);
            build_upload_items(
                &fallback_pairs,
                upload_cfg.album_meta.as_ref(),
                None,
                &upload_cfg.title_template,
                &upload_cfg.desc_template,
            )
        } else {
            build_upload_items_for_cue(
                &cue_pairs,
                cue,
                upload_cfg.album_meta.as_ref(),
                &upload_cfg.title_template,
                &upload_cfg.desc_template,
            )
        }
    } else {
        let pairs = collect_mp4_pairs_for_upload(input_path, output_dir, recursive);
        build_upload_items(
            &pairs,
            upload_cfg.album_meta.as_ref(),
            None,
            &upload_cfg.title_template,
            &upload_cfg.desc_template,
        )
    };

    if items.is_empty() {
        return BatchSummary {
            headline: String::new(),
            err_count: 1,
            error_message: Some(format!(
                "No se encontraron videos .mp4 en '{}'.",
                output_dir.display()
            )),
        };
    }

    post_boxed(
        hwnd,
        WM_APP_LOG,
        0,
        format!("Subiendo {} video(s) a YouTube...", items.len()),
    );

    match execute_youtube_upload_items(hwnd, &items, output_dir, upload_cfg, cancel_flag) {
        Ok(yt) => BatchSummary {
            headline: format!(
                "YouTube finalizado en {:.1}s: {} subido(s), {} omitido(s), {} error(es).",
                started.elapsed().as_secs_f64(),
                yt.uploaded_count,
                yt.skipped_count,
                yt.err_count
            ),
            err_count: yt.err_count,
            error_message: None,
        },
        Err(err) => BatchSummary {
            headline: String::new(),
            err_count: 1,
            error_message: Some(format!("{:#}", err)),
        },
    }
}

fn run_gui_full_album(
    hwnd: HWND,
    cue: &CueSheet,
    tracks: &[CueTrack],
    cfg: &RenderConfig,
    upload_cfg: &UploadSettings,
    cancel_flag: &AtomicBool,
) -> BatchSummary {
    let started = Instant::now();
    let ffmpeg = match find_ffmpeg(cfg.ffmpeg_path.as_deref()) {
        Ok(f) => f,
        Err(err) => {
            return BatchSummary {
                headline: String::new(),
                err_count: 1,
                error_message: Some(format!("{:#}", err)),
            };
        }
    };

    let base_dir = if cue.cue_path.is_dir() {
        cue.cue_path.as_path()
    } else {
        cue.cue_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    };
    let audio_hint = tracks
        .first()
        .and_then(|t| t.source_audio.as_deref())
        .unwrap_or(&cue.audio_path);

    let Some(image_path) = resolve_image(audio_hint, cfg.image.as_deref(), base_dir) else {
        return BatchSummary {
            headline: String::new(),
            err_count: 1,
            error_message: Some(
                "No se encontró imagen asociada (elegí una imagen o colocá cover.jpg/portada.png junto al audio)."
                    .to_string(),
            ),
        };
    };

    if let Err(err) = fs::create_dir_all(&cfg.output) {
        return BatchSummary {
            headline: String::new(),
            err_count: 1,
            error_message: Some(format!(
                "No se pudo crear la carpeta de salida: {}",
                err
            )),
        };
    }

    let out_mp4 = full_album_output_path(&cfg.output, cue, upload_cfg.album_meta.as_ref());
    let album_title = upload_cfg
        .album_meta
        .as_ref()
        .map(|a| a.album_title.as_str())
        .filter(|s| !s.trim().is_empty())
        .or(cue.album_title.as_deref());

    unsafe {
        PostMessageW(hwnd, WM_APP_PROGRESS, 0, 1);
    }

    let mut render_ok = 0usize;
    let mut skipped_count = 0usize;

    let plan = if out_mp4.exists() && !cfg.overwrite {
        skipped_count = 1;
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!("[OMITIDO] Ya existe '{}'.", out_mp4.display()),
        );
        compute_timeline(tracks, &cue.audio_path, Some(&ffmpeg))
    } else {
        post_boxed(
            hwnd,
            WM_APP_STATUS,
            0,
            format!("Renderizando álbum completo ({} pista(s))...", tracks.len()),
        );
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!(
                "Renderizando álbum completo ({} pistas) -> '{}' (imagen 100% fija)...",
                tracks.len(),
                out_mp4.display()
            ),
        );
        match render_full_album_to_mp4(
            &ffmpeg,
            tracks,
            &cue.audio_path,
            album_title,
            &image_path,
            &out_mp4,
            cfg,
        ) {
            Ok(plan) => {
                render_ok = 1;
                post_boxed(
                    hwnd,
                    WM_APP_LOG,
                    0,
                    format!("[OK] Álbum completo guardado en {}", out_mp4.display()),
                );
                plan
            }
            Err(err) => {
                return BatchSummary {
                    headline: String::new(),
                    err_count: 1,
                    error_message: Some(format!("{:#}", err)),
                };
            }
        }
    };

    unsafe {
        PostMessageW(hwnd, WM_APP_PROGRESS, 1, 1);
    }

    let ts_block = format_youtube_timestamps(&plan);
    post_boxed(
        hwnd,
        WM_APP_LOG,
        0,
        "[TIMESTAMPS PARA YOUTUBE]:".to_string(),
    );
    for line in ts_block.lines() {
        post_boxed(hwnd, WM_APP_LOG, 0, line.to_string());
    }

    let full_album_item = build_full_album_upload_item(
        &out_mp4,
        cue,
        upload_cfg.album_meta.as_ref(),
        &ts_block,
        &upload_cfg.desc_template,
    );
    if let Ok(sidecar) = write_video_sidecar_txt(&full_album_item) {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!("[TXT] Guardado: {}", sidecar.display()),
        );
    }
    if let Ok(master) =
        write_master_tracklist_txt(&cfg.output, std::slice::from_ref(&full_album_item))
    {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!("[TXT] Lista guardada: {}", master.display()),
        );
    }

    let mut total_errs = 0usize;
    let mut yt_note = String::new();
    if upload_cfg.upload_after_render && !cancel_flag.load(Ordering::Relaxed) {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!(
                "Subiendo álbum completo '{}' a YouTube...",
                full_album_item.title
            ),
        );
        match execute_youtube_upload_items(
            hwnd,
            &[full_album_item],
            &cfg.output,
            upload_cfg,
            cancel_flag,
        ) {
            Ok(yt) => {
                total_errs += yt.err_count;
                yt_note = format!(
                    " | YouTube: {} subido(s), {} omitido(s), {} error(es)",
                    yt.uploaded_count, yt.skipped_count, yt.err_count
                );
            }
            Err(err) => {
                total_errs += 1;
                post_boxed(
                    hwnd,
                    WM_APP_LOG,
                    0,
                    format!("[YOUTUBE ERROR] {:#}", err),
                );
                yt_note = " | Error en subida a YouTube".to_string();
            }
        }
    }

    BatchSummary {
        headline: format!(
            "Álbum completo finalizado en {:.1}s: {} renderizado(s), {} omitido(s), {} capítulo(s){}.",
            started.elapsed().as_secs_f64(),
            render_ok,
            skipped_count,
            plan.chapters.len(),
            yt_note
        ),
        err_count: total_errs,
        error_message: None,
    }
}

fn run_gui_cue_render(
    hwnd: HWND,
    cue: &CueSheet,
    tracks: &[CueTrack],
    cfg: &RenderConfig,
    upload_cfg: &UploadSettings,
    cancel_flag: &AtomicBool,
) -> BatchSummary {
    let started = Instant::now();
    let ffmpeg = match find_ffmpeg(cfg.ffmpeg_path.as_deref()) {
        Ok(f) => f,
        Err(err) => {
            return BatchSummary {
                headline: String::new(),
                err_count: 1,
                error_message: Some(format!("{:#}", err)),
            };
        }
    };

    let cue_dir = cue
        .cue_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let Some(image_path) = resolve_image(&cue.audio_path, cfg.image.as_deref(), cue_dir) else {
        return BatchSummary {
            headline: String::new(),
            err_count: 1,
            error_message: Some(
                "No se encontró imagen asociada (elegí una imagen o colocá cover.jpg/portada.png junto al FLAC/CUE)."
                    .to_string(),
            ),
        };
    };

    if let Err(err) = fs::create_dir_all(&cfg.output) {
        return BatchSummary {
            headline: String::new(),
            err_count: 1,
            error_message: Some(format!(
                "No se pudo crear la carpeta de salida: {}",
                err
            )),
        };
    }

    let mut all_pairs = Vec::with_capacity(tracks.len());
    let mut pending = Vec::new();
    let mut skipped_count = 0usize;

    for tr in tracks {
        let out_mp4 = cfg.output.join(format!("{}.mp4", cue_track_output_stem(tr)));
        all_pairs.push((tr.clone(), out_mp4.clone()));
        if out_mp4.exists() && !cfg.overwrite {
            skipped_count += 1;
            continue;
        }
        pending.push((tr.clone(), out_mp4));
    }

    if skipped_count > 0 {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!(
                "[OMITIDOS] {} video(s) ya existentes en la carpeta de salida.",
                skipped_count
            ),
        );
    }

    let total_pending = pending.len();
    let ok_count = AtomicUsize::new(0);
    let err_count = AtomicUsize::new(0);

    if total_pending > 0 {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!(
                "Renderizando {} pista(s) desde CUE+FLAC '{}'...",
                total_pending,
                cue.audio_path.display()
            ),
        );
        unsafe {
            PostMessageW(hwnd, WM_APP_PROGRESS, 0, total_pending as LPARAM);
        }

        let pool = match rayon::ThreadPoolBuilder::new().num_threads(cfg.jobs).build() {
            Ok(p) => p,
            Err(err) => {
                return BatchSummary {
                    headline: String::new(),
                    err_count: 1,
                    error_message: Some(format!("Error iniciando hilos: {}", err)),
                };
            }
        };

        let done_count = AtomicUsize::new(0);
        let hwnd_val = hwnd as usize;

        pool.install(|| {
            pending.par_iter().for_each(|(tr, out_mp4)| {
                let hwnd = hwnd_val as HWND;
                if cancel_flag.load(Ordering::Relaxed) {
                    return;
                }

                let label = format!("{:02} - {}", tr.track_number, tr.title);
                post_boxed(hwnd, WM_APP_STATUS, 0, format!("Renderizando: {}", label));

                match render_cue_track_to_mp4(
                    &ffmpeg,
                    tr.source_audio.as_deref().unwrap_or(&cue.audio_path),
                    tr,
                    &image_path,
                    out_mp4,
                    cfg,
                ) {
                    Ok(()) => {
                        ok_count.fetch_add(1, Ordering::Relaxed);
                        post_boxed(hwnd, WM_APP_LOG, 0, format!("[OK] {}.mp4", label));
                    }
                    Err(err) => {
                        err_count.fetch_add(1, Ordering::Relaxed);
                        post_boxed(
                            hwnd,
                            WM_APP_LOG,
                            0,
                            format!("[ERROR] {}: {:#}", label, err),
                        );
                    }
                }

                let done = done_count.fetch_add(1, Ordering::Relaxed) + 1;
                unsafe {
                    PostMessageW(hwnd, WM_APP_PROGRESS, done, total_pending as LPARAM);
                }
            });
        });
    }

    let render_ok = ok_count.load(Ordering::Relaxed);
    let mut total_errs = err_count.load(Ordering::Relaxed);
    if cancel_flag.load(Ordering::Relaxed) {
        return BatchSummary {
            headline: format!(
                "Cancelado en {:.1}s: {} renderizado(s), {} omitido(s), {} error(es).",
                started.elapsed().as_secs_f64(),
                render_ok,
                skipped_count,
                total_errs
            ),
            err_count: total_errs,
            error_message: None,
        };
    }

    let upload_items = build_upload_items_for_cue(
        &all_pairs,
        cue,
        upload_cfg.album_meta.as_ref(),
        &upload_cfg.title_template,
        &upload_cfg.desc_template,
    );
    for item in &upload_items {
        let _ = write_video_sidecar_txt(item);
    }
    if let Ok(master) = write_master_tracklist_txt(&cfg.output, &upload_items) {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!("[TXT] Guardados archivos .txt y lista: {}", master.display()),
        );
    }

    let mut yt_note = String::new();
    if upload_cfg.upload_after_render && !upload_items.is_empty() {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!("Subiendo {} video(s) a YouTube...", upload_items.len()),
        );
        match execute_youtube_upload_items(
            hwnd,
            &upload_items,
            &cfg.output,
            upload_cfg,
            cancel_flag,
        ) {
                Ok(yt) => {
                    total_errs += yt.err_count;
                    yt_note = format!(
                        " | YouTube: {} subido(s), {} omitido(s), {} error(es)",
                        yt.uploaded_count, yt.skipped_count, yt.err_count
                    );
                }
                Err(err) => {
                    total_errs += 1;
                    post_boxed(
                        hwnd,
                        WM_APP_LOG,
                        0,
                        format!("[YOUTUBE ERROR] {:#}", err),
                    );
                    yt_note = " | Error en subida a YouTube".to_string();
                }
            }
        }

    BatchSummary {
        headline: format!(
            "Finalizado en {:.1}s: {} renderizado(s), {} omitido(s), {} error(es){}.",
            started.elapsed().as_secs_f64(),
            render_ok,
            skipped_count,
            total_errs,
            yt_note
        ),
        err_count: total_errs,
        error_message: None,
    }
}

fn run_gui_batch(
    hwnd: HWND,
    cfg: &RenderConfig,
    upload_cfg: &UploadSettings,
    cancel_flag: &AtomicBool,
) -> BatchSummary {
    let started = Instant::now();
    let ffmpeg = match find_ffmpeg(cfg.ffmpeg_path.as_deref()) {
        Ok(f) => f,
        Err(err) => {
            return BatchSummary {
                headline: String::new(),
                err_count: 1,
                error_message: Some(format!("{:#}", err)),
            };
        }
    };

    let (input_base, audio_files) = match collect_audio_files(&cfg.input, &cfg.output, cfg.recursive) {
        Ok(res) => res,
        Err(err) => {
            return BatchSummary {
                headline: String::new(),
                err_count: 1,
                error_message: Some(format!("{:#}", err)),
            };
        }
    };

    if audio_files.is_empty() {
        return BatchSummary {
            headline: String::new(),
            err_count: 0,
            error_message: Some(format!(
                "No se encontraron archivos de audio (.flac, .mp3, etc.) en '{}'.",
                cfg.input.display()
            )),
        };
    }

    if let Err(err) = fs::create_dir_all(&cfg.output).with_context(|| {
        format!(
            "No se pudo crear la carpeta de salida: {}",
            cfg.output.display()
        )
    }) {
        return BatchSummary {
            headline: String::new(),
            err_count: 1,
            error_message: Some(format!("{:#}", err)),
        };
    }

    let mut all_pairs = Vec::with_capacity(audio_files.len());
    let mut pending_jobs = Vec::new();
    let mut skipped_count = 0usize;
    for audio_path in audio_files {
        let output_path = output_path_for(&audio_path, &input_base, &cfg.output, cfg.recursive);
        all_pairs.push((audio_path.clone(), output_path.clone()));
        if output_path.exists() && !cfg.overwrite {
            skipped_count += 1;
            continue;
        }
        pending_jobs.push(RenderJob {
            mp3_path: audio_path,
            output_path,
        });
    }

    if skipped_count > 0 {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!(
                "[OMITIDOS] {} video(s) ya existentes en la carpeta de salida.",
                skipped_count
            ),
        );
    }

    let total_pending = pending_jobs.len();
    let ok_count = AtomicUsize::new(0);
    let err_count = AtomicUsize::new(0);

    if total_pending > 0 {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!(
                "Renderizando {} video(s) con {}...",
                total_pending,
                ffmpeg.display()
            ),
        );
        unsafe {
            PostMessageW(hwnd, WM_APP_PROGRESS, 0, total_pending as LPARAM);
        }

        let pool = match rayon::ThreadPoolBuilder::new().num_threads(cfg.jobs).build() {
            Ok(p) => p,
            Err(err) => {
                return BatchSummary {
                    headline: String::new(),
                    err_count: 1,
                    error_message: Some(format!("Error iniciando hilos: {}", err)),
                };
            }
        };

        let done_count = AtomicUsize::new(0);
        let hwnd_val = hwnd as usize;

        pool.install(|| {
            pending_jobs.par_iter().for_each(|job| {
                let hwnd = hwnd_val as HWND;
                if cancel_flag.load(Ordering::Relaxed) {
                    return;
                }

                let song_name = job
                    .mp3_path
                    .file_name()
                    .unwrap_or_else(|| OsStr::new("?"))
                    .to_string_lossy()
                    .to_string();

                post_boxed(
                    hwnd,
                    WM_APP_STATUS,
                    0,
                    format!("Renderizando: {}", song_name),
                );

                let result = match resolve_image(&job.mp3_path, cfg.image.as_deref(), &input_base) {
                    Some(image_path) => render_single(&ffmpeg, job, &image_path, cfg),
                    None => Err(anyhow::anyhow!(
                        "Sin imagen asociada (elegí una imagen o colocá una portada en la carpeta)"
                    )),
                };

                match result {
                    Ok(()) => {
                        ok_count.fetch_add(1, Ordering::Relaxed);
                        let out_name = job
                            .output_path
                            .file_name()
                            .unwrap_or_else(|| OsStr::new("?"))
                            .to_string_lossy();
                        post_boxed(
                            hwnd,
                            WM_APP_LOG,
                            0,
                            format!("[OK] {} -> {}", song_name, out_name),
                        );
                    }
                    Err(err) => {
                        err_count.fetch_add(1, Ordering::Relaxed);
                        post_boxed(
                            hwnd,
                            WM_APP_LOG,
                            0,
                            format!("[ERROR] {}: {:#}", song_name, err),
                        );
                    }
                }

                let done = done_count.fetch_add(1, Ordering::Relaxed) + 1;
                unsafe {
                    PostMessageW(hwnd, WM_APP_PROGRESS, done, total_pending as LPARAM);
                }
            });
        });
    }

    let render_ok = ok_count.load(Ordering::Relaxed);
    let mut total_errs = err_count.load(Ordering::Relaxed);
    let cancelled = cancel_flag.load(Ordering::Relaxed);

    if cancelled {
        return BatchSummary {
            headline: format!(
                "Cancelado en {:.1}s: {} renderizado(s), {} omitido(s), {} error(es).",
                started.elapsed().as_secs_f64(),
                render_ok,
                skipped_count,
                total_errs
            ),
            err_count: total_errs,
            error_message: None,
        };
    }

    let upload_items = build_upload_items(
        &all_pairs,
        upload_cfg.album_meta.as_ref(),
        Some(&ffmpeg),
        &upload_cfg.title_template,
        &upload_cfg.desc_template,
    );
    for item in &upload_items {
        let _ = write_video_sidecar_txt(item);
    }
    if let Ok(master) = write_master_tracklist_txt(&cfg.output, &upload_items) {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!("[TXT] Guardados archivos .txt y lista: {}", master.display()),
        );
    }

    let mut yt_note = String::new();
    if upload_cfg.upload_after_render && !upload_items.is_empty() {
        post_boxed(
            hwnd,
            WM_APP_LOG,
            0,
            format!("Subiendo {} video(s) a YouTube...", upload_items.len()),
        );
        match execute_youtube_upload_items(
            hwnd,
            &upload_items,
            &cfg.output,
            upload_cfg,
            cancel_flag,
        ) {
                Ok(yt) => {
                    total_errs += yt.err_count;
                    yt_note = format!(
                        " | YouTube: {} subido(s), {} omitido(s), {} error(es)",
                        yt.uploaded_count, yt.skipped_count, yt.err_count
                    );
                }
                Err(err) => {
                    total_errs += 1;
                    post_boxed(
                        hwnd,
                        WM_APP_LOG,
                        0,
                        format!("[YOUTUBE ERROR] {:#}", err),
                    );
                    yt_note = " | Error en subida a YouTube".to_string();
                }
            }
        }

    BatchSummary {
        headline: format!(
            "Finalizado en {:.1}s: {} renderizado(s), {} omitido(s), {} error(es){}.",
            started.elapsed().as_secs_f64(),
            render_ok,
            skipped_count,
            total_errs,
            yt_note
        ),
        err_count: total_errs,
        error_message: None,
    }
}

fn handle_drop_files(hwnd: HWND, state: &mut GuiState, hdrop: HDROP) {
    if state.is_running {
        unsafe {
            DragFinish(hdrop);
        }
        return;
    }

    let len = unsafe { DragQueryFileW(hdrop, 0, null_mut(), 0) };
    if len > 0 {
        let mut buf = vec![0u16; (len + 1) as usize];
        unsafe {
            DragQueryFileW(hdrop, 0, buf.as_mut_ptr(), buf.len() as u32);
        }
        let dropped = from_wide_slice(&buf);
        let path = PathBuf::from(&dropped);

        if path.is_dir()
            || has_extension(&path, &["cue"])
            || has_extension(&path, AUDIO_EXTENSIONS)
        {
            set_window_text(state.edit_input, &dropped);
            if get_window_text(state.edit_output).trim().is_empty() {
                let base = if path.is_dir() {
                    path.clone()
                } else {
                    path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf()
                };
                set_window_text(state.edit_output, &base.join("output").to_string_lossy());
            }
            if path.is_file() {
                try_load_cue_for_path(state, &path, true);
            } else if path.is_dir() && is_checked(state.chk_full_album) {
                try_load_directory_into_review(state, &path, true, true);
            } else {
                clear_cue_ui(state);
            }
            append_log(state.edit_log, &format!("Entrada seleccionada: {}", dropped));
            save_settings(state);
        } else if path.is_file() && has_extension(&path, IMAGE_EXTENSIONS) {
            set_window_text(state.edit_image, &dropped);
            append_log(state.edit_log, &format!("Imagen seleccionada: {}", dropped));
            save_settings(state);
        } else if path.is_file() && has_extension(&path, &["json"]) {
            let content = fs::read_to_string(&path).unwrap_or_default();
            if content.contains("\"installed\"") || content.contains("\"web\"") {
                set_window_text(state.edit_client_secret, &dropped);
                append_log(
                    state.edit_log,
                    &format!("client_secret.json seleccionado: {}", dropped),
                );
                save_settings(state);
            } else {
                set_window_text(state.edit_vgmdb_code, &dropped);
                start_vgmdb_search(hwnd, state);
            }
        } else {
            show_msg_box(
                hwnd,
                "Archivo no compatible",
                "Soltá una carpeta, .mp3, .cue, .flac, imagen o archivo .json.",
                MB_OK | MB_ICONWARNING,
            );
        }
    }

    unsafe {
        DragFinish(hdrop);
    }
}

fn handle_command(hwnd: HWND, state: &mut GuiState, cmd_id: usize) {
    match cmd_id {
        ID_BTN_BROWSE_INPUT_DIR => {
            let current = get_window_text(state.edit_input);
            if let Some(folder) = pick_folder(hwnd, "Elegí la carpeta con los archivos MP3", &current)
            {
                set_window_text(state.edit_input, &folder.to_string_lossy());
                if get_window_text(state.edit_output).trim().is_empty() {
                    set_window_text(state.edit_output, &folder.join("output").to_string_lossy());
                }
                if is_checked(state.chk_full_album) {
                    try_load_directory_into_review(state, &folder, true, true);
                } else {
                    clear_cue_ui(state);
                }
                save_settings(state);
            }
        }
        ID_BTN_BROWSE_INPUT_FILE => {
            let current = get_window_text(state.edit_input);
            if let Some(file) = pick_file(
                hwnd,
                "Elegí un archivo de Audio o CUE",
                &[
                    (
                        "Audio / CUE (*.flac;*.mp3;*.cue;*.wav;*.m4a;*.ogg;*.opus;*.ape;*.wv)",
                        "*.flac;*.mp3;*.cue;*.wav;*.m4a;*.ogg;*.opus;*.ape;*.wv",
                    ),
                    ("Archivos FLAC (*.flac)", "*.flac"),
                    ("Archivos MP3 (*.mp3)", "*.mp3"),
                    ("Hojas CUE (*.cue)", "*.cue"),
                    ("Todos los archivos (*.*)", "*.*"),
                ],
                &current,
            ) {
                set_window_text(state.edit_input, &file.to_string_lossy());
                if has_extension(&file, &["cue"]) || has_extension(&file, AUDIO_EXTENSIONS) {
                    try_load_cue_for_path(state, &file, true);
                } else {
                    clear_cue_ui(state);
                }
                save_settings(state);
            }
        }
        ID_BTN_BROWSE_IMAGE => {
            let current = get_window_text(state.edit_image);
            let fallback = if current.trim().is_empty() {
                get_window_text(state.edit_input)
            } else {
                current
            };
            if let Some(img) = pick_file(
                hwnd,
                "Elegí la imagen estática",
                &[
                    (
                        "Imágenes (*.png;*.jpg;*.jpeg;*.webp;*.bmp)",
                        "*.png;*.jpg;*.jpeg;*.webp;*.bmp",
                    ),
                    ("Todos los archivos (*.*)", "*.*"),
                ],
                &fallback,
            ) {
                set_window_text(state.edit_image, &img.to_string_lossy());
                save_settings(state);
            }
        }
        ID_BTN_CLEAR_IMAGE => {
            set_window_text(state.edit_image, "");
            save_settings(state);
        }
        ID_BTN_BROWSE_OUTPUT => {
            let current = get_window_text(state.edit_output);
            if let Some(folder) =
                pick_folder(hwnd, "Elegí la carpeta de salida para los MP4 / MP3", &current)
            {
                set_window_text(state.edit_output, &folder.to_string_lossy());
                save_settings(state);
            }
        }
        ID_BTN_VGMDB_SEARCH => start_vgmdb_search(hwnd, state),
        ID_BTN_BROWSE_SECRET => {
            let current = get_window_text(state.edit_client_secret);
            if let Some(file) = pick_file(
                hwnd,
                "Elegí el archivo client_secret.json",
                &[("Archivo JSON (*.json)", "*.json"), ("Todos (*.*)", "*.*")],
                &current,
            ) {
                set_window_text(state.edit_client_secret, &file.to_string_lossy());
                save_settings(state);
            }
        }
        ID_BTN_CONNECT_YT => start_youtube_connect(hwnd, state),
        ID_BTN_START => start_render(hwnd, state),
        ID_BTN_SPLIT_CUE_MP3 => start_split_cue_mp3(hwnd, state),
        ID_CHK_FULL_ALBUM => {
            let input_path = PathBuf::from(get_window_text(state.edit_input).trim());
            if is_checked(state.chk_full_album) {
                if input_path.is_file()
                    && (has_extension(&input_path, &["cue"])
                        || has_extension(&input_path, AUDIO_EXTENSIONS))
                {
                    try_load_cue_for_path(state, &input_path, true);
                } else if input_path.is_dir() && state.cue_sheet.is_none() {
                    try_load_directory_into_review(state, &input_path, true, true);
                }
            } else if state
                .cue_sheet
                .as_ref()
                .is_some_and(|c| c.cue_path.is_dir() || c.cue_path == input_path)
            {
                clear_cue_ui(state);
            }
            save_settings(state);
        }
        ID_BTN_COPY_TIMESTAMPS => {
            if state.is_running {
                return;
            }
            let input_path = PathBuf::from(get_window_text(state.edit_input).trim());
            if state.cue_sheet.is_none() {
                if input_path.is_file() {
                    try_load_cue_for_path(state, &input_path, true);
                } else if input_path.is_dir() {
                    try_load_directory_into_review(state, &input_path, true, true);
                }
            }

            let Some(cue) = state.cue_sheet.clone() else {
                show_msg_box(
                    hwnd,
                    "Sin pistas para timestamps",
                    "Cargá un .cue/.flac o elegí una carpeta con archivos MP3 para generar los timestamps.",
                    MB_OK | MB_ICONWARNING,
                );
                return;
            };

            let tracks = match parse_review_tracks(state, &cue) {
                Ok(t) => t,
                Err(err) => {
                    show_msg_box(
                        hwnd,
                        "Error en lista de pistas",
                        &format!("{:#}", err),
                        MB_OK | MB_ICONWARNING,
                    );
                    return;
                }
            };

            let ffmpeg = find_ffmpeg(None).ok();
            let plan = compute_timeline(&tracks, &cue.audio_path, ffmpeg.as_deref());
            let ts_block = format_youtube_timestamps(&plan);
            if ts_block.trim().is_empty() {
                show_msg_box(
                    hwnd,
                    "Sin timestamps",
                    "No se pudieron calcular los timestamps.",
                    MB_OK | MB_ICONWARNING,
                );
                return;
            }

            let clip_text = ts_block.replace('\n', "\r\n");
            match copy_text_to_clipboard(hwnd, &clip_text) {
                Ok(()) => {
                    append_log(
                        state.edit_log,
                        &format!("[TIMESTAMPS COPIADOS] {} capítulo(s):", plan.chapters.len()),
                    );
                    for line in ts_block.lines() {
                        append_log(state.edit_log, &format!("  {}", line));
                    }
                    set_window_text(
                        state.lbl_status,
                        &format!(
                            "Timestamps copiados al portapapeles ({} capítulos).",
                            plan.chapters.len()
                        ),
                    );
                    show_msg_box(
                        hwnd,
                        "Timestamps copiados",
                        &format!(
                            "Se copiaron {} timestamps al portapapeles.\nPodés pegarlos en la descripción de YouTube con Ctrl+V (también quedaron visibles en el log).",
                            plan.chapters.len()
                        ),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                Err(err) => {
                    show_msg_box(
                        hwnd,
                        "Error al copiar",
                        &format!("{:#}", err),
                        MB_OK | MB_ICONERROR,
                    );
                }
            }
        }
        ID_BTN_UPLOAD_ONLY => start_upload_only(hwnd, state),
        ID_BTN_CANCEL => {
            if state.is_running {
                state.cancel_flag.store(true, Ordering::Relaxed);
                unsafe {
                    EnableWindow(state.btn_cancel, 0);
                }
                set_window_text(
                    state.lbl_status,
                    "Cancelando... esperando que finalicen los procesos actuales.",
                );
                append_log(
                    state.edit_log,
                    "[CANCELANDO] Deteniendo nuevos procesos en la cola...",
                );
            }
        }
        ID_BTN_OPEN_OUTPUT => {
            let out_raw = get_window_text(state.edit_output).trim().to_string();
            if out_raw.is_empty() {
                return;
            }
            let out_path = PathBuf::from(&out_raw);
            if !out_path.exists() {
                let _ = fs::create_dir_all(&out_path);
            }
            let target_w = to_wide(&out_path.to_string_lossy());
            let open_w = to_wide("open");
            unsafe {
                ShellExecuteW(
                    hwnd,
                    open_w.as_ptr(),
                    target_w.as_ptr(),
                    null(),
                    null(),
                    SW_SHOWNORMAL,
                );
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CREATE {
        let cs = &*(lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
        let state_ptr = cs.lpCreateParams as *mut GuiState;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
        build_controls(hwnd, &mut *state_ptr);
        DragAcceptFiles(hwnd, 1);
        return 0;
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GuiState;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &mut *state_ptr;

    match msg {
        WM_COMMAND => {
            let cmd_id = wparam & 0xFFFF;
            handle_command(hwnd, state, cmd_id);
            0
        }
        WM_DROPFILES => {
            handle_drop_files(hwnd, state, wparam as HDROP);
            0
        }
        WM_CTLCOLORSTATIC => {
            let hdc = wparam as windows_sys::Win32::Graphics::Gdi::HDC;
            let ctrl = lparam as HWND;
            if ctrl == state.edit_log || ctrl == state.edit_cue_tracks {
                SetBkColor(hdc, 0x00FF_FFFF);
                return GetStockObject(WHITE_BRUSH) as LRESULT;
            }
            SetBkMode(hdc, TRANSPARENT as i32);
            (COLOR_BTNFACE + 1) as isize as LRESULT
        }
        WM_APP_LOG => {
            if lparam != 0 {
                let text = Box::from_raw(lparam as *mut String);
                append_log(state.edit_log, &text);
            }
            0
        }
        WM_APP_STATUS => {
            if lparam != 0 {
                let text = Box::from_raw(lparam as *mut String);
                set_window_text(state.lbl_status, &text);
            }
            0
        }
        WM_APP_PROGRESS => {
            let completed = wparam as usize;
            let total = (lparam as usize).max(1);
            let pct = (completed * 100) / total;
            send_msg(state.progress, PBM_SETRANGE32, 0, total as LPARAM);
            send_msg(state.progress, PBM_SETPOS, completed, 0);
            if !state.cancel_flag.load(Ordering::Relaxed) {
                set_window_text(
                    state.lbl_status,
                    &format!("Progreso: {} / {} ({}%)", completed, total, pct),
                );
            }
            0
        }
        WM_APP_VGMDB_DONE => {
            state.is_searching = false;
            if !state.is_running {
                EnableWindow(state.btn_vgmdb_search, 1);
            }
            if lparam != 0 {
                let res = Box::from_raw(lparam as *mut std::result::Result<AlbumMetadata, String>);
                match *res {
                    Ok(album) => {
                        let comp_str = album.composers.join(", ");
                        set_window_text(state.edit_album, &album.album_title);
                        set_window_text(state.edit_composers, &comp_str);
                        set_window_text(
                            state.lbl_vgmdb_info,
                            &format!("[{}] {} pistas cargadas", album.source, album.tracks.len()),
                        );

                        // Si hay un CUE cargado, actualizar títulos de pistas con los del álbum encontrado
                        if let Some(ref mut cue) = state.cue_sheet {
                            for (idx, cue_tr) in cue.tracks.iter_mut().enumerate() {
                                let found = album
                                    .tracks
                                    .iter()
                                    .find(|t| t.track_number == cue_tr.track_number)
                                    .or_else(|| album.tracks.get(idx));
                                if let Some(m) = found {
                                    cue_tr.title = m.title.clone();
                                }
                            }
                            set_window_text(
                                state.edit_cue_tracks,
                                &format_tracks_for_review(&cue.tracks),
                            );
                        }

                        append_log(
                            state.edit_log,
                            &format!(
                                "[{}] Album: '{}' | Composer(s): {} | {} tracks",
                                album.source,
                                album.album_title,
                                if comp_str.is_empty() {
                                    "N/A"
                                } else {
                                    &comp_str
                                },
                                album.tracks.len()
                            ),
                        );
                        state.album_meta = Some(album);
                        save_settings(state);
                    }
                    Err(err_msg) => {
                        set_window_text(state.lbl_vgmdb_info, "No se encontró el disco.");
                        append_log(state.edit_log, &format!("[BUSQUEDA ERROR] {}", err_msg));
                        show_msg_box(hwnd, "Búsqueda de disco", &err_msg, MB_OK | MB_ICONWARNING);
                    }
                }
            }
            0
        }
        WM_APP_OAUTH_DONE => {
            state.is_authing = false;
            if !state.is_running {
                EnableWindow(state.btn_connect_yt, 1);
            }
            update_yt_auth_label(state);
            if lparam != 0 {
                let res = Box::from_raw(lparam as *mut std::result::Result<(), String>);
                match *res {
                    Ok(()) => {
                        save_settings(state);
                        append_log(
                            state.edit_log,
                            "[YOUTUBE] Cuenta conectada y tokens guardados correctamente.",
                        );
                        show_msg_box(
                            hwnd,
                            "YouTube conectado",
                            "La cuenta de YouTube quedó conectada correctamente.",
                            MB_OK | MB_ICONINFORMATION,
                        );
                    }
                    Err(err_msg) => {
                        append_log(state.edit_log, &format!("[YOUTUBE AUTH ERROR] {}", err_msg));
                        show_msg_box(
                            hwnd,
                            "Error de conexión YouTube",
                            &err_msg,
                            MB_OK | MB_ICONERROR,
                        );
                    }
                }
            }
            0
        }
        WM_APP_DONE => {
            if lparam != 0 {
                let summary = Box::from_raw(lparam as *mut BatchSummary);
                set_running_ui(state, false);

                if let Some(ref err_msg) = summary.error_message {
                    set_window_text(state.lbl_status, "Error en el proceso.");
                    append_log(state.edit_log, &format!("[ERROR] {}", err_msg));
                    show_msg_box(hwnd, "Error", err_msg, MB_OK | MB_ICONERROR);
                    return 0;
                }

                set_window_text(state.lbl_status, &summary.headline);
                append_log(state.edit_log, &summary.headline);

                let icon = if summary.err_count > 0 {
                    MB_ICONWARNING
                } else {
                    MB_ICONINFORMATION
                };
                show_msg_box(hwnd, "MP3 a Video", &summary.headline, MB_OK | icon);
            }
            0
        }
        WM_CLOSE => {
            if state.is_running {
                let ans = show_msg_box(
                    hwnd,
                    "Proceso en curso",
                    "Hay un proceso en ejecución en este momento. ¿Querés cancelar y cerrar la ventana?",
                    MB_YESNO | MB_ICONQUESTION,
                );
                if ans != IDYES {
                    return 0;
                }
                state.cancel_flag.store(true, Ordering::Relaxed);
            }
            save_settings(state);
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            if !state.font_regular.is_null() {
                DeleteObject(state.font_regular as HGDIOBJ);
            }
            if !state.font_bold.is_null() {
                DeleteObject(state.font_bold as HGDIOBJ);
            }
            if !state.font_small.is_null() {
                DeleteObject(state.font_small as HGDIOBJ);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(state_ptr));
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(crate) fn run_gui() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
    }
    enable_visual_styles();

    let dpi = unsafe {
        let hdc = GetDC(null_mut());
        let d = if !hdc.is_null() {
            let val = GetDeviceCaps(hdc, LOGPIXELSX as i32);
            ReleaseDC(null_mut(), hdc);
            if val > 0 {
                val
            } else {
                96
            }
        } else {
            96
        };
        d
    };

    let mut state = Box::new(GuiState::new(dpi));
    state.font_regular = create_font(dpi, 9, 400);
    state.font_bold = create_font(dpi, 10, 600);
    state.font_small = create_font(dpi, 8, 400);

    let hinst = unsafe { GetModuleHandleW(null()) };
    let class_name = to_wide("Mp3VideoMainWindowClass");
    let title = to_wide("MP3 / CUE+FLAC a Video - VGMdb & YouTube (FFmpeg)");

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: unsafe { LoadIconW(null_mut(), IDI_APPLICATION) },
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: (COLOR_BTNFACE + 1) as isize as HBRUSH,
        lpszMenuName: null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: unsafe { LoadIconW(null_mut(), IDI_APPLICATION) },
    };

    if unsafe { RegisterClassExW(&wc) } == 0 {
        unsafe {
            CoUninitialize();
        }
        bail!("No se pudo registrar la clase de ventana Win32.");
    }

    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: state.px(900),
        bottom: state.px(740),
    };
    unsafe {
        AdjustWindowRectEx(&mut rect, style, 0, 0);
    }
    let win_w = rect.right - rect.left;
    let win_h = rect.bottom - rect.top;

    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let pos_x = ((screen_w - win_w) / 2).max(0);
    let pos_y = ((screen_h - win_h) / 2).max(0);

    let state_raw = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            pos_x,
            pos_y,
            win_w,
            win_h,
            null_mut(),
            null_mut(),
            hinst,
            state_raw as *const _,
        )
    };

    if hwnd.is_null() {
        unsafe {
            drop(Box::from_raw(state_raw));
            CoUninitialize();
        }
        bail!("No se pudo crear la ventana principal.");
    }

    unsafe {
        ShowWindow(hwnd, SW_SHOWNORMAL);
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        if unsafe { IsDialogMessageW(hwnd, &msg) } == 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe {
        CoUninitialize();
    }
    Ok(())
}
