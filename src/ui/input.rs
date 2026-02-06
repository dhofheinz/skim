//! Input handling for the TUI.
//!
//! This module processes keyboard input and dispatches to the appropriate
//! handler based on current view and mode.

use crate::app::{
    App, AppEvent, CachedArticleState, ConfirmAction, ContentState, ContextMenuState,
    ContextMenuSubState, FetchResult, Focus, SubscribeState, View, CONTEXT_MENU_ITEMS,
};
use crate::feed::{discover_feed, refresh_all, refresh_one};
use crate::keybindings::{Action as KbAction, Context as KbContext};
use crate::util::validate_url;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::helpers::{
    catch_task_panic, exit_search_mode, exit_starred_mode, restore_articles_from_search,
    try_spawn_content_load, ERR_ARTICLE_NO_URL,
};
use super::Action;
use crate::util::{validate_url_for_open, MAX_SEARCH_QUERY_LENGTH};

/// Map the current focus panel to a keybinding context for context-specific lookups.
fn focus_to_context(focus: Focus) -> KbContext {
    match focus {
        Focus::Categories => KbContext::Categories,
        Focus::Feeds => KbContext::FeedList,
        Focus::Articles => KbContext::ArticleList,
        Focus::WhatsNew => KbContext::WhatsNew,
    }
}

/// Main input dispatch function.
///
/// Routes input to the appropriate handler based on current mode and view.
pub(super) async fn handle_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    // Handle help overlay input first (captures all keys when visible)
    if app.show_help {
        return Ok(handle_help_input(app, code));
    }

    // Handle confirmation dialog input (captures all keys when visible)
    if app.pending_confirm.is_some() {
        return handle_confirm_input(app, code, event_tx);
    }

    // Handle subscribe dialog input (captures all keys when visible)
    if app.subscribe_state.is_some() {
        return handle_subscribe_input(app, code, event_tx).await;
    }

    // Handle context menu input (captures all keys when visible)
    if app.context_menu.is_some() {
        return handle_context_menu_input(app, code, event_tx).await;
    }

    // Handle search mode input separately
    if app.search_mode {
        return handle_search_input(app, code).await;
    }

    match app.view {
        View::Browse => handle_browse_input(app, code, modifiers, event_tx).await,
        View::Reader => handle_reader_input(app, code, modifiers, event_tx),
    }
}

/// Handle input while the help overlay is visible.
///
/// Captures all keys: j/k/Up/Down scroll, Esc/q/? dismiss.
fn handle_help_input(app: &mut App, code: KeyCode) -> Action {
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
            app.show_help = false;
            app.help_scroll_offset = 0;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.help_scroll_offset = app.help_scroll_offset.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.help_scroll_offset = app.help_scroll_offset.saturating_sub(1);
        }
        _ => {}
    }
    Action::Continue
}

