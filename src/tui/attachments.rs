//! Small clipboard/file-paste support for the terminal host. Media is stored
//! as file references in the WAL, never as enormous inline base64 strings.
//! Remote terminal clipboards are not assumed to be locally accessible.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    path::{Path, PathBuf},
};

use crate::attachments::{MAX_FILE_BYTES, MAX_IMAGE_BYTES, MAX_LOG_BYTES};
pub const MAX_ATTACHMENTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    File,
    Log,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub kind: Kind,
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub lines: Option<usize>,
    pub chars: Option<usize>,
    pub mime: Option<&'static str>,
    /// Private-use token in the editor and persisted user message.
    pub marker: Option<char>,
    /// A recalled attachment points at an already-persisted source. Discarding
    /// the edited draft must not delete the original session's file.
    pub draft_copy: bool,
}

fn save(workdir: &Path, bytes: &[u8], suffix: &str) -> Result<PathBuf> {
    // Do not pollute the project worktree (and its git status) with uploads.
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.to_path_buf());
    let folder = home.join(".kroma").join("attachments");
    crate::private_file::save(&folder, "paste", suffix, bytes)
}

impl Attachment {
    /// Import a file through the shared host importer so Desktop and TUI agree
    /// on limits, mime sniffing and private storage.
    pub fn from_file(path: &Path, workdir: &Path) -> Result<Self> {
        let item = crate::host_attachments::import_file(path, workdir)?;
        let mut attachment =
            Self::from_item(&item).context("host attachment is not a usable reference")?;
        // The imported file is a private copy owned by this draft.
        attachment.draft_copy = true;
        Ok(attachment)
    }

    pub fn from_log(text: &str, workdir: &Path) -> Result<Self> {
        if text.len() > MAX_LOG_BYTES {
            bail!("pasted text is larger than 8 MiB");
        }
        let path = save(workdir, text.as_bytes(), "log")?;
        Ok(Self {
            kind: Kind::Log,
            name: "Pasted text".into(),
            path,
            bytes: text.len() as u64,
            lines: Some(text.lines().count().max(1)),
            chars: Some(text.chars().count()),
            mime: None,
            marker: None,
            draft_copy: true,
        })
    }

    pub fn from_pixels(image: arboard::ImageData<'_>, workdir: &Path) -> Result<Self> {
        let (width, height) = (image.width, image.height);
        let expected = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .context("image dimensions too large")?;
        if expected != image.bytes.len() || expected as u64 > MAX_FILE_BYTES * 2 {
            bail!("clipboard image pixels are too large");
        }
        let mut encoded = Vec::new();
        {
            let mut png = png::Encoder::new(&mut encoded, width as u32, height as u32);
            png.set_color(png::ColorType::Rgba);
            png.set_depth(png::BitDepth::Eight);
            png.write_header()?.write_image_data(image.bytes.as_ref())?;
        }
        if encoded.len() > MAX_IMAGE_BYTES {
            bail!("clipboard image exceeds 8 MiB");
        }
        let stored = save(workdir, &encoded, "png")?;
        Ok(Self {
            kind: Kind::Image,
            name: "Clipboard image".into(),
            path: stored,
            bytes: encoded.len() as u64,
            lines: None,
            chars: None,
            mime: Some("image/png"),
            marker: None,
            draft_copy: true,
        })
    }

    pub fn label(&self) -> String {
        let unit = if self.bytes >= 1_048_576 {
            format!("{:.1} MB", self.bytes as f64 / 1_048_576.0)
        } else if self.bytes >= 1024 {
            format!("{} KB", self.bytes.div_ceil(1024))
        } else {
            format!("{} B", self.bytes)
        };
        match self.kind {
            Kind::Log => format!("▤ log · {} lines · {unit}", self.lines.unwrap_or(1)),
            Kind::Image => format!("▧ {} · {unit}", self.name),
            Kind::File => format!("▣ {} · {unit}", self.name),
        }
    }

    pub fn inline_label(&self) -> String {
        match self.kind {
            Kind::Log => format!("{}字符", self.chars.unwrap_or(self.bytes as usize)),
            _ => self.name.clone(),
        }
    }

    pub fn item(&self) -> Value {
        json!({"type":"kroma_attachment","kind":match self.kind {
            Kind::Image=>"image",Kind::Log=>"log",Kind::File=>"file"
        },"name":self.name,"path":self.path,"mime":self.mime,"bytes":self.bytes,"chars":self.chars,
          "marker":self.marker.map(|c|c.to_string())})
    }

