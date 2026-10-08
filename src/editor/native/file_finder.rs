use super::*;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{ScrollStrategy, relative, uniform_list};

impl NativeEditor {
    pub(super) fn finder_open(&self) -> bool {
        matches!(
            self.browser,
            BrowserMode::Files | BrowserMode::Buffers | BrowserMode::Text
        )
    }

    fn finder_len(&self) -> usize {
        if self.browser == BrowserMode::Text {
            self.search_results.len()
        } else {
            self.file_matches.len()
        }
    }

    pub(super) fn selected_finder_file(&self) -> Option<(ProjectFile, Option<usize>)> {
        if self.browser == BrowserMode::Text {
            self.search_results.get(self.file_selection).map(|m| {
                (
                    ProjectFile {
                        path: m.path.clone(),
                        label: m.label.clone(),
                    },
                    Some(m.line),
                )
            })
        } else {
            self.file_matches
                .get(self.file_selection)
                .cloned()
                .map(|file| (file, None))
        }
    }

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
        } else if self.browser == BrowserMode::Text {
            self.search_project(cx);
        }
    }

    pub(super) fn refresh_buffer_finder(&mut self, cx: &mut Context<Self>) {
        self.buffer_index_generation += 1;
        let generation = self.buffer_index_generation;
        self.buffer_finder = None;
        self.buffer_finder_error = None;
        self.reset_finder_results();
        let files = self
            .tabs
            .iter()
            .filter_map(|path| {
                let relative = path.strip_prefix(&self.canonical_root).ok()?;
                Some(ProjectFile {
                    path: path.clone(),
                    label: relative
                        .components()
                        .map(|part| part.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                })
            })
            .collect::<Vec<_>>();
        crate::review::icons::ensure_tiles(
            files.iter().map(|file| file.label.as_str()),
            &mut self.icon_tiles,
            cx,
        );
        cx.spawn(async move |view, cx| {
            let finder = cx
                .background_spawn(async move { Finder::new(&files) })
                .await;
            let _ = view.update(cx, |this, cx| {
                if this.buffer_index_generation != generation
                    || this.browser != BrowserMode::Buffers
                {
                    return;
                }
                match finder {
                    Ok(finder) => this.buffer_finder = Some(Arc::new(Mutex::new(finder))),
                    Err(error) => {
                        this.buffer_finder_error =
                            Some(format!("Could not index open tabs: {error:#}"))
                    }
                }
                this.search_files(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn reset_finder_results(&mut self) {
        self.file_generation.fetch_add(1, Ordering::Relaxed);
        self.preview_generation += 1;
        self.file_selection = 0;
        self.file_matches.clear();
        self.file_total = 0;
        self.search_results.clear();
        self.search_truncated = false;
        self.search_error = None;
        self.preview_text = Some(finder::Preview::default());
        self.file_searching = false;
    }

    pub(super) fn search_files(&mut self, cx: &mut Context<Self>) {
        let mode = self.browser;
        if !matches!(mode, BrowserMode::Files | BrowserMode::Buffers) {
            return;
        }
        self.reset_finder_results();
        let finder = if mode == BrowserMode::Buffers {
            self.buffer_finder.clone()
        } else {
            self.finder.clone()
        };
        let Some(finder) = finder else {
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
                    || this.browser != mode
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

    pub(super) fn search_project(&mut self, cx: &mut Context<Self>) {
        if self.browser != BrowserMode::Text {
            return;
        }
        self.reset_finder_results();
        let query = self.text_query.read(cx).value().to_string();
        if query.is_empty() || self.indexing {
            cx.notify();
            return;
        }
        let epoch = self.file_generation.clone();
        let generation = epoch.load(Ordering::Relaxed);
        self.file_searching = true;
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            if epoch.load(Ordering::Relaxed) != generation {
                return;
            }
            let Ok(Some((root, mut files))) = view.update(cx, |this, _| {
                (this.file_generation.load(Ordering::Relaxed) == generation
                    && this.browser == BrowserMode::Text)
                    .then(|| (this.canonical_root.clone(), this.files.clone()))
            }) else {
                return;
            };
            let results = cx
                .background_spawn(async move {
                    // The sidebar groups folders first, but grep retains its
                    // alphabetical file priority before applying its limit.
                    files.sort_unstable_by(|a, b| a.label.cmp(&b.label));
                    project::search_text(&root, &files, &query, || {
                        epoch.load(Ordering::Relaxed) != generation
                    })
                })
                .await;
            let Some(results) = results else {
                return;
            };
            let _ = view.update(cx, |this, cx| {
                if this.file_generation.load(Ordering::Relaxed) != generation
                    || this.browser != BrowserMode::Text
                {
                    return;
                }
                this.search_results = results.items;
                this.search_truncated = results.truncated;
                this.search_error = results.error;
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
        self.preview_text = Some("Loading preview…".to_owned().into());
        let Some((file, line)) = self.selected_finder_file() else {
            self.preview_text = Some(finder::Preview::default());
            return;
        };
        if self.browser == BrowserMode::Buffers {
            let document = if self.path.as_ref() == Some(&file.path) {
                Some(&self.editor)
            } else {
                self.inactive_documents
                    .get(&file.path)
                    .map(|document| &document.editor)
            };
            self.preview_text = Some(document.map_or_else(
                || "Buffer is no longer open".to_owned().into(),
                |document| {
                    let state = document.read(cx);
                    let text = state.text();
                    let end = text.floor_char_boundary(text.len().min(40_000));
                    let value = text.slice(..end).to_string();
                    let mut preview = finder::preview_text(&value, None, 0);
                    if end < text.len() && !preview.text.ends_with("… Preview truncated\n") {
                        preview.text.push_str("\n… Preview truncated\n");
                    }
                    preview
                },
            ));
            cx.notify();
            return;
        }
        let column = if self.browser == BrowserMode::Text {
            self.search_results
                .get(self.file_selection)
                .map_or(0, |m| m.column)
        } else {
            0
        };
        let root = self.canonical_root.clone();
        cx.spawn(async move |view, cx| {
            let text = cx
                .background_spawn(async move { finder::preview(&root, &file, line, column) })
                .await;
            let _ = view.update(cx, |this, cx| {
                if this.preview_generation == generation && this.finder_open() {
                    this.preview_text = Some(text);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.finder_len() == 0 {
            return;
        }
        self.file_selection = index.min(self.finder_len() - 1);
        self.finder_scroll
            .scroll_to_item(self.file_selection, ScrollStrategy::Nearest);
        self.load_file_preview(cx);
        cx.notify();
    }

    pub(super) fn accept_file(&mut self, cx: &mut Context<Self>) {
        if let Some((file, line)) = self.selected_finder_file() {
            if self.browser == BrowserMode::Buffers && !self.tabs.contains(&file.path) {
                self.refresh_buffer_finder(cx);
                return;
            }
            self.request_open(file.path, line, cx);
            self.browser = self.finder_previous;
            self.file_generation.fetch_add(1, Ordering::Relaxed);
            self.preview_generation += 1;
            self.preview_text = None;
        }
        cx.notify();
    }

    pub(super) fn close_file_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.browser = self.finder_previous;
        self.file_generation.fetch_add(1, Ordering::Relaxed);
        self.preview_generation += 1;
        self.preview_text = None;
        self.editor_focus(cx).focus(window, cx);
        cx.notify();
    }

    pub(super) fn render_file_finder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.finder_open() {
            return None;
        }
        let grep = self.browser == BrowserMode::Text;
        let buffers = self.browser == BrowserMode::Buffers;
        let selected = self.selected_finder_file();
        let status =
            if buffers && self.buffer_finder.is_none() && self.buffer_finder_error.is_none() {
                "Indexing open tabs…".to_owned()
            } else if buffers && self.buffer_finder_error.is_some() {
                self.buffer_finder_error.clone().unwrap()
            } else if !buffers && self.indexing {
                "Indexing checkout…".to_owned()
            } else if !grep && !buffers && self.finder_error.is_some() {
                self.finder_error.clone().unwrap()
            } else if self.file_searching {
                "Searching…".to_owned()
            } else if grep && self.search_error.is_some() {
                self.search_error.clone().unwrap()
            } else if grep && self.text_query.read(cx).value().is_empty() {
                "Type to search across files (case-insensitive literal text)".to_owned()
            } else if self.finder_len() == 0 {
                if grep {
                    "No matches"
                } else if buffers && self.tabs.is_empty() {
                    "No open tabs"
                } else if buffers {
                    "No matching open tabs"
                } else {
                    "No matching files"
                }
                .to_owned()
            } else if grep {
                format!(
                    "{}{} matches",
                    self.search_results.len(),
                    if self.search_truncated { "+" } else { "" }
                )
            } else {
                format!("{} / {} matches", self.file_matches.len(), self.file_total)
            };
        let results = uniform_list(
            "native-finder-results",
            self.finder_len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| {
                        let (file_label, label, id) = if this.browser == BrowserMode::Text {
                            let item = &this.search_results[index];
                            (
                                item.label.clone(),
                                format!("{}:{}  {}", item.label, item.line, item.preview),
                                format!("native-search-{index}"),
                            )
                        } else {
                            let file = &this.file_matches[index];
                            (
                                file.label.clone(),
                                file.label.clone(),
                                format!("native-file-{index}"),
                            )
                        };
                        Button::new(id.clone())
                            .debug_selector(move || id.clone())
                            .ghost()
                            .small()
                            .h(px(28.))
                            .w_full()
                            .p_0()
                            .accessibility_label(label.clone())
                            .when(index == this.file_selection, |row| {
                                row.bg(cx.theme().accent)
                                    .text_color(cx.theme().accent_foreground)
                            })
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .px_3()
                                    .child(this.file_icon(&file_label))
                                    .child(div().min_w_0().flex_1().text_ellipsis().child(label)),
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
                        .max_h(px(800.))
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .min_h_0()
                        .overflow_hidden()
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().background)
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
                                .h(px(36.))
                                .flex_shrink_0()
                                .px_3()
                                .gap_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(div().flex_1().text_color(cx.theme().foreground).child(
                                    if grep {
                                        "Live grep"
                                    } else if buffers {
                                        "Open tabs"
                                    } else {
                                        "Find files"
                                    },
                                ))
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
                            div()
                                .flex_shrink_0()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(
                                    Input::new(if grep {
                                        &self.text_query
                                    } else {
                                        &self.file_query
                                    })
                                    .small(),
                                ),
                        )
                        .child(
                            h_flex()
                                .flex_1()
                                .min_h_0()
                                .items_stretch()
                                .child(
                                    v_flex()
                                        .w(relative(0.44))
                                        .min_w_0()
                                        .min_h_0()
                                        .child(
                                            div()
                                                .px_3()
                                                .py_2()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child("Results"),
                                        )
                                        .child(results),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .min_h_0()
                                        .border_l_1()
                                        .border_color(cx.theme().border)
                                        .child(
                                            div()
                                                .px_3()
                                                .py_2()
                                                .text_xs()
                                                .text_ellipsis()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(
                                                    selected
                                                        .map(|(file, line)| match line {
                                                            Some(line) => format!(
                                                                "{}:{} · preview from line {}",
                                                                file.label,
                                                                line,
                                                                self.preview_first_line
                                                            ),
                                                            None => file.label,
                                                        })
                                                        .unwrap_or("Preview".into()),
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
                            h_flex()
                                .h(px(30.))
                                .px_3()
                                .gap_3()
                                .border_t_1()
                                .border_color(cx.theme().border)
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(div().flex_1().min_w_0().text_ellipsis().child(status))
                                .child("↑ ↓ select · Enter open · Esc close"),
                        ),
                )
                .into_any_element(),
        )
    }
}
