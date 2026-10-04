use super::*;
use gpui_kit::{ScrollStrategy, relative, uniform_list};

impl NativeEditor {
    pub(super) fn install_finder(&mut self, finder: Result<Finder>, cx: &mut Context<Self>) {
        match finder {
            Ok(finder) => {
                self.finder = Some(Arc::new(Mutex::new(finder)));
                self.finder_error = None;
            }
            Err(error) => {
                self.finder = None;
                self.finder_error = Some(format!("Could not index files: {error:#}"));
            }
        }
        if self.browser == BrowserMode::Files {
            self.search_files(cx);
        }
    }

    pub(super) fn search_files(&mut self, cx: &mut Context<Self>) {
        self.file_generation.fetch_add(1, Ordering::Relaxed);
        self.preview_generation += 1;
        self.file_selection = 0;
        self.file_matches.clear();
        self.file_total = 0;
        self.preview_text = Some(String::new());
        self.file_searching = false;
        let Some(finder) = self.finder.clone() else {
            cx.notify();
            return;
        };
        let epoch = self.file_generation.clone();
        let generation = epoch.load(Ordering::Relaxed);
        let query = self.file_query.read(cx).value().to_string();
        self.file_searching = true;
        cx.spawn(async move |view, cx| {
            let results = cx
                .background_spawn(async move {
                    if epoch.load(Ordering::Relaxed) != generation {
                        return None;
                    }
                    finder
                        .lock()
                        .expect("file finder lock poisoned")
                        .search(&query, || epoch.load(Ordering::Relaxed) != generation)
                })
                .await;
            let Some(results) = results else {
                return;
            };
            let _ = view.update(cx, |this, cx| {
                if this.file_generation.load(Ordering::Relaxed) != generation
                    || this.browser != BrowserMode::Files
                {
                    return;
                }
                this.file_matches = results.files;
                this.file_total = results.total;
                this.file_searching = false;
                this.finder_scroll.scroll_to_item(0, ScrollStrategy::Top);
                this.load_file_preview(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_file_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_generation += 1;
        let generation = self.preview_generation;
        self.preview_text = Some("Loading preview…".into());
        let Some(file) = self.file_matches.get(self.file_selection).cloned() else {
            self.preview_text = Some(String::new());
            return;
        };
        let root = self.canonical_root.clone();
        cx.spawn(async move |view, cx| {
            let text = cx
                .background_spawn(async move { finder::preview(&root, &file) })
                .await;
            let _ = view.update(cx, |this, cx| {
                if this.preview_generation == generation && this.browser == BrowserMode::Files {
                    this.preview_text = Some(text);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.file_matches.is_empty() {
            return;
        }
        self.file_selection = index.min(self.file_matches.len() - 1);
        self.finder_scroll
            .scroll_to_item(self.file_selection, ScrollStrategy::Nearest);
        self.load_file_preview(cx);
        cx.notify();
    }

    pub(super) fn accept_file(&mut self, cx: &mut Context<Self>) {
        if let Some(file) = self.file_matches.get(self.file_selection) {
            self.request_open(file.path.clone(), None, cx);
            self.browser = self.finder_previous;
            self.file_generation.fetch_add(1, Ordering::Relaxed);
            self.preview_generation += 1;
        }
        cx.notify();
    }

    pub(super) fn close_file_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.browser = self.finder_previous;
        self.file_generation.fetch_add(1, Ordering::Relaxed);
        self.preview_generation += 1;
        self.editor_focus(cx).focus(window, cx);
        cx.notify();
    }

    pub(super) fn render_file_finder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.browser != BrowserMode::Files {
            return None;
        }
        let selected = self.file_matches.get(self.file_selection);
        let status = if self.indexing {
            "Indexing checkout…".to_owned()
        } else if let Some(error) = self.finder_error.as_ref() {
            error.clone()
        } else if self.file_searching {
            "Searching…".to_owned()
        } else if self.file_matches.is_empty() {
            "No matching files".to_owned()
        } else {
            format!("{} / {} matches", self.file_matches.len(), self.file_total)
        };
        let results = uniform_list(
            "native-finder-results",
            self.file_matches.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| {
                        let file = &this.file_matches[index];
                        Button::new(format!("native-file-{index}"))
                            .debug_selector(move || format!("native-file-{}", index))
                            .ghost()
                            .small()
                            .h(px(30.))
                            .w_full()
                            .p_0()
                            .accessibility_label(file.label.clone())
                            .when(index == this.file_selection, |row| row.bg(rgb(0x28374c)))
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .px_3()
                                    .child(div().w(px(10.)).text_color(rgb(0x61afef)).child(
                                        if index == this.file_selection {
                                            "›"
                                        } else {
                                            ""
                                        },
                                    ))
                                    .child(this.file_icon(&file.label))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .text_ellipsis()
                                            .child(file.label.clone()),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_file(index, cx);
                                this.accept_file(cx);
                            }))
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.finder_scroll)
        .flex_1()
        .min_h_0();
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(gpui_kit::rgba(0x00000099))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.close_file_finder(window, cx);
                            }),
                        ),
                )
                .child(
                    v_flex()
                        .id("native-file-finder")
                        .occlude()
                        .debug_selector(|| "native-file-finder".into())
                        .relative()
                        .w(relative(0.92))
                        .h(relative(0.84))
                        .max_w(px(1200.))
                        .min_h_0()
                        .overflow_hidden()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0x3b7182))
                        .bg(rgb(0x101419))
                        .shadow_lg()
                        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            let key = event.keystroke.key.as_str();
                            let control = event.keystroke.modifiers.control;
                            let direction = match key {
                                "arrowdown" | "down" => Some(true),
                                "arrowup" | "up" => Some(false),
                                "n" if control => Some(true),
                                "p" if control => Some(false),
                                _ => None,
                            };
                            if let Some(down) = direction {
                                let index = if down {
                                    this.file_selection + 1
                                } else {
                                    this.file_selection.saturating_sub(1)
                                };
                                this.select_file(index, cx);
                            } else if key == "escape" {
                                this.close_file_finder(window, cx);
                            } else if key == "enter" {
                                this.accept_file(cx);
                            } else {
                                return;
                            }
                            window.prevent_default();
                            cx.stop_propagation();
                        }))
                        .child(
                            h_flex()
                                .h(px(40.))
                                .flex_shrink_0()
                                .px_3()
                                .gap_2()
                                .border_b_1()
                                .border_color(rgb(0x26313b))
                                .child(div().flex_1().text_color(rgb(0x61afef)).child("Find files"))
                                .child(
                                    Button::new("native-finder-refresh")
                                        .label("Refresh")
                                        .ghost()
                                        .small()
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.refresh_files(cx)),
                                        ),
                                )
                                .child(
                                    Button::new("native-finder-close")
                                        .icon(IconName::Close)
                                        .ghost()
                                        .small()
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_file_finder(window, cx)
                                        })),
                                ),
                        )
                        .child(
                            h_flex()
                                .flex_1()
                                .min_h_0()
                                .items_stretch()
                                .child(
                                    v_flex()
                                        .w(relative(0.5))
                                        .min_w_0()
                                        .min_h_0()
                                        .child(
                                            div()
                                                .px_3()
                                                .py_2()
                                                .text_xs()
                                                .text_color(rgb(0x8fa5b8))
                                                .child("RESULTS"),
                                        )
                                        .child(results)
                                        .child(
                                            v_flex()
                                                .flex_shrink_0()
                                                .p_2()
                                                .gap_1()
                                                .border_t_1()
                                                .border_color(rgb(0xb38255))
                                                .child(Input::new(&self.file_query).small())
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(0x8794a2))
                                                        .child(status),
                                                ),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .min_h_0()
                                        .border_l_1()
                                        .border_color(rgb(0x3b7182))
                                        .child(
                                            div()
                                                .px_3()
                                                .py_2()
                                                .text_xs()
                                                .text_ellipsis()
                                                .text_color(rgb(0x8fa5b8))
                                                .child(
                                                    selected
                                                        .map(|file| file.label.clone())
                                                        .unwrap_or("PREVIEW".into()),
                                                ),
                                        )
                                        .child(
                                            div().flex_1().min_h_0().overflow_hidden().child(
                                                Editor::new(&self.preview_editor)
                                                    .readonly(true)
                                                    .tab_index(-1)
                                                    .appearance(false)
                                                    .bordered(false)
                                                    .h_full(),
                                            ),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(rgb(0x8794a2))
                                .child("↑ ↓ / Ctrl+N P  select     Enter  open     Esc  close"),
                        ),
                )
                .into_any_element(),
        )
    }
}