/// Handle input in browse view (feeds + articles panels).
async fn handle_browse_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    let context = focus_to_context(app.focus);
    let action = app.keybindings.action_for_key(code, modifiers, context);

    match action {
        Some(KbAction::Quit) => return Ok(Action::Quit),
        Some(KbAction::Back) => {
            // Priority: Dismiss What's New panel first, then exit starred mode
            if app.show_whats_new {
                app.dismiss_whats_new();
            } else {
                exit_starred_mode(app, "ESC").await?;
            }
        }
        Some(KbAction::NavDown) => app.nav_down(),
        Some(KbAction::NavUp) => app.nav_up(),
        Some(KbAction::CycleFocus) => {
            let has_cats = app.show_categories;
            let has_wn = app.show_whats_new && !app.whats_new.is_empty();
            app.focus = match (has_wn, has_cats, app.focus) {
                // With What's New + Categories: WN → Cat → Feeds → Articles → WN
                (true, true, Focus::WhatsNew) => Focus::Categories,
                (true, true, Focus::Categories) => Focus::Feeds,
                (true, true, Focus::Feeds) => Focus::Articles,
                (true, true, Focus::Articles) => Focus::WhatsNew,
                // With What's New only: WN → Feeds → Articles → WN
                (true, false, Focus::WhatsNew) => Focus::Feeds,
                (true, false, Focus::Feeds) => Focus::Articles,
                (true, false, Focus::Articles) => Focus::WhatsNew,
                (true, false, Focus::Categories) => Focus::Feeds,
                // With Categories only: Cat → Feeds → Articles → Cat
                (false, true, Focus::Categories) => Focus::Feeds,
                (false, true, Focus::Feeds) => Focus::Articles,
                (false, true, Focus::Articles) => Focus::Categories,
                (false, true, Focus::WhatsNew) => Focus::Categories,
                // Neither: Feeds → Articles → Feeds
                (false, false, Focus::Feeds) => Focus::Articles,
                (false, false, Focus::Articles) => Focus::Feeds,
                (false, false, _) => Focus::Feeds,
            };
        }
        Some(KbAction::ToggleCategories) => {
            app.show_categories = !app.show_categories;
            if !app.show_categories && app.focus == Focus::Categories {
                app.focus = Focus::Feeds;
            }
            app.needs_redraw = true;
        }
        Some(KbAction::CollapseCategory) => {
            if app.focus == Focus::Categories {
                if let Some(cat_id) = app.selected_category_id() {
                    if !app.collapsed_categories.contains(&cat_id) {
                        app.toggle_category_collapse(cat_id);
                        app.needs_redraw = true;
                    }
                }
            }
        }
        Some(KbAction::ExpandCategory) => {
            if app.focus == Focus::Categories {
                if let Some(cat_id) = app.selected_category_id() {
                    if app.collapsed_categories.contains(&cat_id) {
                        app.toggle_category_collapse(cat_id);
                        app.needs_redraw = true;
                    }
                }
            }
        }
        Some(KbAction::Select) => {
            handle_enter_key(app, event_tx).await?;
        }
        Some(KbAction::RefreshAll) => {
            handle_refresh_all(app, event_tx).await;
        }
        Some(KbAction::RefreshOne) => {
            handle_refresh_one(app, event_tx).await;
        }
        Some(KbAction::ToggleStar) => {
            handle_star_toggle_browse(app, event_tx).await;
        }
        Some(KbAction::OpenInBrowser) => {
            if let Some(article) = app.selected_article() {
                if let Some(url) = &article.url {
                    // SEC: Validate URL before open::that() to prevent command injection
                    if let Err(e) = validate_url_for_open(url) {
                        app.set_status(e);
                    } else if let Err(e) = open::that(&**url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    }
                } else {
                    app.set_status(ERR_ARTICLE_NO_URL);
                }
            }
        }
        Some(KbAction::OpenFeedSite) => {
            // Open feed website (html_url from OPML)
            if let Some(feed) = app.selected_feed() {
                if let Some(url) = &feed.html_url {
                    // SEC: Validate URL before open::that() to prevent command injection
                    if let Err(e) = validate_url_for_open(url) {
                        app.set_status(e);
                    } else if let Err(e) = open::that(url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    } else {
                        app.set_status(format!("Opening {} website...", feed.title));
                    }
                } else {
                    app.set_status("No website URL for this feed");
                }
            }
        }
        Some(KbAction::EnterSearch) => {
            // PERF-008: Cache current state before entering search mode
            // Arc::clone is O(1) - just increments reference count
            app.cached_articles = Some(CachedArticleState {
                feed_id: app.selected_feed().map(|f| f.id),
                articles: Arc::clone(&app.articles),
                selected: app.selected_article,
            });
            app.search_mode = true;
            app.search_input.clear();
            app.search_feed_id = app.selected_feed().map(|f| f.id);
        }
        Some(KbAction::ToggleStarredMode) => {
            handle_starred_mode_toggle(app).await?;
        }
        Some(KbAction::MarkFeedRead) => {
            handle_mark_feed_read(app, event_tx).await;
        }
        Some(KbAction::MarkAllRead) => {
            handle_mark_all_read(app, event_tx).await;
        }
        Some(KbAction::ExportOpml) => {
            handle_export_opml(app, event_tx);
        }
        Some(KbAction::CycleTheme) => {
            let name = app.cycle_theme();
            app.set_status(format!("Theme: {}", name));
        }
        Some(KbAction::ShowHelp) => {
            app.show_help = true;
            app.help_scroll_offset = 0;
        }
        Some(KbAction::DeleteFeed) => {
            if let Some(feed) = app.selected_feed() {
                let feed_id = feed.id;
                let title = feed.title.to_string();
                app.pending_confirm = Some(ConfirmAction::DeleteFeed { feed_id, title });
            }
        }
        Some(KbAction::Subscribe) => {
            app.subscribe_state = Some(SubscribeState::InputUrl {
                input: String::new(),
            });
        }
        Some(KbAction::ContextMenu) => {
            if let Some(feed) = app.selected_feed() {
                app.context_menu = Some(ContextMenuState {
                    feed_id: feed.id,
                    feed_title: feed.title.to_string(),
                    feed_url: feed.url.clone(),
                    feed_html_url: feed.html_url.clone(),
                    selected_item: 0,
                    sub_state: ContextMenuSubState::MainMenu,
                });
            }
        }
        _ => {}
    }
    Ok(Action::Continue)
}

