//! Win32 file dialogs and attachment I/O.
//!
//! WinUI 3 does not expose the common file pickers through the filtered
//! projection, and the WinRT pickers need an `HWND` interop object. The classic
//! `GetOpenFileNameW` / `GetSaveFileNameW` dialogs are enough for choosing
//! attachments and a save location, and they run synchronously on the UI thread
//! just like the GTK file dialogs.

use std::path::{Path, PathBuf};

use mailin_core::ComposeAttachment;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_ALLOWMULTISELECT, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Shell::{
    BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, ILFree, SHBrowseForFolderW,
    SHGetPathFromIDListW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Maximum size of a single attached file. The same limit is enforced by the
/// GTK client and keeps the SQLite outbox and SMTP submission sane.
pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// Show the common open dialog. Returns an empty list when the user cancels.
pub fn pick_files(title: &str) -> Vec<PathBuf> {
    let mut buffer = vec![0u16; 65_536];
    let filter = wide("All files\0*.*\0\0");
    let title = wide(title);

    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.lpstrFile = buffer.as_mut_ptr();
    dialog.nMaxFile = buffer.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.Flags = OFN_ALLOWMULTISELECT
        | OFN_FILEMUSTEXIST
        | OFN_PATHMUSTEXIST
        | OFN_NOCHANGEDIR
        | OFN_EXPLORER;

    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        return Vec::new();
    }
    parse_multiselect(&buffer)
}

/// Open a URL (http/https/mailto) with the shell association.
///
/// `ShellExecuteW` hands the URI directly to the shell without a `cmd.exe`
/// re-parse, so a crafted URL cannot inject extra command arguments the way
/// `cmd /C start <url>` could. Failures are logged, never ignored.
pub fn open_url(url: &str) {
    let verb = wide("open");
    let target = wide(url);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value > 32 on success; anything else is an
    // SE_ERR_* code.
    if result as usize <= 32 {
        tracing::warn!(url = %url, code = result as usize, "ShellExecuteW could not open the URL");
    }
}

/// Open a folder in Explorer. Used by the startup recovery window, where a
/// user may want to inspect or move the database files by hand.
pub fn open_folder(path: &std::path::Path) {
    let verb = wide("explore");
    let target = wide(&path.to_string_lossy());
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as usize <= 32 {
        tracing::warn!(path = %path.display(), code = result as usize, "ShellExecuteW could not open the folder");
    }
}

/// Turn a display name into a file name that is safe to write into a
/// folder: path separators and characters Windows rejects are replaced,
/// and the result is capped in length.
pub fn safe_file_name(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    for character in name.chars() {
        let replacement = match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            control if control.is_control() => '_',
            other => other,
        };
        cleaned.push(replacement);
    }
    let trimmed = cleaned.trim().trim_end_matches('.').to_owned();
    let trimmed = if trimmed.is_empty() {
        "attachment".to_owned()
    } else {
        trimmed
    };
    if trimmed.chars().count() > 120 {
        let mut shortened: String = trimmed.chars().take(120).collect();
        shortened.push('…');
        shortened
    } else {
        trimmed
    }
}

/// Ask the user for a folder. Returns `None` when the dialog is cancelled.
///
/// The classic folder browser is used for the same reason as the file
/// dialogs above: the WinRT pickers need an `HWND` interop object that the
/// filtered projection does not offer. It returns a filesystem directory
/// only, so the caller can save a batch of files into it.
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    let title = wide(title);
    let mut display_name = vec![0u16; 260];

    let mut info: BROWSEINFOW = unsafe { std::mem::zeroed() };
    info.hwndOwner = std::ptr::null_mut();
    info.pidlRoot = std::ptr::null_mut();
    info.pszDisplayName = display_name.as_mut_ptr();
    info.lpszTitle = title.as_ptr();
    info.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;

    let item = unsafe { SHBrowseForFolderW(&info) };
    if item.is_null() {
        return None;
    }
    let mut path = vec![0u16; 260];
    let ok = unsafe { SHGetPathFromIDListW(item, path.as_mut_ptr()) };
    unsafe { ILFree(item as *const _) };
    if ok == 0 {
        return None;
    }
    let length = path
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(path.len());
    let path = String::from_utf16_lossy(&path[..length]);
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