    pub fn from_item(item: &Value) -> Option<Self> {
        if item["type"] != "kroma_attachment" {
            return None;
        }
        let kind = match item["kind"].as_str()? {
            "image" => Kind::Image,
            "file" => Kind::File,
            "log" => Kind::Log,
            _ => return None,
        };
        let mime = match item["mime"].as_str() {
            Some("image/png") => Some("image/png"),
            Some("image/jpeg") => Some("image/jpeg"),
            Some("image/webp") => Some("image/webp"),
            _ => None,
        };
        Some(Self {
            kind,
            name: item["name"].as_str().unwrap_or("attachment").to_owned(),
            path: PathBuf::from(item["path"].as_str()?),
            bytes: item["bytes"].as_u64().unwrap_or_default(),
            lines: item["lines"].as_u64().map(|v| v as usize),
            chars: item["chars"].as_u64().map(|v| v as usize),
            mime,
            marker: crate::attachments::marker(item),
            draft_copy: false,
        })
    }
}

#[cfg(test)]
pub fn labels(items: Option<&[Value]>) -> Vec<String> {
    items
        .unwrap_or_default()
        .iter()
        .filter(|value| value["type"] == "kroma_attachment")
        .enumerate()
        .map(|(index, value)| {
            format!(
                "#{} {} {}",
                index + 1,
                match value["kind"].as_str().unwrap_or("") {
                    "image" => "▧",
                    "log" => "▤",
                    _ => "▣",
                },
                value["name"].as_str().unwrap_or("attachment")
            )
        })
        .take(MAX_ATTACHMENTS)
        .collect()
}

pub fn display_message(text: &str, items: Option<&[Value]>) -> String {
    let attached = items
        .unwrap_or_default()
        .iter()
        .filter(|p| p["type"] == "kroma_attachment")
        .collect::<Vec<_>>();
    let mut visible = String::new();
    for ch in text.chars() {
        if let Some(part) = attached
            .iter()
            .find(|item| crate::attachments::marker(item) == Some(ch))
        {
            let name = if part["kind"] == "log" {
                format!(
                    "{}字符",
                    part["chars"]
                        .as_u64()
                        .unwrap_or(part["bytes"].as_u64().unwrap_or(0))
                )
            } else {
                part["name"].as_str().unwrap_or("附件").to_owned()
            };
            visible.push_str(&format!("【{name}】"));
        } else if (0xE000..=0xF8FF).contains(&(ch as u32)) {
            visible.push_str("【附件不可用】");
        } else {
            visible.push(ch);
        }
    }
    for (index, part) in attached.into_iter().enumerate() {
        if crate::attachments::marker(part).is_none() {
            let kind = match part["kind"].as_str().unwrap_or("") {
                "image" => "▧",
                "log" => "▤",
                _ => "▣",
            };
            visible.push_str(&format!(
                "\n[#{} {kind} {}]",
                index + 1,
                part["name"].as_str().unwrap_or("attachment")
            ));
        }
    }
    visible
}

/// Alt+V uses the host OS clipboard. The terminal's regular paste event
/// handles text from a remote/SSH terminal independently of this.
pub enum ClipboardPaste {
    Attach(Vec<Attachment>),
    Text(String),
}

pub fn clipboard_text() -> Result<String> {
    arboard::Clipboard::new()
        .context("clipboard unavailable in this terminal environment")?
        .get_text()
        .context("clipboard has no text; use Alt+V for images or files")
}

pub fn clipboard(workdir: &Path) -> Result<ClipboardPaste> {
    let mut clipboard =
        arboard::Clipboard::new().context("clipboard unavailable in this terminal environment")?;
    if let Ok(image) = clipboard.get_image() {
        return Ok(ClipboardPaste::Attach(vec![Attachment::from_pixels(
            image, workdir,
        )?]));
    }
    let text = clipboard
        .get_text()
        .context("clipboard has no text, image or files")?;
    // Clipboard managers often encode a copied file as a local file URI.
    // A literal path is also accepted. No remote-upload protocol is assumed.
    if let Some(path) = text.trim().strip_prefix("file://") {
        return Ok(ClipboardPaste::Attach(vec![Attachment::from_file(
            Path::new(path),
            workdir,
        )?]));
    }
    if !text.contains('\n') && Path::new(text.trim()).is_file() {
        return Ok(ClipboardPaste::Attach(vec![Attachment::from_file(
            Path::new(text.trim()),
            workdir,
        )?]));
    }
    Ok(ClipboardPaste::Text(text))
}