/// Handle input while the context menu is visible.
async fn handle_context_menu_input(
    app: &mut App,
    code: KeyCode,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    // Take ownership temporarily to match on sub_state
    let mut menu = match app.context_menu.take() {
        Some(m) => m,
        None => return Ok(Action::Continue),
    };

    match menu.sub_state {
        ContextMenuSubState::MainMenu => match code {
            KeyCode::Char('k') | KeyCode::Up => {
                menu.selected_item = menu.selected_item.saturating_sub(1);
                app.context_menu = Some(menu);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                menu.selected_item = (menu.selected_item + 1).min(CONTEXT_MENU_ITEMS.len() - 1);
                app.context_menu = Some(menu);
            }
            KeyCode::Enter => {
                match menu.selected_item {
                    0 => {
                        // Rename: transition to Renaming sub-state
                        menu.sub_state = ContextMenuSubState::Renaming {
                            input: menu.feed_title.clone(),
                        };
                        app.context_menu = Some(menu);
                    }
                    1 => {
                        // Move to Category: transition to CategoryPicker
                        menu.sub_state = ContextMenuSubState::CategoryPicker { selected: 0 };
                        app.context_menu = Some(menu);
                    }
                    2 => {
                        // Delete: close menu, delegate to confirmation flow
                        app.pending_confirm = Some(ConfirmAction::DeleteFeed {
                            feed_id: menu.feed_id,
                            title: menu.feed_title,
                        });
                        // context_menu is already None from take()
                    }
                    3 => {
                        // Refresh: close menu, spawn refresh_one
                        // context_menu is already None from take()
                        handle_refresh_one(app, event_tx).await;
                    }
                    4 => {
                        // Open in Browser: validate URL, open
                        // context_menu is already None from take()
                        let url = menu.feed_html_url.as_deref().unwrap_or(&menu.feed_url);
                        // SEC: Validate URL before open::that() to prevent command injection
                        if let Err(e) = validate_url_for_open(url) {
                            app.set_status(e);
                        } else if let Err(e) = open::that(url) {
                            app.set_status(format!("Failed to open browser: {}", e));
                        } else {
                            app.set_status(format!("Opening {}...", menu.feed_title));
                        }
                    }
                    _ => {
                        app.context_menu = Some(menu);
                    }
                }
            }
            KeyCode::Esc => {
                // Cancel — context_menu is already None from take()
            }
            _ => {
                app.context_menu = Some(menu);
            }
        },
        ContextMenuSubState::Renaming { ref mut input } => match code {
            KeyCode::Char(c) => {
                // SEC-017: Cap rename input length to prevent memory abuse
                if input.len() < 256 {
                    input.push(c);
                }
                app.context_menu = Some(menu);
            }
            KeyCode::Backspace => {
                input.pop();
                app.context_menu = Some(menu);
            }
            KeyCode::Enter => {
                let new_title = input.trim().to_owned();
                if new_title.is_empty() {
                    app.set_status("Name cannot be empty");
                    app.context_menu = Some(menu);
                } else {
                    // Spawn rename task
                    let feed_id = menu.feed_id;
                    let db = app.db.clone();
                    let tx = event_tx.clone();
                    let title = new_title.clone();
                    tokio::spawn(async move {
                        match db.rename_feed(feed_id, &title).await {
                            Ok(()) => {
                                let _ = tx
                                    .send(AppEvent::FeedRenamed {
                                        feed_id,
                                        new_title: title,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(AppEvent::FeedRenameFailed {
                                        feed_id,
                                        error: e.to_string(),
                                    })
                                    .await;
                            }
                        }
                    });
                    // context_menu is already None from take()
                }
            }
            KeyCode::Esc => {
                // Return to main menu
                menu.sub_state = ContextMenuSubState::MainMenu;
                app.context_menu = Some(menu);
            }
            _ => {
                app.context_menu = Some(menu);
            }
        },
        ContextMenuSubState::CategoryPicker { ref mut selected } => {
            // Category list: index 0 = "Uncategorized", then each category
            let cat_count = app.categories.len() + 1; // +1 for Uncategorized
            match code {
                KeyCode::Char('k') | KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    app.context_menu = Some(menu);
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    *selected = (*selected + 1).min(cat_count.saturating_sub(1));
                    app.context_menu = Some(menu);
                }
                KeyCode::Enter => {
                    let selected_idx = *selected;
                    let feed_id = menu.feed_id;
                    let (category_id, category_name) = if selected_idx == 0 {
                        (None, "Uncategorized".to_string())
                    } else {
                        let cat = &app.categories[selected_idx - 1];
                        (Some(cat.id), cat.name.clone())
                    };

                    let db = app.db.clone();
                    let tx = event_tx.clone();
                    let cat_name = category_name.clone();
                    tokio::spawn(async move {
                        match db.move_feed_to_category(feed_id, category_id).await {
                            Ok(()) => {
                                let _ = tx
                                    .send(AppEvent::FeedMoved {
                                        feed_id,
                                        category_id,
                                        category_name: cat_name,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(AppEvent::FeedMoveFailed {
                                        feed_id,
                                        error: e.to_string(),
                                    })
                                    .await;
                            }
                        }
                    });
                    // context_menu is already None from take()
                }
                KeyCode::Esc => {
                    // Return to main menu
                    menu.sub_state = ContextMenuSubState::MainMenu;
                    app.context_menu = Some(menu);
                }
                _ => {
                    app.context_menu = Some(menu);
                }
            }
        }
    }
    Ok(Action::Continue)
}

/// Handle input while the subscribe dialog is visible.
async fn handle_subscribe_input(
    app: &mut App,
    code: KeyCode,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    // Take ownership temporarily to match on state
    let state = app.subscribe_state.take();
    match state {
        Some(SubscribeState::InputUrl { mut input }) => match code {
            KeyCode::Char(c) => {
                // SEC-017: Cap URL input length to prevent memory abuse from held keys
                if input.len() < 2048 {
                    input.push(c);
                }
                app.subscribe_state = Some(SubscribeState::InputUrl { input });
            }
            KeyCode::Backspace => {
                input.pop();
                app.subscribe_state = Some(SubscribeState::InputUrl { input });
            }
            KeyCode::Enter => {
                let url = input.trim().to_owned();
                if url.is_empty() {
                    app.subscribe_state = Some(SubscribeState::InputUrl { input });
                    return Ok(Action::Continue);
                }
                // Validate URL before discovery
                if let Err(e) = validate_url(&url) {
                    app.set_status(format!("Invalid URL: {}", e));
                    app.subscribe_state = Some(SubscribeState::InputUrl { input });
                    return Ok(Action::Continue);
                }
                // Transition to Discovering and spawn background task
                app.subscribe_state = Some(SubscribeState::Discovering { url: url.clone() });
                let client = app.http_client.clone();
                let tx = event_tx.clone();
                tokio::spawn(async move {
                    let result = discover_feed(&client, &url).await;
                    let _ = tx
                        .send(AppEvent::FeedDiscovered {
                            url: url.clone(),
                            result: result.map_err(|e| e.to_string()),
                        })
                        .await;
                });
            }
            KeyCode::Esc => {
                // Cancel — subscribe_state is already None from take()
            }
            _ => {
                app.subscribe_state = Some(SubscribeState::InputUrl { input });
            }
        },
        Some(SubscribeState::Discovering { url }) => {
            if code == KeyCode::Esc {
                // Cancel — ignore result when it arrives (state is already None)
                app.set_status("Subscribe cancelled");
            } else {
                // Keep state; only Esc cancels during discovery
                app.subscribe_state = Some(SubscribeState::Discovering { url });
            }
        }
        Some(SubscribeState::Preview { feed }) => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                // Confirm subscription — insert feed into DB
                let title = feed.title.clone();
                let feed_url = feed.feed_url.clone();
                let site_url = feed.site_url.clone();
                let db = app.db.clone();
                let tx = event_tx.clone();
                tokio::spawn(async move {
                    match db.insert_feed(&feed_url, &title, site_url.as_deref()).await {
                        Ok(_) => {
                            let _ = tx.send(AppEvent::FeedSubscribed { title }).await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(AppEvent::FeedSubscribeFailed {
                                    error: e.to_string(),
                                })
                                .await;
                        }
                    }
                });
                // subscribe_state is already None from take()
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.set_status("Subscribe cancelled");
                // subscribe_state is already None from take()
            }
            _ => {
                app.subscribe_state = Some(SubscribeState::Preview { feed });
            }
        },
        None => {
            // Should not happen; defensive
        }
    }
    Ok(Action::Continue)
}

/// Handle input while the confirmation dialog is visible.
///
/// y/Y confirms the action, n/N/Esc cancels.
fn handle_confirm_input(
    app: &mut App,
    code: KeyCode,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Some(ConfirmAction::DeleteFeed { feed_id, title }) = app.pending_confirm.take() {
                app.set_status(format!("Deleting {}...", title));

                let db = app.db.clone();
                let tx = event_tx.clone();
                tokio::spawn(async move {
                    let tx_panic = tx.clone();
                    match catch_task_panic(async {
                        match db.delete_feed(feed_id).await {
                            Ok(articles_removed) => {
                                if let Err(e) = tx
                                    .send(AppEvent::FeedDeleted {
                                        feed_id,
                                        title,
                                        articles_removed,
                                    })
                                    .await
                                {
                                    tracing::warn!(error = %e, event = "FeedDeleted", "Channel send failed (receiver dropped)");
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, feed_id, "Failed to delete feed");
                                if let Err(e) = tx
                                    .send(AppEvent::FeedDeleteFailed {
                                        feed_id,
                                        error: e.to_string(),
                                    })
                                    .await
                                {
                                    tracing::warn!(error = %e, event = "FeedDeleteFailed", "Channel send failed (receiver dropped)");
                                }
                            }
                        }
                    })
                    .await
                    {
                        Ok(()) => {}
                        Err(panic_msg) => {
                            tracing::error!(task = "delete_feed", feed_id, error = %panic_msg, "Background task panicked");
                            let _ = tx_panic
                                .send(AppEvent::TaskPanicked {
                                    task: "delete_feed",
                                    error: panic_msg,
                                })
                                .await;
                        }
                    }
                });
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.pending_confirm = None;
            app.set_status("Cancelled");
        }
        _ => {} // Ignore other keys
    }
    Ok(Action::Continue)
}

