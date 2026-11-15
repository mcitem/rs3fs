use super::*;
use crate::services::store::TextEncoding;
#[cfg(windows)]
use crate::ui::file_viewer::{self, FileKind, PreparedFile, PreparedPreview};
use gpui_kit::base::input::Rope;
use gpui_kit::component::menu::DropdownMenu;
use std::sync::atomic::AtomicBool;
use std::{cell::RefCell, rc::Rc};

pub(super) struct TextPreview {
    editor: Entity<EditorState>,
    saved: Rope,
    encoding: TextEncoding,
    store: Store,
    readonly: bool,
    readonly_hint_target: Option<Bounds<Pixels>>,
    saving: bool,
    _changes: Subscription,
}

impl TextPreview {
    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).text() != &self.saved
    }
}

impl FilePreview {
    pub(super) fn is_dirty(&self, cx: &App) -> bool {
        matches!(&self.content, PreviewContent::Ready(text) if text.is_dirty(cx))
    }
}

#[cfg(windows)]
pub(super) struct PdfPreview {
    file: Arc<PreparedFile>,
    image: Arc<Image>,
    index: u32,
    count: u32,
    loading: bool,
}

pub(super) struct PreviewDownload {
    received: Arc<AtomicU64>,
    download: bool,
    downloading: Arc<AtomicBool>,
    job: tokio::task::AbortHandle,
}

impl Drop for PreviewDownload {
    fn drop(&mut self) {
        self.job.abort();
    }
}

impl Browser {
    pub(super) fn has_pending_text_preview(&self, cx: &App) -> bool {
        self.preview.as_ref().is_some_and(|preview| {
            matches!(&preview.content, PreviewContent::Ready(text) if text.saving || text.is_dirty(cx))
        })
    }

    pub(super) fn open_entry(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        if entry.directory() {
            self.navigate(entry.key().to_owned(), cx);
            return;
        }
        if self.busy.is_some() {
            return;
        }
        if let Some(preview) = &self.preview
            && let PreviewContent::Ready(text) = &preview.content
        {
            if preview.entry.key() == entry.key()
                && (text.saving
                    || text.is_dirty(cx)
                    || (entry.etag().is_some() && preview.entry.etag() == entry.etag()))
            {
                self.show_preview = true;
                text.editor.read(cx).focus_handle(cx).focus(window, cx);
                cx.notify();
                return;
            }
            let next = entry.clone();
            if self.confirm_preview_leave(cx, move |this, window, cx| {
                this.open_entry(next, window, cx);
            }) {
                return;
            }
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        self.tree_multi.clear();
        self.tree_range_anchor = None;
        self.tree_selected = Some(entry.key().to_owned());
        #[cfg(windows)]
        if let Some((kind, extension)) = FileKind::from_name(entry.name())
            && entry.size() <= kind.limit() as i64
        {
            if self.preview.as_ref().is_some_and(|preview| {
                preview.entry.key() == entry.key()
                    && entry.etag().is_some()
                    && preview.entry.etag() == entry.etag()
                    && matches!(
                        preview.content,
                        PreviewContent::Image(_) | PreviewContent::Pdf(_)
                    )
            }) {
                self.show_preview = true;
                cx.notify();
                return;
            }
            self.open_file_viewer(store, entry, kind, extension, window, cx);
            return;
        }
        let limit = u64::from(self.preview_limit_mb) * BYTES_PER_MB;
        let unavailable = if !is_text_file(entry.name()) {
            #[cfg(windows)]
            let recognized = FileKind::from_name(entry.name()).is_some();
            #[cfg(not(windows))]
            let recognized = false;
            Some(if recognized {
                "文件超过当前预览大小上限"
            } else {
                "此文件类型暂不支持预览"
            })
        } else if entry.size() > limit as i64 {
            Some("文件超过当前预览大小上限")
        } else {
            None
        };
        if let Some(reason) = unavailable {
            self.preview = Some(FilePreview {
                entry,
                content: PreviewContent::Unavailable(reason),
            });
            self.show_preview = true;
            self.tree_focus.focus(window, cx);
            cx.notify();
            return;
        }
        let source = entry.clone();
        let editor_store = store.clone();
        let slots = self.transfer_slots.clone();
        #[cfg(windows)]
        let profile = self.connected_profile;
        self.load_preview(
            entry,
            true,
            window,
            cx,
            move |received, downloading| async move {
                #[cfg(windows)]
                return file_viewer::prepare_text(
                    store,
                    source,
                    profile,
                    limit,
                    received,
                    downloading,
                    slots,
                )
                .await;
                #[cfg(not(windows))]
                {
                    let directory = tempfile::tempdir()?;
                    let path = directory.path().join("source.txt");
                    downloading.store(true, Ordering::Relaxed);
                    let result = store
                        .download_preview_with_workers(&source, &path, limit, &received, &slots)
                        .await;
                    downloading.store(false, Ordering::Relaxed);
                    result?;
                    crate::services::store::decode_editable_text(tokio::fs::read(path).await?)
                }
            },
            move |(text, encoding), name, window, cx| {
                let editor = cx.new(|cx| {
                    let editor = EditorState::new(window, cx).soft_wrap(false);
                    let editor = match preview_language(name) {
                        Some(language) => editor.language(language),
                        None => editor,
                    };
                    editor.default_value(text)
                });
                let saved = editor.read(cx).text().clone();
                let changes = cx.subscribe(&editor, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                });
                editor.read(cx).focus_handle(cx).focus(window, cx);
                Ok(PreviewContent::Ready(TextPreview {
                    editor,
                    saved,
                    encoding,
                    store: editor_store,
                    readonly: true,
                    readonly_hint_target: None,
                    saving: false,
                    _changes: changes,
                }))
            },
        );
    }

