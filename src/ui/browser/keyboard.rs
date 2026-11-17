use super::*;

impl Browser {
    pub(super) fn object_selected(&self, key: &str) -> bool {
        if self.object_multi.is_empty() {
            self.selected.as_deref() == Some(key)
        } else {
            self.object_multi.contains(key)
        }
    }

    pub(super) fn select_object(&mut self, key: &str, control: bool, shift: bool) {
        if shift {
            let anchor = self
                .object_anchor
                .as_deref()
                .or(self.selected.as_deref())
                .unwrap_or(key);
            let range = {
                let rows = self.rows();
                let end = rows.iter().position(|entry| entry.key() == key);
                let start = rows.iter().position(|entry| entry.key() == anchor).or(end);
                start.zip(end).map(|(start, end)| {
                    rows[start.min(end)..=start.max(end)]
                        .iter()
                        .map(|entry| entry.key().to_owned())
                        .collect::<HashSet<_>>()
                })
            };
            self.object_anchor = Some(anchor.to_owned());
            if let Some(range) = range {
                if !control {
                    self.object_multi.clear();
                }
                self.object_multi.extend(range);
            }
        } else if control {
            if self.object_multi.is_empty()
                && let Some(selected) = &self.selected
            {
                self.object_multi.insert(selected.clone());
            }
            if !self.object_multi.remove(key) {
                self.object_multi.insert(key.to_owned());
            }
            self.object_anchor = Some(key.to_owned());
            self.selected = if self.object_multi.contains(key) {
                Some(key.to_owned())
            } else {
                self.rows()
                    .iter()
                    .find(|entry| self.object_multi.contains(entry.key()))
                    .map(|entry| entry.key().to_owned())
            };
            self.tree_selected = self.selected.clone();
            return;
        } else {
            self.object_multi.clear();
            self.object_anchor = Some(key.to_owned());
        }
        self.selected = Some(key.to_owned());
        self.tree_selected = self.selected.clone();
    }

    pub(super) fn keyboard(
        &mut self,
        event: &KeyDownEvent,
        tree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let key = event.keystroke.key.as_str();
        let selection = if tree {
            self.tree_selected.as_deref()
        } else {
            self.selected.as_deref()
        };
        let entry = if tree {
            self.tree
                .iter()
                .map(|(_, row)| row)
                .find(|row| Some(row.key()) == selection)
                .cloned()
        } else {
            self.selection()
        };
        if !tree && event.keystroke.modifiers.control && !event.keystroke.modifiers.alt {
            match key {
                "a" => {
                    self.object_multi = self
                        .rows()
                        .iter()
                        .map(|entry| entry.key().to_owned())
                        .collect();
                    self.selected = self
                        .selected
                        .clone()
                        .filter(|key| self.object_multi.contains(key))
                        .or_else(|| self.rows().first().map(|entry| entry.key().to_owned()));
                    self.object_anchor = self.selected.clone();
                    cx.notify();
                }
                #[cfg(windows)]
                "c" => self.copy_objects_to_clipboard(window, cx),
                #[cfg(windows)]
                "v" => self.paste_clipboard_files(window, cx),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        match key {
            "up" | "down" | "left" | "right"
                if matches!(key, "up" | "down") || (!tree && self.grid_layout) =>
            {
                let columns = if !tree && self.grid_layout {
                    self.grid_columns.get()
                } else {
                    1
                };
                let step = if matches!(key, "left" | "right") {
                    1
                } else {
                    columns
                };
                let keys: Vec<&str> = if tree {
                    self.tree.iter().map(|(_, row)| row.key()).collect()
                } else {
                    self.rows().iter().map(Entry::key).collect()
                };
                if keys.is_empty() {
                    return;
                }
                let index = keys.iter().position(|key| Some(*key) == selection);
                let index = index.map_or(0, |index| {
                    if matches!(key, "up" | "left") {
                        index.saturating_sub(step)
                    } else {
                        (index + step).min(keys.len() - 1)
                    }
                });
                let target = Some(keys[index].to_owned());
                if tree {
                    self.tree_multi.clear();
                    self.tree_range_anchor = None;
                    self.tree_selected = target;
                    self.create_draft = None;
                    self.tree_scroll
                        .scroll_to_item(self.bucket_tree_offset() + index, ScrollStrategy::Top);
                } else {
                    self.create_draft = None;
                    if let Some(target) = target {
                        self.select_object(&target, false, event.keystroke.modifiers.shift);
                    }
                    if !tree && self.grid_layout {
                        self.grid_scroll
                            .scroll_to_item(index / columns, ScrollStrategy::Top);
                    }
                    self.list_scroll.scroll_to_item(index, ScrollStrategy::Top);
                }
                cx.notify();
            }
            "escape" if !tree && !self.object_multi.is_empty() => {
                self.object_multi.clear();
                self.object_anchor = None;
                self.selected = None;
                cx.notify();
            }
            "escape" if tree && !self.tree_multi.is_empty() => {
                self.tree_multi.clear();
                self.tree_range_anchor = None;
                cx.notify();
            }
            "enter" if !tree && self.object_multi.len() > 1 => {}
            "enter" if tree && self.tree_multi.len() > 1 => {}
            "enter" => {
                if let Some(entry) = entry {
                    self.open_entry(entry, window, cx);
                }
            }
            "right" if tree => {
                if let Some(entry) = entry
                    && entry.directory()
                    && !self.expanded.contains(entry.key())
                {
                    self.disclose(entry.key().to_owned(), cx);
                }
            }
            "left" if tree => {
                if let Some(entry) = entry {
                    if self.expanded.contains(entry.key()) {
                        self.disclose(entry.key().to_owned(), cx);
                    } else if !entry.key().is_empty() {
                        self.tree_selected = Some(parent_prefix(entry.key()).to_owned());
                        cx.notify();
                    }
                }
            }
            "backspace" if !tree => self.navigate(parent_prefix(&self.prefix).to_owned(), cx),
            "f2" if !tree && self.object_multi.len() > 1 => {}
            "f2" if tree && self.tree_multi.len() > 1 => {}
            "f2" => {
                if let Some(entry) = entry.filter(|e| !e.key().is_empty()) {
                    self.edit(Edit::Rename(entry, tree), window, cx);
                }
            }
            "delete" if !tree && self.object_multi.len() > 1 => {
                self.confirm_batch_delete(false, event.keystroke.modifiers.shift, window, cx);
            }
            "delete" if tree && !self.tree_multi.is_empty() => {
                self.confirm_batch_delete(true, event.keystroke.modifiers.shift, window, cx);
            }
            "delete" => {
                if let Some(entry) = entry.filter(|e| !e.key().is_empty()) {
                    self.confirm_delete(entry, event.keystroke.modifiers.shift, window, cx);
                }
            }
            "f5" => self.refresh(cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}