/// Handle Enter key press in browse view.
async fn handle_enter_key(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) -> Result<()> {
    if app.focus == Focus::WhatsNew {
        // PERF-023: Fetch full Article from DB for reader entry
        let entry = app.whats_new.get(app.whats_new_selected).or_else(|| {
            // Stale selection index — fallback to first entry
            app.whats_new.first()
        });

        if let Some(entry) = entry {
            let article_id = entry.article_id;
            match app.db.get_article_by_id(article_id).await? {
                Some(article) => {
                    app.view = View::Reader;
                    app.scroll_offset = 0;
                    app.content_state = ContentState::Loading { article_id };
                    app.reader_article = Some(article.clone());
                    app.db.mark_article_read(article_id).await?;

                    try_spawn_content_load(app, &article, event_tx);
                }
                None => {
                    app.set_status("Article no longer exists");
                }
            }
        } else {
            app.set_status("No new articles");
        }
    } else if app.focus == Focus::Categories {
        // Category selected → filter feeds, move focus to Feeds panel
        app.focus = Focus::Feeds;
        app.selected_feed = 0;
        app.selected_article = 0;
        app.needs_redraw = true;
    } else if app.focus == Focus::Feeds {
        // Load articles for selected feed
        if let Some(feed) = app.selected_feed() {
            let feed_id = feed.id;
            // PERF-008: Invalidate cache when feed selection changes
            app.cached_articles = None;
            // BUG-015: Update search_feed_id when switching feeds during search mode
            if app.search_mode {
                app.search_feed_id = Some(feed_id);
            }
            app.articles = Arc::new(app.db.get_articles_for_feed(feed_id, None).await?);
            app.selected_article = 0;
            app.clamp_selections();
            app.focus = Focus::Articles;
        }
    } else if app.focus == Focus::Articles {
        // Enter reader view and spawn content loading task
        if let Some(article) = app.enter_reader() {
            app.db.mark_article_read(article.id).await?;

            // Spawn content loading task
            try_spawn_content_load(app, &article, event_tx);
        }
    }
    Ok(())
}