    #[cfg(windows)]
    fn open_file_viewer(
        &mut self,
        store: Store,
        entry: Entry,
        kind: FileKind,
        extension: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = entry.clone();
        let profile = self.connected_profile;
        let svg_renderer = cx.svg_renderer();
        let slots = self.transfer_slots.clone();
        self.load_preview(
            entry,
            true,
            window,
            cx,
            move |received, downloading| {
                file_viewer::prepare(
                    store,
                    source,
                    profile,
                    kind,
                    extension,
                    svg_renderer,
                    received,
                    downloading,
                    slots,
                )
            },
            |prepared, _, _, _| {
                Ok(match prepared {
                    PreparedPreview::Image(image) => PreviewContent::Image(image),
                    PreparedPreview::Pdf(file, image, count) => PreviewContent::Pdf(PdfPreview {
                        file,
                        image,
                        count,
                        index: 0,
                        loading: false,
                    }),
                })
            },
        );
    }

    fn load_preview<T: Send + 'static, F: Future<Output = Result<T>> + Send + 'static>(
        &mut self,
        entry: Entry,
        download: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        prepare: impl FnOnce(Arc<AtomicU64>, Arc<AtomicBool>) -> F,
        open: impl FnOnce(T, &str, &mut Window, &mut Context<Self>) -> Result<PreviewContent> + 'static,
    ) {
        let received = Arc::new(AtomicU64::new(0));
        let downloading = Arc::new(AtomicBool::new(false));
        let target = format!(
            "{:?} /{:?}",
            self.store.as_ref().map_or("", Store::bucket),
            entry.key()
        );
        let started = Instant::now();
        self.log(
            false,
            format!("正在读取预览，{target}，{} B", entry.size().max(0)),
        );
        let mut job = self
            .runtime
            .spawn(prepare(received.clone(), downloading.clone()));
        self.preview = Some(FilePreview {
            entry,
            content: PreviewContent::Loading(PreviewDownload {
                received: received.clone(),
                download,
                downloading: downloading.clone(),
                job: job.abort_handle(),
            }),
        });
        self.show_preview = true;
        self.tree_focus.focus(window, cx);
        self.error = None;
        self.clear_notice();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |view, cx| {
            let mut last_progress = (0, false);
            let result = loop {
                tokio::select! {
                    result = &mut job => break result.map_err(anyhow::Error::from).and_then(|result| result),
                    _ = executor.timer(Duration::from_millis(100)), if download => {
                        let current = (received.load(Ordering::Relaxed), downloading.load(Ordering::Relaxed));
                        if current != last_progress {
                            last_progress = current;
                            let _ = view.update(cx, |this, cx| {
                                if this.is_loading_preview(&received) { cx.notify(); }
                            });
                        }
                    }
                }
            };
            let _ = cx.update(|window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if !this.is_loading_preview(&received) {
                        return;
                    }
                    let name = this.preview.as_ref().unwrap().entry.name().to_owned();
                    let content = match result.and_then(|value| open(value, &name, window, cx)) {
                        Ok(content) => {
                            this.log(false, format!("预览完成，{target}，下载 {} B，{:.2} 秒", received.load(Ordering::Relaxed), started.elapsed().as_secs_f64()));
                            content
                        }
                        Err(error) => {
                            this.log(true, format!("读取预览失败，{target}，{:.2} 秒，{error:#}", started.elapsed().as_secs_f64()));
                            PreviewContent::Failed(format!("{error:#}").into())
                        }
                    };
                    if this.is_loading_preview(&received) {
                        this.preview.as_mut().unwrap().content = content;
                        cx.notify();
                    }
                });
            });
        }).detach();
        cx.notify();
    }

    pub(super) fn active_preview_download(&self) -> Option<(&Entry, i64)> {
        let preview = self.preview.as_ref()?;
        match &preview.content {
            PreviewContent::Loading(task) if task.downloading.load(Ordering::Relaxed) => {
                Some((&preview.entry, task.received.load(Ordering::Relaxed) as i64))
            }
            _ => None,
        }
    }

    fn is_loading_preview(&self, received: &Arc<AtomicU64>) -> bool {
        self.preview.as_ref().is_some_and(|preview| {
            matches!(&preview.content, PreviewContent::Loading(task) if Arc::ptr_eq(&task.received, received))
        })
    }

    #[cfg(windows)]
    fn turn_pdf_page(&mut self, index: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(FilePreview {
            content: PreviewContent::Pdf(pdf),
            ..
        }) = &mut self.preview
        else {
            return;
        };
        if pdf.loading || index >= pdf.count || index == pdf.index {
            return;
        }
        pdf.loading = true;
        let file = pdf.file.clone();
        let render_file = file.clone();
        let job = self
            .runtime
            .spawn_blocking(move || file_viewer::pdf_page(render_file.path(), index));
        cx.spawn_in(window, async move |view, cx| {
            let result = job
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let _ = cx.update(|_, cx| {
                let _ = view.update(cx, |this, cx| {
                    let Some(FilePreview {
                        content: PreviewContent::Pdf(pdf),
                        ..
                    }) = &mut this.preview
                    else {
                        return;
                    };
                    if !Arc::ptr_eq(&pdf.file, &file) {
                        return;
                    }
                    pdf.loading = false;
                    match result {
                        Ok((image, _)) => {
                            pdf.index = index;
                            pdf.image = image;
                            cx.notify();
                        }
                        Err(error) => this.show_error(error, cx),
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    #[cfg(windows)]
    fn pdf_pane(&self, pdf: &PdfPreview, cx: &Context<Self>) -> AnyElement {
        let previous = pdf.index.saturating_sub(1);
        let next = pdf.index + 1;
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("pdf-prev")
                            .small()
                            .ghost()
                            .label("上一页")
                            .disabled(pdf.loading || pdf.index == 0)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.turn_pdf_page(previous, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{} / {}", pdf.index + 1, pdf.count)),
                    )
                    .child(
                        Button::new("pdf-next")
                            .small()
                            .ghost()
                            .label("下一页")
                            .disabled(pdf.loading || next >= pdf.count)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.turn_pdf_page(next, window, cx)
                            })),
                    )
                    .when(pdf.loading, |bar| bar.child(Spinner::new().small())),
            )
            .child(
                div().flex_1().min_h_0().child(
                    img(pdf.image.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                ),
            )
            .into_any_element()
    }

    pub(super) fn close_preview(&mut self) {
        self.preview = None;
        self.show_preview = false;
    }

    pub(super) fn cancel_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_preview_leave(cx, Self::cancel_preview) {
            return;
        }
        self.close_preview();
        self.tree_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn confirm_preview_leave(
        &mut self,
        cx: &mut Context<Self>,
        proceed: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        let Some(FilePreview {
            entry,
            content: PreviewContent::Ready(text),
        }) = &self.preview
        else {
            return false;
        };
        if text.saving {
            self.show_notice("正在保存，请等待保存完成", cx);
            return true;
        }
        if !text.is_dirty(cx) {
            return false;
        }
        if self.confirming_preview_leave {
            return true;
        }
        self.confirming_preview_leave = true;
        let name = format!("“{}”有未保存的更改", entry.name());
        let window = self.preview_window;
        let view = cx.weak_entity();
        let proceed = Rc::new(RefCell::new(Some(proceed)));
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                window.open_dialog(cx, move |dialog, _, _| {
                    let owner = view.clone();
                    let proceed = proceed.clone();
                    let finish = move |save, window: &mut Window, cx: &mut App| {
                        let Some(proceed) = proceed.borrow_mut().take() else {
                            return;
                        };
                        window.close_dialog(cx);
                        let _ = owner.update(cx, |this, cx| {
                            this.confirming_preview_leave = false;
                            let leave =
                                move |this: &mut Self,
                                      window: &mut Window,
                                      cx: &mut Context<Self>| {
                                    this.close_preview();
                                    proceed(this, window, cx);
                                    cx.notify();
                                };
                            if save {
                                this.save_text_preview_with(window, cx, leave);
                            } else {
                                leave(this, window, cx);
                            }
                        });
                    };
                    let save = finish.clone();
                    let owner = view.clone();
                    dialog
                        .title(name.clone())
                        .overlay_closable(false)
                        .on_close(move |_, _, cx| {
                            let _ =
                                owner.update(cx, |this, _| this.confirming_preview_leave = false);
                        })
                        .footer(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("discard-text-changes")
                                        .outline()
                                        .label("放弃更改")
                                        .on_click(move |_, window, cx| finish(false, window, cx)),
                                )
                                .child(
                                    Button::new("save-text-and-continue")
                                        .primary()
                                        .label("保存并继续")
                                        .on_click(move |_, window, cx| save(true, window, cx)),
                                ),
                        )
                });
            });
        });
        true
    }

    pub(super) fn save_text_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.save_text_preview_with(window, cx, |_, _, _| {})
    }

    pub(super) fn saving_in_scope(&self, scope: &TransferScopes) -> bool {
        self.preview.as_ref().is_some_and(|preview| {
            matches!(&preview.content, PreviewContent::Ready(text)
                if text.saving && scope.contains_entry(text.store.bucket(), preview.entry.key(), false))
        })
    }

    fn save_text_preview_with(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_saved: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        if self.preview.as_ref().is_some_and(|preview| {
            matches!(&preview.content, PreviewContent::Ready(text) if text.is_dirty(cx)
                && self.write_locked_in(text.store.bucket(), preview.entry.key(), false))
        }) {
            self.show_write_locked(cx);
            return true;
        }
        let Some(FilePreview {
            entry,
            content: PreviewContent::Ready(text),
        }) = &mut self.preview
        else {
            return false;
        };
        if text.saving {
            return true;
        }
        if !text.is_dirty(cx) {
            on_saved(self, window, cx);
            return true;
        }
        let Some(etag) = entry.etag().map(str::to_owned) else {
            self.show_error(anyhow::anyhow!("缺少对象 ETag，无法安全保存"), cx);
            return true;
        };
        let key = entry.key().to_owned();
        let editor = text.editor.clone();
        let saved = editor.read(cx).text().clone();
        let encoding = text.encoding;
        let store = text.store.clone();
        let revision = self.bucket_revision;
        text.saving = true;
        let snapshot = saved.clone();
        let job = self.runtime.spawn(async move {
            let value = snapshot.to_string();
            ensure!(!value.contains('\0'), "文本包含空字符，无法保存");
            let bytes = encoding.encode(&value);
            let size = bytes.len() as i64;
            let etag = store.save_text(&key, &etag, bytes).await?;
            Ok((size, etag))
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = job
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let _ = cx.update(|window, cx| {
                let _ = view.update(cx, |this, cx| {
                    let Some(FilePreview {
                        entry,
                        content: PreviewContent::Ready(text),
                    }) = &mut this.preview
                    else {
                        return;
                    };
                    if text.editor != editor {
                        return;
                    }
                    text.saving = false;
                    match result {
                        Ok((size, etag)) => {
                            entry.update_after_write(size, etag);
                            text.saved = saved;
                            let clean = !text.is_dirty(cx);
                            if entry.etag().is_none() && clean {
                                text.readonly = true;
                            }
                            this.error = None;
                            if clean {
                                on_saved(this, window, cx);
                            }
                            if this.bucket_revision == revision {
                                this.upload_refresh_pending = true;
                                this.refresh_after_upload(cx);
                                this.show_notice(
                                    if clean {
                                        "已保存到存储桶"
                                    } else {
                                        "已保存，之后的修改仍未保存"
                                    },
                                    cx,
                                );
                            }
                        }
                        Err(error) => {
                            this.show_error(error.context("保存失败，更改仍保留在编辑器中"), cx)
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
        true
    }

    fn set_text_preview_mode(
        &mut self,
        editor: &Entity<EditorState>,
        readonly: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(FilePreview {
            entry,
            content: PreviewContent::Ready(text),
        }) = &mut self.preview
            && &text.editor == editor
            && (readonly || entry.etag().is_some())
        {
            text.readonly = readonly;
            text.readonly_hint_target = None;
            text.editor.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn readonly_hint_key_down(
        &mut self,
        key: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if key.key == "escape"
            && let Some(FilePreview {
                content: PreviewContent::Ready(text),
                ..
            }) = &mut self.preview
            && text.readonly_hint_target.is_some()
        {
            text.readonly_hint_target = None;
            text.editor.read(cx).focus_handle(cx).focus(window, cx);
            cx.stop_propagation();
            cx.notify();
        } else if is_text_edit_key(key) {
            self.show_readonly_hint(window, cx);
        }
    }

    pub(crate) fn show_readonly_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_preview
            && self.settings_page.is_none()
            && !self.tasks_maximized
            && !window.has_active_dialog(cx)
            && !window.has_active_sheet(cx)
            && let Some(FilePreview {
                content: PreviewContent::Ready(text),
                ..
            }) = &mut self.preview
            && text.readonly
        {
            let editor = text.editor.read(cx);
            if editor.focus_handle(cx).is_focused(window) {
                let target = editor
                    .cursor_layout()
                    .map(|(caret, _)| caret)
                    .unwrap_or_else(|| editor.input_bounds());
                if text.readonly_hint_target != Some(target) {
                    text.readonly_hint_target = Some(target);
                    cx.notify();
                }
            }
        }
    }

    fn readonly_hint(&self, entry: &Entry, text: &TextPreview, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let has_etag = entry.etag().is_some();
        let editor = text.editor.clone();
        let dismiss_editor = editor.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .p_2()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .text_sm()
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_md()
            .child(if has_etag {
                "编辑器处于只读模式。"
            } else {
                "缺少对象 ETag，无法安全编辑。"
            })
            .when(has_etag, |hint| {
                hint.child(
                    Button::new("enable-text-editing")
                        .xsmall()
                        .outline()
                        .label("切换为可编辑")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_text_preview_mode(&editor, false, window, cx);
                        })),
                )
            })
            .child(
                Button::new("dismiss-readonly-hint")
                    .xsmall()
                    .ghost()
                    .icon(IconName::X)
                    .accessibility_label("关闭只读提示")
                    .tooltip("关闭只读提示")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_text_preview_mode(&dismiss_editor, true, window, cx);
                    })),
            )
    }

    fn text_preview_status(
        &self,
        entry: &Entry,
        text: &TextPreview,
        cx: &Context<Self>,
    ) -> AnyElement {
        let owner = cx.weak_entity();
        let editor = text.editor.clone();
        let readonly = text.readonly;
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap_2()
            .px_2()
            .flex_shrink_0()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .children(text.saving.then_some("正在保存…"))
            .child(
                Button::new("text-preview-mode")
                    .ghost()
                    .xsmall()
                    .label(if readonly { "只读" } else { "可编辑" })
                    .accessibility_label(if readonly {
                        "编辑模式：只读"
                    } else {
                        "编辑模式：可编辑"
                    })
                    .icon(IconName::ChevronDown)
                    .disabled(
                        entry.etag().is_none()
                            || self.write_locked_in(text.store.bucket(), entry.key(), false),
                    )
                    .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
                        [(true, "只读"), (false, "可编辑")].into_iter().fold(
                            menu,
                            |menu, (mode, label)| {
                                let owner = owner.clone();
                                let editor = editor.clone();
                                menu.item(
                                    PopupMenuItem::new(label)
                                        .checked(readonly == mode)
                                        .on_click(move |_, window, cx| {
                                            let _ = owner.update(cx, |this, cx| {
                                                this.set_text_preview_mode(
                                                    &editor, mode, window, cx,
                                                );
                                            });
                                        }),
                                )
                            },
                        )
                    }),
            )
            .into_any_element()
    }

    #[inline(never)]
    pub(super) fn preview_pane(&self, preview: &FilePreview, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let content = match &preview.content {
            PreviewContent::Ready(text) => div()
                .size_full()
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    this.readonly_hint_key_down(&event.keystroke, window, cx);
                }))
                .child(
                    Editor::new(&text.editor)
                        .appearance(false)
                        .bg(theme
                            .highlight_theme
                            .style
                            .editor_background
                            .unwrap_or(theme.background))
                        .text_color(
                            theme
                                .highlight_theme
                                .style
                                .editor_foreground
                                .unwrap_or(theme.foreground),
                        )
                        .readonly(
                            text.readonly
                                || self.busy.is_some()
                                || self.write_locked_in(
                                    text.store.bucket(),
                                    preview.entry.key(),
                                    false,
                                ),
                        )
                        .aria_label("文件内容")
                        .h(relative(1.)),
                )
                .when_some(text.readonly_hint_target, |pane, target| {
                    pane.child(deferred(
                        anchored()
                            .anchor(Anchor::BottomLeft)
                            .attach(target, Anchor::TopLeft)
                            .offset(point(px(0.), px(-4.)))
                            .snap_to_window_with_margin(px(6.))
                            .child(self.readonly_hint(&preview.entry, text, cx)),
                    ))
                })
                .into_any_element(),
            #[cfg(windows)]
            PreviewContent::Image(image) => img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            #[cfg(windows)]
            PreviewContent::Pdf(pdf) => self.pdf_pane(pdf, cx),
            PreviewContent::Loading(task) => {
                let received = task.received.load(Ordering::Relaxed) as i64;
                let total = preview.entry.size();
                let percent =
                    (total > 0).then(|| (100. * received as f64 / total as f64).min(100.));
                let label = if !task.download {
                    "正在准备本地文件".to_owned()
                } else if total >= 0 && received >= total {
                    "正在准备预览".to_owned()
                } else if total > 0 {
                    format!(
                        "正在下载预览，{} / {}，{:.0}%",
                        size_label(received),
                        size_label(total),
                        percent.unwrap_or(0.)
                    )
                } else {
                    format!("正在下载预览，{}", size_label(received))
                };
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_2()
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_color(theme.muted_foreground)
                            .child(Spinner::new().small())
                            .child(label),
                    )
                    .when_some(percent.filter(|_| task.download), |view, percent| {
                        view.child(
                            Progress::new("preview-download-progress")
                                .w_64()
                                .max_w_full()
                                .value(percent as f32)
                                .accessibility_label("预览下载进度"),
                        )
                    })
                    .child(
                        Button::new("cancel-preview")
                            .small()
                            .label("取消预览")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel_preview(window, cx);
                            })),
                    )
                    .into_any_element()
            }
            PreviewContent::Unavailable(reason) => self.preview_download(
                &preview.entry,
                div().text_color(theme.muted_foreground).child(*reason),
                cx,
            ),
            PreviewContent::Failed(error) => self.preview_download(
                &preview.entry,
                div().text_color(theme.danger).child(error.clone()),
                cx,
            ),
        };
        let pane = div().flex_1().min_h_0().overflow_hidden().child(content);
        #[cfg(windows)]
        let pane = {
            let entry = preview.entry.clone();
            let owner = cx.weak_entity();
            let revision = self.bucket_revision;
            pane.id("file-preview-content")
                .context_menu(move |menu, window, cx| {
                    Self::open_with_submenu(menu, &entry, &owner, revision, window, cx)
                })
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(self.path_bar(Some(&preview.entry), cx))
            .child(pane)
            .when_some(
                match &preview.content {
                    PreviewContent::Ready(text) => {
                        Some(self.text_preview_status(&preview.entry, text, cx))
                    }
                    _ => None,
                },
                |pane, status| pane.child(status),
            )
            .into_any_element()
    }

    fn preview_download(&self, entry: &Entry, message: Div, cx: &Context<Self>) -> AnyElement {
        let entry = entry.clone();
        #[cfg(windows)]
        let open_entry = entry.clone();
        #[cfg(windows)]
        let menu_entry = entry.clone();
        #[cfg(windows)]
        let menu_owner = cx.weak_entity();
        #[cfg(windows)]
        let revision = self.bucket_revision;
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap_2()
            .p_4()
            .child(message)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("文件大小：{}", size_label(entry.size()))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("download-preview-file")
                            .small()
                            .icon(IconName::Download)
                            .label("下载")
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.download(entry.clone(), window, cx);
                            })),
                    )
                    .when(cfg!(windows), |buttons| {
                        #[cfg(windows)]
                        let buttons = buttons.child(
                            Button::new("open-preview-file-with")
                                .small()
                                .label("打开方式")
                                .disabled(self.busy.is_some())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_preview_with(open_entry.clone(), None, window, cx);
                                }))
                                .context_menu(move |menu, _, _| {
                                    Self::open_with_items(menu, &menu_entry, &menu_owner, revision)
                                }),
                        );
                        buttons
                    }),
            )
            .into_any_element()
    }

    #[cfg(windows)]
    pub(super) fn open_with_submenu(
        menu: PopupMenu,
        entry: &Entry,
        owner: &WeakEntity<Self>,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let entry = entry.clone();
        let owner = owner.clone();
        menu.submenu("打开方式", window, cx, move |menu, _, _| {
            Self::open_with_items(menu, &entry, &owner, revision)
        })
    }

    #[cfg(windows)]
    fn open_with_items(
        mut menu: PopupMenu,
        entry: &Entry,
        owner: &WeakEntity<Self>,
        revision: u64,
    ) -> PopupMenu {
        let apps = file_viewer::open_with_apps(entry.name());
        let has_apps = !apps.is_empty();
        for (id, label) in apps {
            let owner = owner.clone();
            let entry = entry.clone();
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if this.bucket_revision == revision {
                        this.open_preview_with(entry.clone(), Some(id.clone()), window, cx);
                    }
                });
            }));
        }
        if has_apps {
            menu = menu.separator();
        }
        let owner = owner.clone();
        let entry = entry.clone();
        menu.item(
            PopupMenuItem::new("选择其他应用").on_click(move |_, window, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if this.bucket_revision == revision {
                        this.open_preview_with(entry.clone(), None, window, cx);
                    }
                });
            }),
        )
    }

    #[cfg(windows)]
    fn open_preview_with(
        &mut self,
        entry: Entry,
        application: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let source = entry.clone();
        let profile = self.connected_profile;
        let slots = self.transfer_slots.clone();
        self.load_preview(
            entry,
            true,
            window,
            cx,
            move |received, downloading| {
                file_viewer::prepare_external(store, source, profile, received, downloading, slots)
            },
            move |file, _, window, cx| {
                let owner = cx.weak_entity();
                // The shell picker needs a separate STA and runs its own message loop.
                let (send, receive) = tokio::sync::oneshot::channel();
                std::thread::Builder::new()
                    .name("s3fs-open-with".into())
                    .spawn(move || {
                        let _ = send.send(file.choose_application(application));
                    })?;
                window
                    .spawn(cx, async move |cx| {
                        if let Ok(Err(error)) = receive.await {
                            let _ = cx.update(|_, cx| {
                                let _ = owner.update(cx, |this, cx| this.show_error(error, cx));
                            });
                        }
                    })
                    .detach();
                Ok(PreviewContent::Unavailable("此文件可用本地应用打开"))
            },
        );
    }

    // Keep the sidebar and object pane builders in separate stack frames.
    // In debug builds, composing both inline can exhaust Windows' main-thread stack.
}

