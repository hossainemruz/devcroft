//! Dedicated Todos board: one kanban column per project plus Unscoped.
//! All writes use Home's stale-write guard.
use super::*;

impl HomeView {
    pub(super) fn todos_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut filters = h_flex().gap_2().flex_wrap().items_center();
        for group in [None, Some(Group::Personal), Some(Group::Work)] {
            let label = group.map_or("All", Group::label);
            let checked = self.todo_group_filter == group;
            filters = filters.child(
                Radio::new(item_id("filter-todo", label))
                    .label(label.to_owned())
                    .checked(checked)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Radios never toggle off: selecting the active one is
                        // a no-op, any other pick becomes the filter.
                        if this.todo_group_filter != group {
                            this.todo_group_filter = group;
                            this.navigation_cursor = None;
                            cx.notify();
                        }
                    })),
            );
        }
        let show_completed = self.show_completed;
        filters = filters.child(
            Checkbox::new("show-completed-todos-page")
                .label("Show completed")
                .checked(show_completed)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    this.show_completed = *checked;
                    this.navigation_cursor = None;
                    cx.notify();
                })),
        );
        let mut board = h_flex().items_stretch().gap_4().flex_wrap();
        // Only columns with visible todos stay on the board — projects
        // without any (under the current group/completion filters) would
        // just be clutter.
        let mut visible_columns = 0;
        for (index, column) in self.todo_board_columns().into_iter().enumerate() {
            let items = self.column_todos(column.key.as_deref());
            if items.is_empty() {
                continue;
            }
            visible_columns += 1;
            let column_key = column.key.clone();
            let column_title = column.title.clone();
            let mut view = v_flex()
                .id(("todo-column", index))
                .flex_1()
                .min_w(px(290.))
                .min_h(px(380.))
                .gap_3()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .on_drop(cx.listener(move |this, drag: &DragTodo, window, cx| {
                    let project = column_key.clone();
                    this.change(window, cx, |data| {
                        data.move_todo_to_project(&drag.id, project.as_deref());
                        Ok(())
                    });
                }))
                .child(
                    h_flex()
                        .justify_between()
                        .gap_2()
                        .child(div().font_semibold().child(column_title.clone()))
                        .child(Tag::secondary().child(items.len().to_string())),
                );
            for item in items {
                view = view.child(self.todo_card(&item, cx));
            }
            let group = self.todo_group_filter.unwrap_or_default();
            let project = column.key.clone().unwrap_or_default();
            view = view.child(
                Button::new(("add-todo-column", index))
                    .ghost()
                    .mt_auto()
                    .label("+ Add todo")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let mut item = Item::new(Kind::Todo);
                        item.group = group;
                        item.project = project.clone();
                        this.editor(item, window, cx);
                    })),
            );
            board = board.child(view);
        }
        if visible_columns == 0 {
            // No column survived the filters: keep an entry point so the
            // board is never a dead end.
            let group = self.todo_group_filter.unwrap_or_default();
            board = board.child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary)
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("No todos match the current filters."),
                    )
                    .child(
                        Button::new("add-todo-empty-board")
                            .ghost()
                            .label("+ Add todo")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let mut item = Item::new(Kind::Todo);
                                item.group = group;
                                this.editor(item, window, cx);
                            })),
                    ),
            );
        }
        v_flex().gap_4()
            .child(filters)
            .child(div().text_sm().text_color(cx.theme().muted_foreground)
                .child("Drag todos between project columns to re-scope them, or reorder within a column. Unscoped holds todos with no project."))
            .when_some(self.error.clone(), |view, error| view.child(div().text_color(cx.theme().danger).child(error)))
            .child(board)
    }
}