/// Handle refresh all feeds (r key).
async fn handle_refresh_all(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    // Prevent multiple concurrent refreshes
    if app.refresh_progress.is_some() {
        app.set_status("Refresh already in progress");
    } else if app.feeds.is_empty() {
        app.set_status("No feeds to refresh");
    } else {
        app.set_status("Refreshing feeds...");
        app.refresh_progress = Some((0, app.feeds.len()));

        // Clone what we need for the background task
        // PERF-011: Arc::clone is O(1) - just increments reference count
        let db = app.db.clone();
        let client = app.http_client.clone();
        let feeds = Arc::clone(&app.feeds);
        let tx = event_tx.clone();

        // Create progress channel with proper lifecycle management
        // RES-001: Clone sender for task, drop original immediately
        // This ensures receiver terminates when task completes (success or panic)
        let (progress_tx, mut progress_rx) = mpsc::channel::<(usize, usize)>(32);
        let progress_tx_for_task = progress_tx.clone();
        drop(progress_tx); // Drop original - only task has sender now

        // Spawn refresh task
        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                // Clone tx for rate limit events
                let rate_limit_tx = tx.clone();

                // Spawn the refresh in a separate task
                let mut refresh_handle = tokio::spawn(async move {
                    refresh_all(db, client, feeds, progress_tx_for_task, Some(rate_limit_tx))
                        .await
                });

                // Forward progress updates with timeout safety net
                // RES-001: Receiver loop terminates when sender drops OR timeout
                // B-2: Also monitor refresh_handle to detect panics immediately
                let mut handle_result = None;
                loop {
                    tokio::select! {
                        Some((done, total)) = progress_rx.recv() => {
                            if let Err(e) = tx.send(AppEvent::RefreshProgress(done, total)).await {
                                tracing::warn!(error = %e, event = "RefreshProgress", "Channel send failed (receiver dropped)");
                            }
                        }
                        result = &mut refresh_handle => {
                            // B-2: Refresh task completed (success or panic) — break immediately
                            if let Err(ref e) = result {
                                tracing::warn!(error = %e, "Refresh task panicked");
                            }
                            handle_result = Some(result);
                            break;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            tracing::warn!("Progress receiver timed out after 30s");
                            break;
                        }
                        else => break, // Channel closed (sender dropped)
                    }
                }

                // Get results and send completion
                // B-2: Use cached result if handle completed in select, otherwise await
                let join_result = match handle_result {
                    Some(r) => r,
                    None => refresh_handle.await,
                };
                if let Ok(results) = join_result {
                    let fetch_results: Vec<FetchResult> = results
                        .into_iter()
                        .map(|r| {
                            let (new_articles, error) = match r.result {
                                Ok(count) => (count, None),
                                Err(e) => (0, Some(e.to_string())),
                            };
                            FetchResult {
                                feed_id: r.feed_id,
                                new_articles,
                                error,
                            }
                        })
                        .collect();
                    if let Err(e) = tx.send(AppEvent::RefreshComplete(fetch_results)).await {
                        tracing::warn!(error = %e, event = "RefreshComplete", "Channel send failed (receiver dropped)");
                    }
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "refresh", error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "refresh",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    }
}

/// Handle refresh single feed (Shift+R).
async fn handle_refresh_one(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    if app.refresh_progress.is_some() {
        app.set_status("Refresh already in progress");
    } else if let Some(feed) = app.selected_feed().cloned() {
        app.set_status(format!("Refreshing {}...", feed.title));
        app.refresh_progress = Some((0, 1));

        let db = app.db.clone();
        let client = app.http_client.clone();
        let tx = event_tx.clone();

        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                let result = refresh_one(&db, &client, &feed, Some(&tx)).await;

                // Send progress complete
                if let Err(e) = tx.send(AppEvent::RefreshProgress(1, 1)).await {
                    tracing::warn!(error = %e, event = "RefreshProgress", "Channel send failed (receiver dropped)");
                }

                // Convert to FetchResult for completion event
                let (new_articles, error): (usize, Option<String>) = match result.result {
                    Ok(count) => (count, None),
                    Err(e) => (0, Some(e.to_string())),
                };
                let fetch_result = FetchResult {
                    feed_id: result.feed_id,
                    new_articles,
                    error,
                };
                if let Err(e) = tx.send(AppEvent::RefreshComplete(vec![fetch_result])).await {
                    tracing::warn!(error = %e, event = "RefreshComplete", "Channel send failed (receiver dropped)");
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "refresh_one", error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "refresh_one",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    } else {
        app.set_status("No feed selected");
    }
}

/// Handle star toggle in browse view.
async fn handle_star_toggle_browse(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    if let Some(article) = app.selected_article() {
        let article_id = article.id;
        let current_starred = article.starred;

        // SAFETY: All Arc::make_mut calls on app.articles must happen on the event loop
        // thread (never in spawned tasks). Background tasks may hold Arc clones for reading
        // but must never mutate. This ensures sequential access without locks.
        // PERF-015: Use Arc::make_mut for copy-on-write optimization
        // Only clones the Vec if there are other references, otherwise mutates in place
        let articles = Arc::make_mut(&mut app.articles);
        if let Some(article) = articles.iter_mut().find(|a| a.id == article_id) {
            article.starred = !current_starred;
        }
        // PERF-023: WhatsNewEntry is display-only; no starred field to update
        app.needs_redraw = true;

        // Invalidate cache if it exists - prevents stale data on search/starred mode exit
        if app.cached_articles.is_some() {
            tracing::debug!(article_id, "Invalidating article cache due to star toggle");
            app.cached_articles = None;
        }

        // Spawn background task to persist change
        let db = app.db.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                match db.toggle_article_starred(article_id).await {
                    Ok(new_status) => {
                        if let Err(e) = tx
                            .send(AppEvent::StarToggled {
                                article_id,
                                starred: new_status,
                            })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggled", "Channel send failed (receiver dropped)");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, article_id, "Failed to toggle star");
                        if let Err(e) = tx
                            .send(AppEvent::StarToggleFailed { article_id, original_status: current_starred })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggleFailed", "Channel send failed (receiver dropped)");
                        }
                    }
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "star_toggle", article_id, error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "star_toggle",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    }
}

/// Handle mark all articles read for current feed (a key).
async fn handle_mark_feed_read(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    let Some(feed) = app.selected_feed() else {
        return;
    };
    let feed_id = feed.id;

    // Optimistic UI: mark all visible articles as read
    let articles = Arc::make_mut(&mut app.articles);
    for article in articles.iter_mut() {
        if article.feed_id == feed_id {
            article.read = true;
        }
    }
    // PERF-023: WhatsNewEntry is display-only; no read field to update
    app.cached_articles = None;
    app.needs_redraw = true;

    // Spawn background task to persist
    let db = app.db.clone();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            match db.mark_all_read_for_feed(feed_id).await {
                Ok(count) => {
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadComplete {
                            feed_id: Some(feed_id),
                            count,
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadComplete", "Channel send failed (receiver dropped)");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, feed_id, "Failed to mark feed read");
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadFailed {
                            feed_id: Some(feed_id),
                            error: e.to_string(),
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadFailed", "Channel send failed (receiver dropped)");
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "mark_feed_read", feed_id, error = %panic_msg, "Background task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "mark_feed_read",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    });
}

/// Handle mark all articles read across all feeds (A key).
async fn handle_mark_all_read(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    // Optimistic UI: mark all visible articles as read
    let articles = Arc::make_mut(&mut app.articles);
    for article in articles.iter_mut() {
        article.read = true;
    }
    // PERF-023: WhatsNewEntry is display-only; no read field to update
    app.cached_articles = None;
    app.needs_redraw = true;

    // Spawn background task to persist
    let db = app.db.clone();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            match db.mark_all_read().await {
                Ok(count) => {
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadComplete {
                            feed_id: None,
                            count,
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadComplete", "Channel send failed (receiver dropped)");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to mark all read");
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadFailed {
                            feed_id: None,
                            error: e.to_string(),
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadFailed", "Channel send failed (receiver dropped)");
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "mark_all_read", error = %panic_msg, "Background task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "mark_all_read",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    });
}

/// Handle OPML export (e key in feed panel).
fn handle_export_opml(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    app.set_status("Exporting feeds...");

    // Snapshot feeds and categories from in-memory state (Arc::clone is O(1))
    let feeds = Arc::clone(&app.feeds);
    let categories = Arc::clone(&app.categories);
    let count = feeds.len();
    let tx = event_tx.clone();

    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            // Determine export path
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            let config_dir = std::path::PathBuf::from(&home).join(".config/skim");
            let export_path = config_dir.join("feeds-export.opml");

            // S-8: Verify config directory exists and is within expected location
            if let Ok(canonical_config) = config_dir.canonicalize() {
                if !export_path.starts_with(&canonical_config) {
                    let _ = tx
                        .send(AppEvent::ExportFailed {
                            error: "Export path is outside config directory".to_string(),
                        })
                        .await;
                    return;
                }
            }

            let result = crate::feed::export_to_file_with_categories(
                &feeds,
                &categories,
                &export_path,
            );

            match result {
                Ok(()) => {
                    let path_str = export_path.display().to_string();
                    if let Err(e) = tx
                        .send(AppEvent::ExportComplete {
                            count,
                            path: path_str,
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "ExportComplete", "Channel send failed (receiver dropped)");
                    }
                }
                Err(e) => {
                    if let Err(e) = tx
                        .send(AppEvent::ExportFailed {
                            error: e.to_string(),
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "ExportFailed", "Channel send failed (receiver dropped)");
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "export_opml", error = %panic_msg, "Background task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "export_opml",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    });
}

/// Handle starred mode toggle (S key).
async fn handle_starred_mode_toggle(app: &mut App) -> Result<()> {
    if app.starred_mode {
        exit_starred_mode(app, "S toggle").await?;
    } else {
        // Enter starred mode - cache current state first
        // PERF-008: Cache current state before entering starred mode
        // Arc::clone is O(1) - just increments reference count
        app.cached_articles = Some(CachedArticleState {
            feed_id: app.selected_feed().map(|f| f.id),
            articles: Arc::clone(&app.articles),
            selected: app.selected_article,
        });

        match app.db.get_starred_articles().await {
            Ok(starred) => {
                tracing::info!(starred_count = starred.len(), "Entering starred mode");
                app.starred_mode = true;

                // PERF-014: Build prefix cache for starred mode display
                // Pre-compute formatted feed prefixes to avoid N allocations per render frame
                app.feed_prefix_cache.clear();
                for article in &starred {
                    if !app.feed_prefix_cache.contains_key(&article.feed_id) {
                        if let Some(feed_title) = app.feed_title_cache.get(&article.feed_id) {
                            let prefix =
                                format!("[{}] ", crate::util::truncate_to_width(feed_title, 15));
                            app.feed_prefix_cache.insert(article.feed_id, prefix);
                        }
                    }
                }

                app.articles = Arc::new(starred);
                app.selected_article = 0;
                app.clamp_selections();
                app.focus = Focus::Articles;
            }
            Err(e) => {
                // Failed to enter starred mode, clear cache
                app.cached_articles = None;
                app.set_status(format!("Failed to load starred: {}", e));
            }
        }
    }
    Ok(())
}

/// Handle input in reader view.
///
/// Uses keybinding registry for action dispatch with Reader context.
pub(super) fn handle_reader_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    let action = app
        .keybindings
        .action_for_key(code, modifiers, KbContext::Reader);

    match action {
        Some(KbAction::Quit) => return Ok(Action::Quit),
        Some(KbAction::ExitReader) => {
            app.exit_reader();
        }
        Some(KbAction::ScrollDown) => {
            app.scroll_down(1);
            app.clamp_reader_scroll();
        }
        Some(KbAction::ScrollUp) => app.scroll_up(1),
        Some(KbAction::PageDown) => {
            app.scroll_down(20);
            app.clamp_reader_scroll();
        }
        Some(KbAction::PageUp) => app.scroll_up(20),
        Some(KbAction::ToggleStar) => {
            handle_star_toggle_reader(app, event_tx);
        }
        Some(KbAction::OpenInBrowser) => {
            if let Some(article) = app.reader_article.as_ref() {
                if let Some(url) = &article.url {
                    // SEC: Validate URL before open::that() to prevent command injection
                    if let Err(e) = validate_url_for_open(url) {
                        app.set_status(e);
                    } else if let Err(e) = open::that(&**url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(Action::Continue)
}

/// Handle star toggle in reader view.
fn handle_star_toggle_reader(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    if let Some(article) = app.reader_article.as_mut() {
        let article_id = article.id;
        let original_starred = article.starred;

        // Optimistic UI update in reader
        article.starred = !original_starred;
        app.needs_redraw = true;

        let db = app.db.clone();
        let tx = event_tx.clone();

        // Invalidate cache if it exists - prevents stale data on search/starred mode exit
        if app.cached_articles.is_some() {
            tracing::debug!(
                article_id,
                "Invalidating article cache due to star toggle in reader"
            );
            app.cached_articles = None;
        }

        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                match db.toggle_article_starred(article_id).await {
                    Ok(new_status) => {
                        // Send authoritative new status from DB
                        if let Err(e) = tx
                            .send(AppEvent::StarToggled {
                                article_id,
                                starred: new_status,
                            })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggled", "Channel send failed (receiver dropped)");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, article_id, "Failed to toggle star");
                        if let Err(e) = tx
                            .send(AppEvent::StarToggleFailed { article_id, original_status: original_starred })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggleFailed", "Channel send failed (receiver dropped)");
                        }
                    }
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "star_toggle_reader", article_id, error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "star_toggle_reader",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    }
}

/// Handle input in search mode.
async fn handle_search_input(app: &mut App, code: KeyCode) -> Result<Action> {
    match code {
        KeyCode::Esc => {
            exit_search_mode(app).await?;
        }
        KeyCode::Enter => {
            // Cancel any pending debounce - explicit search takes priority
            // Must clear BEFORE search execution to prevent race with tick handler
            app.search_debounce = None;
            app.search_mode = false;
            // PERF-006: Execute pending search immediately on Enter
            // Use pending_search if available, otherwise use current search_input
            let query = app
                .pending_search
                .take()
                .unwrap_or_else(|| app.search_input.clone());
            if !query.is_empty() {
                // Committing to search results - clear cache
                app.cached_articles = None;
                app.articles = Arc::new(app.db.search_articles(&query).await?);
                app.selected_article = 0;
                app.clamp_selections();
            } else {
                // Empty query - restore original feed articles from cache or DB
                restore_articles_from_search(app, false).await?;
            }
            app.search_feed_id = None; // Clear after use
        }
        KeyCode::Backspace => {
            app.search_input.pop();
            // PERF-006: Set debounce instead of immediate search
            app.search_debounce = Some(tokio::time::Instant::now());
            app.pending_search = Some(app.search_input.clone());
        }
        KeyCode::Char(c) => {
            // Prevent input beyond max search length
            if app.search_input.len() >= MAX_SEARCH_QUERY_LENGTH {
                app.set_status(format!(
                    "Search query at max length ({} chars)",
                    MAX_SEARCH_QUERY_LENGTH
                ));
                return Ok(Action::Continue);
            }
            app.search_input.push(c);
            // PERF-006: Set debounce instead of immediate search
            app.search_debounce = Some(tokio::time::Instant::now());
            app.pending_search = Some(app.search_input.clone());
        }
        _ => {}
    }
    Ok(Action::Continue)
}