fn is_text_edit_key(keystroke: &Keystroke) -> bool {
    let key = keystroke.key.as_str();
    let modifiers = keystroke.modifiers;
    if modifiers.alt {
        return false;
    }
    if modifiers.control || modifiers.platform {
        return matches!(key, "v" | "x" | "z" | "y" | "backspace" | "delete");
    }
    matches!(key, "backspace" | "delete" | "enter" | "space")
        || keystroke
            .key_char
            .as_deref()
            .unwrap_or(key)
            .chars()
            .next()
            .is_some_and(|character| {
                !character.is_control()
                    && (keystroke.key_char.is_some() || key.chars().count() == 1)
            })
}

fn preview_language(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    let stem = name.split('.').next().unwrap_or_default();
    if matches!(stem, "dockerfile" | "containerfile") {
        return Some("dockerfile");
    }
    if name == "cargo.lock" {
        return Some("toml");
    }
    let (_, extension) = name.rsplit_once('.')?;
    match extension {
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
        "rs" => Some("rust"),
        "py" | "pyw" => Some("python"),
        "sh" | "bash" => Some("bash"),
        "ps1" | "psm1" | "psd1" => Some("powershell"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "md" | "markdown" => Some("markdown"),
        "dockerfile" | "containerfile" => Some("dockerfile"),
        "json" | "jsonc" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "xml" => Some("xml"),
        _ => None,
    }
}

#[cfg(test)]
#[::core::prelude::v1::test]
fn preview_grammars_produce_highlight_styles() {
    use gpui_kit::component::{
        highlighter::{HighlightTheme, SyntaxHighlighter},
        input::Rope,
    };

    crate::register_preview_grammars();
    let theme = HighlightTheme::default_dark();
    for (name, source) in [
        ("a.ts", "const x: string = 'abc';"),
        ("a.tsx", "const x = <span>Hello</span>;"),
        ("a.js", "const x = 'abc';"),
        ("a.rs", "fn main() {}"),
        ("a.py", "def main(): pass"),
        ("a.sh", "if true; then echo 'abc'; fi"),
        ("a.ps1", "if ($true) { Write-Host 'abc' }"),
        ("a.html", "<p class='test'>Hello</p>"),
        ("a.css", "p { color: red; }"),
        ("a.md", "# Heading\n\n**bold**"),
        ("Dockerfile", "FROM alpine:3\nRUN echo hello\n"),
        ("Dockerfile.dev", "FROM alpine:3\n"),
        ("build.Dockerfile", "FROM alpine:3\n"),
        ("Containerfile", "FROM alpine:3\n"),
        ("settings.JSON", r#"{"enabled": true, "count": 2}"#),
        ("settings.jsonc", "// Settings\n{\"enabled\": true}\n"),
        ("settings.YAML", "app:\n  enabled: true\n  name: demo\n"),
        ("compose.yml", "services: {web: {image: nginx}}\n"),
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("Cargo.lock", "version = 4\n[[package]]\nname = \"demo\"\n"),
        (
            "config.xml",
            "<config enabled=\"true\"><name>demo</name></config>",
        ),
    ] {
        let mut highlighter = SyntaxHighlighter::new(preview_language(name).unwrap());
        let rope = Rope::from(source);
        assert!(highlighter.update(None, &rope, None), "{name}");
        assert!(
            highlighter
                .styles(&(0..source.len()), theme.as_ref())
                .iter()
                .any(|(_, style)| *style != HighlightStyle::default()),
            "{name}"
        );
    }
}

fn is_text_file(name: &str) -> bool {
    if preview_language(name).is_some() {
        return true;
    }
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        ".gitignore"
            | ".gitattributes"
            | ".gitibures"
            | ".gitmodules"
            | ".editorconfig"
            | ".env"
            | "makefile"
            | "license"
            | "readme"
            | "yarn.lock"
    ) || name.starts_with(".env.")
        || name.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(
                extension,
                "md" | "markdown"
                    | "txt"
                    | "log"
                    | "js"
                    | "mjs"
                    | "cjs"
                    | "ts"
                    | "mts"
                    | "cts"
                    | "jsx"
                    | "tsx"
                    | "py"
                    | "pyw"
                    | "rs"
                    | "go"
                    | "java"
                    | "kt"
                    | "c"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "cs"
                    | "rb"
                    | "php"
                    | "swift"
                    | "html"
                    | "htm"
                    | "vue"
                    | "svelte"
                    | "css"
                    | "scss"
                    | "less"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "ps1"
                    | "bat"
                    | "cmd"
                    | "ini"
                    | "cfg"
                    | "conf"
                    | "csv"
                    | "tsv"
                    | "sql"
            )
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn text_preview_matches_extensions_and_special_names() {
        for name in [
            "README.MD",
            "notes.txt",
            "main.js",
            "main.ts",
            ".gitignore",
            ".gitattributes",
            ".gitibures",
            "main.py",
            "main.rs",
            "Dockerfile",
        ] {
            assert!(super::is_text_file(name), "{name}");
        }
        for name in ["image.png", "archive.zip", "notes.txt.exe", "document.docx"] {
            assert!(!super::is_text_file(name), "{name}");
        }
    }
}
