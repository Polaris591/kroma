//! Small clipboard/file-paste support for the terminal host. Media is stored
//! as file references in the WAL, never as enormous inline base64 strings.
//! Remote terminal clipboards are not assumed to be locally accessible.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    env, fs,
    io::Read,
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

fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
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
    pub fn from_file(path: &Path, workdir: &Path) -> Result<Self> {
        let full = path
            .canonicalize()
            .with_context(|| format!("file not found: {}", path.display()))?;
        let stat = full.metadata()?;
        if !stat.is_file() {
            bail!("only regular files can be attached");
        }
        if stat.len() > MAX_FILE_BYTES {
            bail!("file is larger than 32 MiB");
        }
        let mut file = fs::File::open(&full)?;
        let mut bytes = Vec::with_capacity(stat.len() as usize);
        file.read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!("file changed during import");
        }
        let mime = image_type(&bytes);
        let kind = if mime.is_some() {
            Kind::Image
        } else {
            Kind::File
        };
        if kind == Kind::Image && bytes.len() > MAX_IMAGE_BYTES {
            bail!("image is larger than 8 MiB");
        }
        let name = full
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect::<String>();
        let extension = match mime {
            Some("image/png") => "png",
            Some("image/jpeg") => "jpg",
            Some("image/webp") => "webp",
            _ => "file",
        };
        let stored = save(workdir, &bytes, extension)?;
        Ok(Self {
            kind,
            name,
            path: stored,
            bytes: bytes.len() as u64,
            lines: None,
            chars: None,
            mime,
            marker: None,
            draft_copy: true,
        })
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
    text.chars().count() >= 1000 || text.lines().count() > 12
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(long_paste(&"a\n".repeat(13)));
        assert!(!long_paste("short input"));
        fs::remove_file(log.path)?;
        fs::remove_file(image.path)?;
        fs::remove_dir_all(workdir)?;
        Ok(())
    }
}
