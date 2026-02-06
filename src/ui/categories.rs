use crate::app::{App, Focus};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

/// Render the category tree panel.
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    if area.width < 3 || area.height < 3 {
        return;
    }

    let is_focused = app.focus == Focus::Categories;
    // PERF-021: Use cached tree and pass to index lookup to avoid redundant rebuilds.
    let tree = app.category_tree();
    let selected_tree_idx = app.category_tree_selected_index_in(&tree);

    let style_selected = app.style("feed_selected");
    let style_unread = app.style("feed_unread");
    let style_normal = app.style("feed_normal");

    let items: Vec<ListItem> = tree
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let indent = "  ".repeat(item.depth);
            let icon = if item.category_id.is_none() {
                // "All" item
                ""
            } else if item.has_children {
                if item.is_expanded {
                    "v "
                } else {
                    "> "
                }
            } else {
                "  "
            };

            let style = if i == selected_tree_idx {
                style_selected
            } else if item.unread_count > 0 {
                style_unread
            } else {
                style_normal
            };

            let mut spans = Vec::with_capacity(3);
            spans.push(Span::styled(format!("{}{}", indent, icon), style));
            spans.push(Span::styled(&*item.name, style));
            if item.unread_count > 0 {
                spans.push(Span::styled(format!(" ({})", item.unread_count), style));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let border_style = if is_focused {
        app.style("panel_border_focused")
    } else {
        app.style("panel_border")
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Categories"),
        )
        .highlight_style(Style::default());

    let mut state = ListState::default().with_selected(Some(selected_tree_idx));
    f.render_stateful_widget(list, area, &mut state);
}
