//! Platform-specific functionality for the GTK front-end.

use mailin_core::compose::ComposeAttachment;
use std::path::Path;

const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

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

fn mime_type(path: &Path) -> String {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "log" => "text/plain".to_string(),
        "html" | "htm" => "text/html".to_string(),
        "css" => "text/css".to_string(),
        "csv" => "text/csv".to_string(),
        "json" => "application/json".to_string(),
        "xml" => "application/xml".to_string(),
        "pdf" => "application/pdf".to_string(),
        "zip" => "application/zip".to_string(),
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "svg" => "image/svg+xml".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "ogg" => "audio/ogg".to_string(),
        "wav" => "audio/wav".to_string(),
        "mp4" => "video/mp4".to_string(),
        "webm" => "video/webm".to_string(),
        "doc" => "application/msword".to_string(),
        "docx" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string()
        }
        "xls" => "application/vnd.ms-excel".to_string(),
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
        "ppt" => "application/vnd.ms-powerpoint".to_string(),
        "pptx" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation".to_string()
        }
        _ => "application/octet-stream".to_string(),
    }
}
