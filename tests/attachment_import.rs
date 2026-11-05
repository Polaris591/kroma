//! Desktop attachment import is the public contract shared with the TUI. This
//! keeps the wire shape and the shared size limits from drifting apart.
use anyhow::Result;
use kroma::host_attachments::import_file;
use serde_json::Value;
use std::{fs, path::Path, path::PathBuf, time::SystemTime, time::UNIX_EPOCH};

fn test_dir(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-attachment-import-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Bytes live in private storage; the WAL only ever sees this reference.
#[test]
fn imported_attachments_keep_the_compact_wire_shape() -> Result<()> {
    let dir = test_dir("shape")?;
    let cases = [
        ("notes.txt", b"plain text".as_slice(), "file", Value::Null),
        (
            "shot.png",
            b"\x89PNG\r\n\x1a\nbody".as_slice(),
            "image",
            Value::from("image/png"),
        ),
    ];
    for (name, bytes, kind, mime) in cases {
        let source = dir.join(name);
        fs::write(&source, bytes)?;
        let item = import_file(&source, &dir)?;
        let wire = item.as_object().expect("attachment is a JSON object");
        let mut keys = wire.keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["bytes", "kind", "mime", "name", "path", "type"],
            "{name}"
        );
        assert_eq!(item["type"], "kroma_attachment");
        assert_eq!(item["kind"], kind);
        assert_eq!(item["mime"], mime);
        assert_eq!(item["name"], name);
        assert_eq!(item["bytes"], bytes.len() as u64);
        let stored = Path::new(item["path"].as_str().expect("stored path"));
        assert_ne!(stored, source.as_path(), "the source is copied");
        assert_eq!(fs::read(stored)?, bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(stored)?.permissions().mode() & 0o777, 0o600);
        }
        fs::remove_file(stored)?;
    }
    fs::remove_dir_all(dir)?;
    Ok(())
}

/// The importer must enforce the same bounds the attachment model uses, so a
/// provider request can never inline bytes the model considers too large.
#[test]
fn imported_attachments_are_bounded() -> Result<()> {
    let dir = test_dir("bounds")?;
    assert!(import_file(&dir.join("missing.png"), &dir).is_err());
    assert!(import_file(&dir, &dir).is_err());
    let large = dir.join("large.bin");
    fs::File::create(&large)?.set_len(32 * 1024 * 1024 + 1)?;
    assert!(import_file(&large, &dir)
        .unwrap_err()
        .to_string()
        .contains("32 MiB"));
    fs::remove_dir_all(dir)?;
    Ok(())
}
