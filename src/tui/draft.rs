use super::*;

impl UiState {
    pub fn attach(&mut self, mut item: Attachment) -> Result<(), String> {
        if self.attachments.len() >= MAX_ATTACHMENTS {
            Self::discard_copy(&item);
            return Err(format!("Maximum {MAX_ATTACHMENTS} attachments"));
        }
        while self.next_marker <= crate::tui::editor::ATTACHMENT_END
            && self
                .editor
                .contains_attachment(char::from_u32(self.next_marker).unwrap())
        {
            self.next_marker += 1;
        }
        let marker = char::from_u32(self.next_marker)
            .filter(|_| self.next_marker <= crate::tui::editor::ATTACHMENT_END)
            .ok_or_else(|| {
                Self::discard_copy(&item);
                "Too many draft insertions; clear or send to reset".to_string()
            })?;
        self.next_marker += 1;
        let label = item.inline_label();
        item.marker = Some(marker);
        self.editor.insert_attachment(marker, label);
        self.attachments.push(item);
        Ok(())
    }

    pub fn finish_attachment_send(&mut self) {
        // The submitted WAL owns these file refs from here onward.
        self.attachments.clear();
        for item in self.removed_drafts.drain(..) {
            Self::discard_copy(&item);
        }
        if let Some(draft) = self.saved_draft_attachments.take() {
            for item in draft {
                Self::discard_copy(&item);
            }
        }
        if let Some(draft) = self.cleared_draft.take() {
            for item in draft.attachments.into_iter().chain(draft.removed) {
                Self::discard_copy(&item);
            }
        }
        self.next_marker = crate::tui::editor::ATTACHMENT_START;
    }

    pub(in crate::tui) fn discard_copy(item: &Attachment) {
        if item.draft_copy {
            let _ = std::fs::remove_file(&item.path);
        }
    }

    /// The editor stores complete historical input including its opaque
    /// attachment references. No media is loaded just to show a past prompt.
    pub fn restore_input_history(&mut self, messages: &[Message]) {
        let entries = messages
            .iter()
            .filter(|m| m.role == "user")
            .rev()
            .take(120)
            .collect::<Vec<_>>();
        let mut records = Vec::with_capacity(entries.len());
        for message in entries.into_iter().rev() {
            let mut text = message.content.clone().unwrap_or_default();
            let mut labels = HashMap::new();
            let mut parts = message.provider_items.clone().unwrap_or_default();
            let mut candidate = crate::tui::editor::ATTACHMENT_START;
            for part in &mut parts {
                let Some(item) = Attachment::from_item(part) else {
                    continue;
                };
                let marker = if let Some(ch) = item.marker {
                    ch
                } else {
                    while candidate <= crate::tui::editor::ATTACHMENT_END
                        && char::from_u32(candidate).is_some_and(|ch| text.contains(ch))
                    {
                        candidate += 1;
                    }
                    // Old or malformed sessions may contain more attachments
                    // than the editor's private-use marker range. Leave any
                    // excess attachment unmarked (legacy list format) rather
                    // than assigning a non-marker Unicode scalar or panicking.
                    let Some(ch) = char::from_u32(candidate)
                        .filter(|_| candidate <= crate::tui::editor::ATTACHMENT_END)
                    else {
                        continue;
                    };
                    candidate += 1;
                    // Older list-style sessions had no inline position.
                    text.push(ch);
                    part["marker"] = serde_json::json!(ch.to_string());
                    ch
                };
                labels.insert(marker, format!("【{}】", item.inline_label()));
            }
            if !text.is_empty() {
                records.push((text, labels, parts));
            }
        }
        self.editor.restore_history(records);
    }

    pub fn history_navigated(&mut self, previous: Option<usize>) {
        let current = self.editor.history_index();
        if current == previous {
            return;
        }
        if previous.is_none() && current.is_some() {
            // Editing a recalled line starts a new draft. The editor replaces
            // its earlier saved draft on the next Up; keep file ownership in
            // sync with that simple history behavior.
            if let Some(abandoned) = self.saved_draft_attachments.take() {
                for item in abandoned {
                    Self::discard_copy(&item);
                }
            }
            self.saved_draft_attachments = Some(std::mem::take(&mut self.attachments));
        }
        self.attachments = if current.is_some() {
            self.editor
                .history_attachments()
                .iter()
                .filter_map(Attachment::from_item)
                .collect()
        } else {
            self.saved_draft_attachments.take().unwrap_or_default()
        };
    }

    pub fn remember_submitted_attachments(&mut self) {
        self.editor
            .remember_attachments(self.attachments.iter().map(Attachment::item).collect());
    }

    pub fn prune_attachments(&mut self) {
        let mut index = 0;
        while index < self.removed_drafts.len() {
            if self.removed_drafts[index]
                .marker
                .is_some_and(|marker| self.editor.contains_attachment(marker))
            {
                self.attachments.push(self.removed_drafts.remove(index));
            } else {
                index += 1;
            }
        }
        self.attachments.retain(|item| {
            if let Some(marker) = item.marker {
                if !self.editor.contains_attachment(marker) {
                    self.removed_drafts.push(item.clone());
                    return false;
                }
            }
            true
        });
    }

    pub fn discard_draft(&mut self) {
        self.editor.clear();
        self.attach_mode = false;
        self.attach_path.clear();
        self.next_marker = crate::tui::editor::ATTACHMENT_START;
        for item in self.attachments.drain(..) {
            Self::discard_copy(&item);
        }
        for item in self.removed_drafts.drain(..) {
            Self::discard_copy(&item);
        }
        if let Some(draft) = self.saved_draft_attachments.take() {
            for item in draft {
                Self::discard_copy(&item);
            }
        }
        if let Some(draft) = self.cleared_draft.take() {
            for item in draft.attachments.into_iter().chain(draft.removed) {
                Self::discard_copy(&item);
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct ClearedDraft {
    text: String,
    attachments: Vec<Attachment>,
    removed: Vec<Attachment>,
}

impl UiState {
    /// Clear the visible draft without deleting its draft files. Ctrl+Z
    /// restores it while the editor is empty; sending or quitting cleans up
    /// through the normal ownership path.
    pub fn clear_draft_undoable(&mut self) {
        if let Some(previous) = self.cleared_draft.take() {
            for item in previous.attachments.into_iter().chain(previous.removed) {
                Self::discard_copy(&item);
            }
        }
        let text = self.editor.text().to_owned();
        let attachments = std::mem::take(&mut self.attachments);
        let removed = std::mem::take(&mut self.removed_drafts);
        self.editor.clear();
        self.attach_mode = false;
        self.attach_path.clear();
        self.next_marker = crate::tui::editor::ATTACHMENT_START;
        self.cleared_draft = Some(ClearedDraft {
            text,
            attachments,
            removed,
        });
        let language = self.preferences.language;
        self.note(
            language
                .text(
                    "Draft cleared · Ctrl+Z restores it",
                    "已清空草稿 · Ctrl+Z 恢复",
                )
                .to_owned(),
        );
    }

    pub fn restore_cleared_draft(&mut self) -> bool {
        let Some(draft) = self.cleared_draft.take() else {
            return false;
        };
        self.editor.replace_all(&draft.text);
        for item in &draft.attachments {
            if let Some(marker) = item.marker {
                self.editor
                    .set_attachment_label(marker, item.inline_label());
            }
        }
        self.attachments = draft.attachments;
        self.removed_drafts = draft.removed;
        let language = self.preferences.language;
        self.note(language.text("Draft restored", "草稿已恢复").to_owned());
        true
    }
}