pub fn long_paste(text: &str) -> bool {
    text.len() > 4 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn logs_and_files_are_stored_as_compact_references() -> Result<()> {
        let workdir =
            std::env::temp_dir().join(format!("kroma-attachments-test-{}", std::process::id()));
        fs::create_dir_all(&workdir)?;
        let log = Attachment::from_log(&"hello\n".repeat(30), &workdir)?;
        assert!(log.label().contains("30 lines"));
        assert!(!log.item().to_string().contains("hello"));
        assert_eq!(fs::read_to_string(&log.path)?, "hello\n".repeat(30));
        let source = workdir.join("test.png");
        fs::write(&source, b"\x89PNG\r\n\x1a\nrest")?;
        let image = Attachment::from_file(&source, &workdir)?;
        assert_eq!(image.kind, Kind::Image);
        assert!(labels(Some(&[image.item()]))[0].contains("test.png"));
        assert!(labels(Some(&[image.item()]))[0].starts_with("#1 "));
        let grouped = labels(Some(&[log.item(), image.item(), log.item()]));
        assert_eq!(grouped.len(), 3);
        assert!(grouped[0].starts_with("#1 ▤"));
        assert!(grouped[1].starts_with("#2 ▧"));
        assert!(grouped[2].starts_with("#3 ▤"));
        assert!(!long_paste(&"a\n".repeat(100)));
        assert!(!long_paste(&"x".repeat(4096)));
        assert!(long_paste(&"x".repeat(4097)));
        assert!(long_paste(&"猫".repeat(1366)));
        assert!(!long_paste("short input"));
        fs::remove_file(log.path)?;
        fs::remove_file(image.path)?;
        fs::remove_dir_all(workdir)?;
        Ok(())
    }

    fn temp_workdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kroma-attachment-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn from_file_imports_images_through_the_shared_host_path() -> Result<()> {
        let workdir = temp_workdir("image");
        let source = workdir.join("diagram.png");
        fs::write(&source, b"\x89PNG\r\n\x1a\nimage")?;
        let image = Attachment::from_file(&source, &workdir)?;
        assert_eq!(image.kind, Kind::Image);
        assert_eq!(image.name, "diagram.png");
        assert_eq!(image.mime, Some("image/png"));
        assert_eq!(image.bytes, 13);
        assert!(image.draft_copy, "an imported file is a draft copy");
        assert_ne!(image.path, source, "the source is copied, not referenced");
        assert_eq!(fs::read(&image.path)?, b"\x89PNG\r\n\x1a\nimage");
        let item = image.item();
        assert_eq!(item["type"], "kroma_attachment");
        assert_eq!(item["kind"], "image");
        assert_eq!(item["mime"], "image/png");
        let mut recalled = Attachment::from_item(&item).expect("recall the sent attachment");
        assert_eq!(recalled.kind, Kind::Image);
        assert_eq!(recalled.path, image.path);
        assert!(!recalled.draft_copy, "a recalled attachment owns no copy");
        recalled.marker = Some('\u{E001}');
        assert_eq!(
            Attachment::from_item(&recalled.item()).unwrap().marker,
            Some('\u{E001}')
        );
        assert!(image.label().contains("diagram.png"));
        fs::remove_file(&image.path)?;
        fs::remove_dir_all(workdir)?;
        Ok(())
    }

    #[test]
    fn from_file_keeps_plain_files_as_references() -> Result<()> {
        let workdir = temp_workdir("file");
        let source = workdir.join("notes.txt");
        fs::write(&source, b"hello\n")?;
        let file = Attachment::from_file(&source, &workdir)?;
        assert_eq!(file.kind, Kind::File);
        assert_eq!(file.mime, None);
        assert_eq!(file.bytes, 6);
        assert!(file.draft_copy);
        assert_eq!(fs::read_to_string(&file.path)?, "hello\n");
        let item = file.item();
        assert_eq!(item["kind"], "file");
        assert_eq!(item["mime"], Value::Null);
        assert_eq!(item["chars"], Value::Null);
        assert!(labels(Some(&[item]))[0].starts_with("#1 ▣ notes.txt"));
        fs::remove_file(&file.path)?;
        fs::remove_dir_all(workdir)?;
        Ok(())
    }

    #[test]
    fn from_file_rejects_directories_missing_paths_and_oversized_images() -> Result<()> {
        let workdir = temp_workdir("reject");
        assert!(Attachment::from_file(&workdir, &workdir)
            .unwrap_err()
            .to_string()
            .contains("regular file"));
        assert!(
            Attachment::from_file(&workdir.join("missing.txt"), &workdir)
                .unwrap_err()
                .to_string()
                .contains("file not found")
        );
        let image = workdir.join("huge.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n")?;
        fs::OpenOptions::new()
            .write(true)
            .open(&image)?
            .set_len(MAX_IMAGE_BYTES as u64 + 1)?;
        assert!(Attachment::from_file(&image, &workdir)
            .unwrap_err()
            .to_string()
            .contains("image exceeds 8 MiB"));
        fs::remove_dir_all(workdir)?;
        Ok(())
    }
}
