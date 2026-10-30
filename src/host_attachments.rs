//! Desktop/host attachment import. The provider-facing shape is shared with
//! TUI attachments; bytes are stored privately, never embedded in WAL.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{fs, io::Read, path::Path};

const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_IMAGE: u64 = 8 * 1024 * 1024;

pub fn import_file(path: &Path, workdir: &Path) -> Result<Value> {
    let full = path
        .canonicalize()
        .with_context(|| format!("file not found: {}", path.display()))?;
    let meta = full.metadata()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        bail!("attachment must be a regular file under 32 MiB");
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    fs::File::open(&full)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        bail!("file changed during import");
    }
    let (mime, extension) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("image/png", "png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ("image/jpeg", "jpg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        ("image/webp", "webp")
    } else {
        ("", "file")
    };
    if !mime.is_empty() && bytes.len() as u64 > MAX_IMAGE {
        bail!("image exceeds 8 MiB");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workdir.to_path_buf());
    let stored = crate::private_file::save(
        &home.join(".kroma").join("attachments"),
        "paste",
        extension,
        &bytes,
    )?;
    let name = full
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect::<String>();
    Ok(
        json!({"type":"kroma_attachment","kind":if mime.is_empty(){"file"}else{"image"},
        "name":name,"path":stored,"mime":if mime.is_empty(){None}else{Some(mime)},
        "bytes":bytes.len()}),
    )
}
