use super::*;

impl Browser {
    pub(super) fn open_remote_picker(
        &self,
        stores: Vec<Store>,
        location: Option<(Store, String)>,
        title: String,
        label: &'static str,
        selectable: impl Fn(&Self, &Store, &str) -> bool + 'static,
        confirm: impl Fn(&mut Self, Store, String, &mut Context<Self>) -> bool + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let owner = cx.weak_entity();
        let picker = cx.new(|cx| {
            let mut picker = SyncRemotePicker {
                owner: owner.clone(),
                runtime: self.runtime.clone(),
                stores,
                location: None,
                folders: Vec::new(),
                error: None,
                loading: false,
                creating: false,
                create_input: None,
                create_error: None,
                request: None,
            };
            picker.browse(location, cx);
            picker
        });
        let confirm = std::rc::Rc::new(confirm);
        window.open_dialog(cx, move |dialog, _, cx| {
            let owner = owner.clone();
            let confirm = confirm.clone();
            let state = picker.read(cx);
            let target = state
                .location
                .as_ref()
                .filter(|(store, prefix)| {
                    !state.loading
                        && state.error.is_none()
                        && state.create_input.is_none()
                        && owner
                            .upgrade()
                            .is_some_and(|owner| selectable(owner.read(cx), store, prefix))
                })
                .cloned();
            dialog
                .title(title.clone())
                .width(px(520.))
                .child(picker.clone())
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("cancel-remote-folder")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-remote-folder")
                                .primary()
                                .label(label)
                                .disabled(target.is_none())
                                .on_click(move |_, window, cx| {
                                    if let Some((store, prefix)) = target.clone()
                                        && owner
                                            .update(cx, |this, cx| confirm(this, store, prefix, cx))
                                            .unwrap_or(false)
                                    {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }
}

struct SyncRemotePicker {
    owner: WeakEntity<Browser>,
    runtime: Arc<tokio::runtime::Runtime>,
    stores: Vec<Store>,
    location: Option<(Store, String)>,
    folders: Vec<Entry>,
    error: Option<SharedString>,
    loading: bool,
    creating: bool,
    create_input: Option<Entity<InputState>>,
    create_error: Option<SharedString>,
    request: Option<Task<()>>,
}

impl SyncRemotePicker {
    fn browse(&mut self, location: Option<(Store, String)>, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        self.create_input = None;
        self.create_error = None;
        self.location = location;
        self.folders.clear();
        self.error = None;
        self.loading = self.location.is_some();
        self.request = self.location.clone().map(|(store, prefix)| {
            let job = self
                .runtime
                .spawn(async move { store.list(&prefix, true).await });
            cx.spawn(async move |view, cx| {
                let result = job
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                let _ = view.update(cx, |this, cx| {
                    this.loading = false;
                    match result {
                        Ok(entries) => {
                            this.folders = entries.into_iter().filter(Entry::directory).collect();
                        }
                        Err(error) => this.error = Some(error.to_string().into()),
                    }
                    cx.notify();
                });
            })
        });
        cx.notify();
    }

    fn create_folder(&mut self, cx: &mut Context<Self>) {
        let (Some((store, prefix)), Some(input)) = (self.location.clone(), &self.create_input)
        else {
            return;
        };
        if self.loading || self.creating {
            return;
        }
        let key = match child_key(&prefix, input.read(cx).value().trim(), true) {
            Ok(key) => key,
            Err(error) => {
                self.create_error = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        if self
            .owner
            .upgrade()
            .is_some_and(|owner| owner.read(cx).write_locked_in(store.bucket(), &key, true))
        {
            self.create_error = Some("该目录正在移动或复制，请等待传输结束".into());
            cx.notify();
            return;
        }
        self.creating = true;
        self.create_error = None;
        let job = self.runtime.spawn({
            let store = store.clone();
            let key = key.clone();
            async move { store.mkdir(key).await }
        });
        cx.spawn(async move |view, cx| {
            let result = job
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let _ = view.update(cx, |this, cx| {
                this.creating = false;
                match result {
                    Ok(()) => this.browse(Some((store, key)), cx),
                    Err(error) => {
                        this.create_error = Some(error.to_string().into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for SyncRemotePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut list = div()
            .id((
                ElementId::from("sync-remote-folders"),
                SharedString::from(
                    self.location
                        .as_ref()
                        .map_or_else(String::new, |(store, prefix)| {
                            format!("{}/{prefix}", store.bucket())
                        }),
                ),
            ))
            .h_64()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        let header = if let Some((store, prefix)) = &self.location {
            let parent =
                (!prefix.is_empty()).then(|| (store.clone(), parent_prefix(prefix).to_owned()));
            let store = store.clone();
            list = list.children(self.folders.iter().map(|entry| {
                let prefix = entry.key().to_owned();
                let store = store.clone();
                Button::new(SharedString::from(prefix.clone()))
                    .small()
                    .ghost()
                    .flex_shrink_0()
                    .w_full()
                    .icon(IconName::Folder)
                    .accessibility_label(entry.name().to_owned())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_left()
                            .child(entry.name().to_owned()),
                    )
                    .disabled(self.creating)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browse(Some((store.clone(), prefix.clone())), cx);
                    }))
            }));
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Button::new("sync-remote-parent")
                        .small()
                        .ghost()
                        .icon(IconName::ArrowUp)
                        .accessibility_label("上一级")
                        .tooltip("上一级")
                        .disabled(self.creating)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.browse(parent.clone(), cx)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(format!("{} /{prefix}", store.bucket())),
                )
                .child(
                    Button::new("new-sync-remote-folder")
                        .small()
                        .ghost()
                        .icon(IconName::FolderPlus)
                        .label("新建文件夹")
                        .disabled(
                            self.loading
                                || self.error.is_some()
                                || self.create_input.is_some()
                                || self.owner.upgrade().is_some_and(|owner| {
                                    owner
                                        .read(cx)
                                        .folder_write_locked_in(store.bucket(), prefix)
                                }),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            let input =
                                cx.new(|cx| InputState::new(window, cx).placeholder("文件夹名称"));
                            input.read(cx).focus_handle(cx).focus(window, cx);
                            this.create_input = Some(input);
                            cx.notify();
                        })),
                )
        } else {
            list = list.children(self.stores.iter().map(|store| {
                let store = store.clone();
                Button::new(SharedString::from(store.bucket().to_owned()))
                    .small()
                    .ghost()
                    .flex_shrink_0()
                    .w_full()
                    .icon(IconName::Database)
                    .accessibility_label(store.bucket().to_owned())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_left()
                            .child(store.bucket().to_owned()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browse(Some((store.clone(), String::new())), cx);
                    }))
            }));
            div().child("选择存储桶")
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(header)
            .when_some(self.create_input.clone(), |view, input| {
                view.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&input).small().disabled(self.creating)),
                        )
                        .child(
                            Button::new("create-sync-remote-folder")
                                .small()
                                .primary()
                                .label(if self.creating {
                                    "正在创建"
                                } else {
                                    "创建"
                                })
                                .disabled(self.creating)
                                .on_click(cx.listener(|this, _, _, cx| this.create_folder(cx))),
                        )
                        .child(
                            Button::new("cancel-sync-remote-folder")
                                .small()
                                .ghost()
                                .label("取消")
                                .disabled(self.creating)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.create_input = None;
                                    this.create_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(self.create_error.clone(), |view, error| {
                view.child(div().text_sm().text_color(theme.danger).child(error))
            })
            .child(
                list.when(self.loading, |list| {
                    list.child(div().p_2().child("正在读取 S3 目录"))
                })
                .when_some(self.error.clone(), |list, error| {
                    list.child(div().p_2().text_color(theme.danger).child(error))
                        .child(
                            Button::new("retry-sync-remote")
                                .small()
                                .label("重试")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.browse(this.location.clone(), cx)
                                })),
                        )
                })
                .when(
                    self.location.is_some()
                        && self.folders.is_empty()
                        && !self.loading
                        && self.error.is_none(),
                    |list| {
                        list.child(
                            div()
                                .p_2()
                                .text_color(theme.muted_foreground)
                                .child("没有子目录"),
                        )
                    },
                ),
            )
    }
}