/// Copy text to the system clipboard. Returns false when another application
/// holds the clipboard or the allocation fails.
pub fn copy_to_clipboard(text: &str) -> bool {
    let mut units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        if EmptyClipboard() == 0 {
            let _ = CloseClipboard();
            return false;
        }
        let handle = GlobalAlloc(GMEM_MOVEABLE, units.len() * std::mem::size_of::<u16>());
        if handle.is_null() {
            let _ = CloseClipboard();
            return false;
        }
        let pointer = GlobalLock(handle) as *mut u16;
        if pointer.is_null() {
            let _ = GlobalUnlock(handle);
            let _ = CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), pointer, units.len());
        units.clear();
        let _ = GlobalUnlock(handle);
        let result = SetClipboardData(CF_UNICODETEXT as u32, handle);
        let _ = CloseClipboard();
        !result.is_null()
    }
}

/// Show the common save dialog. Returns `None` when the user cancels.
pub fn save_file(title: &str, suggested_name: &str) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let name = wide(suggested_name);
    let name_length = name.len().saturating_sub(1).min(buffer.len() - 1);
    buffer[..name_length].copy_from_slice(&name[..name_length]);

    let filter = wide("All files\0*.*\0\0");
    let title = wide(title);

    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.lpstrFile = buffer.as_mut_ptr();
    dialog.nMaxFile = buffer.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER;

    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        return None;
    }
    nul_terminated(&buffer).map(PathBuf::from)
}

const OFN_EXPLORER: u32 = 0x0008_0000;

/// Parse the double-NUL buffer returned by a multi-select open dialog.
fn parse_multiselect(buffer: &[u16]) -> Vec<PathBuf> {
    let parts = split_nul(buffer);
    match parts.as_slice() {
        [] => Vec::new(),
        [single] => vec![PathBuf::from(single)],
        [directory, files @ ..] => files
            .iter()
            .map(|file| Path::new(directory).join(file))
            .collect(),
    }
}

/// Split a wide buffer at its first NUL and decode the prefix.
fn nul_terminated(buffer: &[u16]) -> Option<String> {
    let length = buffer.iter().position(|unit| *unit == 0)?;
    Some(String::from_utf16_lossy(&buffer[..length]))
}

/// Split a double-NUL terminated wide buffer into its non-empty segments.
fn split_nul(buffer: &[u16]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (index, unit) in buffer.iter().enumerate() {
        if *unit == 0 {
            if index == start {
                break;
            }
            parts.push(String::from_utf16_lossy(&buffer[start..index]));
            start = index + 1;
        }
    }
    parts
}

/// Read an attached file and guess its MIME type from the extension.
pub fn load_attachment(path: &Path) -> Result<ComposeAttachment, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "attachment".to_owned());
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("Could not read {name}: {error}"))?;
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{name} is larger than {}",
            file_size(MAX_ATTACHMENT_BYTES)
        ));
    }
    let data = std::fs::read(path).map_err(|error| format!("Could not read {name}: {error}"))?;
    if data.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{name} is larger than {}",
            file_size(MAX_ATTACHMENT_BYTES)
        ));
    }
    Ok(ComposeAttachment::new(name, mime_type(path), data))
}

/// Write attachment bytes to the path chosen by the user.
pub fn save_attachment(path: &Path, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|error| format!("Could not save the attachment: {error}"))
}

/// Human readable size for the attachment picker's error messages.
fn file_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < MIB {
        format!("{:.0} KB", value / KIB)
    } else {
        format!("{:.1} MB", value / MIB)
    }
}

/// A deliberately small extension table. Unknown types fall back to the generic
/// binary type, which is what the SMTP builder expects.
fn mime_type(path: &Path) -> String {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
    .to_owned()
}

/// NUL-terminated UTF-16.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiselect_buffers_are_parsed() {
        let single = wide("C:\\Mail\\note.txt");
        assert_eq!(
            parse_multiselect(&single),
            vec![PathBuf::from("C:\\Mail\\note.txt")]
        );

        let multiple = wide("C:\\Mail\0a.txt\0b.txt\0c.txt\0");
        assert_eq!(
            parse_multiselect(&multiple),
            vec![
                PathBuf::from("C:\\Mail\\a.txt"),
                PathBuf::from("C:\\Mail\\b.txt"),
                PathBuf::from("C:\\Mail\\c.txt"),
            ]
        );
    }

    #[test]
    fn extensions_map_to_mime_types() {
        assert_eq!(mime_type(Path::new("photo.PNG")), "image/png");
        assert_eq!(
            mime_type(Path::new("letter.unknown")),
            "application/octet-stream"
        );
    }
}
