//! Desktop/host attachment import. The provider-facing shape is shared with
//! TUI attachments; bytes are stored privately, never embedded in WAL.
use crate::attachments::{read_bounded, MAX_FILE_BYTES, MAX_IMAGE_BYTES};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

pub fn import_file(path: &Path, workdir: &Path) -> Result<Value> {
    let full = path
        .canonicalize()
        .with_context(|| format!("file not found: {}", path.display()))?;
    let meta = full.metadata()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        bail!("attachment must be a regular file under 32 MiB");
    }
    let bytes = read_bounded(&full, MAX_FILE_BYTES)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
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
    if !mime.is_empty() && bytes.len() > MAX_IMAGE_BYTES {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workdir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kroma-host-import-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ordinary_files_become_private_file_attachments() -> Result<()> {
        let dir = workdir("file");
        let source = dir.join("notes.txt");
        fs::write(&source, b"hello")?;
        let item = import_file(&source, &dir)?;
        assert_eq!(item["type"], "kroma_attachment");
        assert_eq!(item["kind"], "file");
        assert_eq!(item["name"], "notes.txt");
        assert_eq!(item["mime"], Value::Null);
        assert_eq!(item["bytes"], 5);
        let stored = Path::new(item["path"].as_str().unwrap());
        assert_ne!(
            stored,
            source.as_path(),
            "the source is copied, not referenced"
        );
        assert!(stored.to_string_lossy().contains("attachments"));
        assert_eq!(fs::read(stored)?, b"hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(stored)?.permissions().mode() & 0o777, 0o600);
        }
        fs::remove_file(stored)?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn known_image_signatures_keep_their_mime() -> Result<()> {
        let dir = workdir("image");
        for (name, bytes, mime) in [
            ("shot.png", b"\x89PNG\r\n\x1a\nbody".as_slice(), "image/png"),
            ("shot.jpg", b"\xff\xd8\xffbody".as_slice(), "image/jpeg"),
            (
                "shot.webp",
                b"RIFF\x00\x00\x00\x00WEBPbody".as_slice(),
                "image/webp",
            ),
        ] {
            let source = dir.join(name);
            fs::write(&source, bytes)?;
            let item = import_file(&source, &dir)?;
            assert_eq!(item["kind"], "image", "{name}");
            assert_eq!(item["mime"], mime, "{name}");
            assert_eq!(item["name"], name);
            assert_eq!(item["bytes"], bytes.len() as u64);
            let stored = Path::new(item["path"].as_str().unwrap());
            assert_eq!(fs::read(stored)?, bytes);
            fs::remove_file(stored)?;
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn import_rejects_missing_paths_directories_and_oversized_images() -> Result<()> {
        let dir = workdir("reject");
        assert!(import_file(&dir.join("missing.txt"), &dir)
            .unwrap_err()
            .to_string()
            .contains("file not found"));
        assert!(import_file(&dir, &dir)
            .unwrap_err()
            .to_string()
            .contains("regular file"));

        // Shared limits come from the attachment model, not a private copy.
        let large = dir.join("large.bin");
        fs::File::create(&large)?.set_len(MAX_FILE_BYTES + 1)?;
        assert!(import_file(&large, &dir)
            .unwrap_err()
            .to_string()
            .contains("under 32 MiB"));

        let image = dir.join("large.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n")?;
        fs::OpenOptions::new()
            .write(true)
            .open(&image)?
            .set_len(MAX_IMAGE_BYTES as u64 + 1)?;
        assert!(import_file(&image, &dir)
            .unwrap_err()
            .to_string()
            .contains("image exceeds 8 MiB"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
