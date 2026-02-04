use crate::app::{App, AppEvent, ContentState, FetchResult, Focus, View};
use crate::content::fetch_content;
use crate::feed::{refresh_all, refresh_one};
use anyhow::Result;
use crossterm::{
    event::{Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    Frame, Terminal,
};
use std::io::{self, Stdout};
use std::time::Duration;
use tokio::sync::mpsc;

mod articles;
mod feeds;
mod reader;
mod status;
mod whatsnew;

/// Result of handling a key press
pub enum Action {
    Continue,
    Quit,
}

/// Run the TUI application
pub async fn run(
    app: &mut App,
    event_tx: mpsc::Sender<AppEvent>,
    mut event_rx: mpsc::Receiver<AppEvent>,
) -> Result<()> {
    // Install panic hook BEFORE setting up terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let mut terminal = setup_terminal()?;
    let mut event_stream = crossterm::event::EventStream::new();

    loop {
        // Render
        terminal.draw(|f| render(f, app))?;

        // Clear expired status messages
        app.clear_expired_status();

        tokio::select! {
            // Terminal input events
            maybe_event = event_stream.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    match handle_input(app, key.code, key.modifiers, &event_tx).await {
                        Ok(Action::Quit) => break,
                        Ok(Action::Continue) => {}
                        Err(e) => app.set_status(format!("Error: {}", e)),
                    }
                }
            }

            // Background task events
            Some(event) = event_rx.recv() => {
                handle_app_event(app, event).await;
            }

            // Periodic tick for status expiry
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }

    restore_terminal(terminal)?;
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn render(f: &mut Frame, app: &App) {
    match app.view {
        View::Browse => render_browse(f, app),
        View::Reader => render_reader(f, app),
    }
}

fn render_browse(f: &mut Frame, app: &App) {
    // Layout depends on whether What's New panel is visible
    if app.show_whats_new && !app.whats_new.is_empty() {
        // Three rows: What's New (dynamic height), main panels, status bar
        let whats_new_height = (app.whats_new.len() as u16 + 2).min(10); // +2 for border, max 10 lines

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(whats_new_height),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(f.area());

        whatsnew::render(f, app, chunks[0]);

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[1]);

        feeds::render(f, app, main_chunks[0]);
        articles::render(f, app, main_chunks[1]);
        status::render(f, app, chunks[2]);
    } else {
        // Normal two-row layout
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(f.area());

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[0]);

        feeds::render(f, app, main_chunks[0]);
        articles::render(f, app, main_chunks[1]);
        status::render(f, app, chunks[1]);
    }
}

fn render_reader(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    reader::render(f, app, chunks[0]);
    status::render(f, app, chunks[1]);
}

async fn handle_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    // Handle search mode input separately
    if app.search_mode {
        return handle_search_input(app, code).await;
    }

    match app.view {
        View::Browse => handle_browse_input(app, code, modifiers, event_tx).await,
        View::Reader => handle_reader_input(app, code, modifiers),
    }
}

async fn handle_browse_input(
    app: &mut App,
    code: KeyCode,
    _modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    match code {
        KeyCode::Char('q') => return Ok(Action::Quit),
        KeyCode::Esc => {
            // Dismiss What's New panel if visible
            if app.show_whats_new {
                app.dismiss_whats_new();
            }
        }
        KeyCode::Char('j') | KeyCode::Down => app.nav_down(),
        KeyCode::Char('k') | KeyCode::Up => app.nav_up(),
        KeyCode::Tab => {
            app.focus = if app.show_whats_new {
                match app.focus {
                    Focus::WhatsNew => Focus::Feeds,
                    Focus::Feeds => Focus::Articles,
                    Focus::Articles => Focus::WhatsNew,
                }
            } else {
                match app.focus {
                    Focus::Feeds => Focus::Articles,
                    Focus::Articles => Focus::Feeds,
                    Focus::WhatsNew => Focus::Feeds, // Shouldn't happen, but handle gracefully
                }
            };
        }
        KeyCode::Enter => {
            if app.focus == Focus::WhatsNew {
                // Open article from What's New panel
                if let Some((_, article)) = app.whats_new.get(app.whats_new_selected).cloned() {
                    let article_id = article.id;
                    let article_url = article.url.clone();
                    let article_summary = article.summary.clone();

                    // Set up reader view
                    app.view = View::Reader;
                    app.scroll_offset = 0;
                    app.content_state = ContentState::Loading { article_id };
                    app.reader_article = Some(article);
                    app.db.mark_read(article_id).await?;

                    // Spawn content loading task
                    if let Some(url) = article_url {
                        let client = app.http_client.clone();
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let result = fetch_content(&client, &url).await;
                            let _ = tx.send(AppEvent::ContentLoaded(article_id, result)).await;
                        });
                    } else {
                        app.content_state = ContentState::Failed {
                            article_id,
                            error: "Article has no URL".to_string(),
                            fallback: article_summary,
                        };
                    }
                }
            } else if app.focus == Focus::Feeds {
                // Load articles for selected feed
                if let Some(feed) = app.selected_feed() {
                    let feed_id = feed.id;
                    app.articles = app.db.get_articles_for_feed(feed_id).await?;
                    app.selected_article = 0;
                    app.focus = Focus::Articles;
                }
            } else if app.focus == Focus::Articles {
                // Enter reader view and spawn content loading task
                if let Some(article) = app.enter_reader() {
                    let article_id = article.id;
                    let article_url = article.url.clone();
                    let article_summary = article.summary.clone();
                    app.db.mark_read(article_id).await?;

                    // Spawn content loading task
                    if let Some(url) = article_url {
                        let client = app.http_client.clone();
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let result = fetch_content(&client, &url).await;
                            let _ = tx.send(AppEvent::ContentLoaded(article_id, result)).await;
                        });
                    } else {
                        // No URL - use summary as fallback immediately
                        app.content_state = ContentState::Failed {
                            article_id,
                            error: "Article has no URL".to_string(),
                            fallback: article_summary,
                        };
                    }
                }
            }
        }
        KeyCode::Char('r') => {
            // Prevent multiple concurrent refreshes
            if app.refresh_progress.is_some() {
                app.set_status("Refresh already in progress");
            } else if app.feeds.is_empty() {
                app.set_status("No feeds to refresh");
            } else {
                app.set_status("Refreshing feeds...");
                app.refresh_progress = Some((0, app.feeds.len()));

                // Clone what we need for the background task
                let db = app.db.clone();
                let client = app.http_client.clone();
                let feeds = app.feeds.clone();
                let tx = event_tx.clone();

                // Spawn refresh task
                tokio::spawn(async move {
                    // Create a channel for progress updates
                    let (progress_tx, mut progress_rx) = mpsc::channel::<(usize, usize)>(32);

                    // Spawn the refresh in a separate task
                    let refresh_handle =
                        tokio::spawn(
                            async move { refresh_all(db, client, feeds, progress_tx).await },
                        );

                    // Forward progress updates
                    while let Some((done, total)) = progress_rx.recv().await {
                        let _ = tx.send(AppEvent::RefreshProgress(done, total)).await;
                    }

                    // Get results and send completion
                    if let Ok(results) = refresh_handle.await {
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
                        let _ = tx.send(AppEvent::RefreshComplete(fetch_results)).await;
                    }
                });
            }
        }
        KeyCode::Char('R') => {
            // Refresh single feed (Shift+R)
            if app.refresh_progress.is_some() {
                app.set_status("Refresh already in progress");
            } else if let Some(feed) = app.selected_feed().cloned() {
                app.set_status(format!("Refreshing {}...", feed.title));
                app.refresh_progress = Some((0, 1));

                let db = app.db.clone();
                let client = app.http_client.clone();
                let tx = event_tx.clone();

                tokio::spawn(async move {
                    let result = refresh_one(&db, &client, &feed).await;

                    // Send progress complete
                    let _ = tx.send(AppEvent::RefreshProgress(1, 1)).await;

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
                    let _ = tx.send(AppEvent::RefreshComplete(vec![fetch_result])).await;
                });
            } else {
                app.set_status("No feed selected");
            }
        }
        KeyCode::Char('s') => {
            if let Some(article) = app.selected_article() {
                let article_id = article.id;
                app.db.toggle_star(article_id).await?;
                // Refresh article list
                if let Some(feed) = app.selected_feed() {
                    app.articles = app.db.get_articles_for_feed(feed.id).await?;
                }
            }
        }
        KeyCode::Char('o') => {
            if let Some(article) = app.selected_article() {
                if let Some(url) = &article.url {
                    if let Err(e) = open::that(url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    }
                } else {
                    app.set_status("Article has no URL");
                }
            }
        }
        KeyCode::Char('/') => {
            app.search_mode = true;
            app.search_input.clear();
        }
        _ => {}
    }
    Ok(Action::Continue)
}

fn handle_reader_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<Action> {
    match code {
        KeyCode::Char('q') => return Ok(Action::Quit),
        KeyCode::Char('b') | KeyCode::Esc => {
            app.exit_reader();
        }
        KeyCode::Char('j') | KeyCode::Down => app.scroll_down(1),
        KeyCode::Char('k') | KeyCode::Up => app.scroll_up(1),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll_down(20),
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll_up(20),
        KeyCode::Char('s') => {
            if let Some(_article) = app.reader_article.as_ref() {
                // Toggle star would need async here - simplified for now
                // In practice, this would need to be refactored to use events
            }
        }
        KeyCode::Char('o') => {
            if let Some(article) = app.reader_article.as_ref() {
                if let Some(url) = &article.url {
                    if let Err(e) = open::that(url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(Action::Continue)
}

async fn handle_search_input(app: &mut App, code: KeyCode) -> Result<Action> {
    match code {
        KeyCode::Esc => {
            app.search_mode = false;
            app.search_input.clear();
            // Restore original articles for current feed
            if let Some(feed) = app.selected_feed() {
                app.articles = app.db.get_articles_for_feed(feed.id).await?;
            }
            app.selected_article = 0;
        }
        KeyCode::Enter => {
            app.search_mode = false;
            // Keep search results displayed
        }
        KeyCode::Backspace => {
            app.search_input.pop();
            // Re-run search with updated query
            if app.search_input.is_empty() {
                // If query empty, restore feed articles
                if let Some(feed) = app.selected_feed() {
                    app.articles = app.db.get_articles_for_feed(feed.id).await?;
                }
            } else {
                app.articles = app.db.search_articles(&app.search_input).await?;
            }
            app.selected_article = 0;
        }
        KeyCode::Char(c) => {
            app.search_input.push(c);
            // Run search
            app.articles = app.db.search_articles(&app.search_input).await?;
            app.selected_article = 0;
        }
        _ => {}
    }
    Ok(Action::Continue)
}

async fn handle_app_event(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::RefreshProgress(done, total) => {
            app.refresh_progress = Some((done, total));
        }
        AppEvent::RefreshComplete(results) => {
            app.refresh_progress = None;
            let failed = results.iter().filter(|r| r.error.is_some()).count();
            let total_new: usize = results.iter().map(|r| r.new_articles).sum();

            if failed > 0 {
                app.set_status(format!(
                    "Refresh complete. {} new articles, {} feeds failed.",
                    total_new, failed
                ));
            } else {
                app.set_status(format!("Refresh complete. {} new articles.", total_new));
            }

            // Reload feeds with updated counts
            if let Ok(feeds) = app.db.get_feeds_with_unread_counts().await {
                app.feeds = feeds.clone();

                // Populate What's New with recent unread articles
                if total_new > 0 {
                    // Fetch recent articles from feeds that had new content
                    let mut new_articles = Vec::new();
                    for result in &results {
                        if result.new_articles > 0 {
                            // Find the feed title
                            let feed_title = feeds
                                .iter()
                                .find(|f| f.id == result.feed_id)
                                .map(|f| f.title.clone())
                                .unwrap_or_else(|| "Unknown".to_string());

                            // Get recent unread articles for this feed
                            if let Ok(articles) = app.db.get_articles_for_feed(result.feed_id).await
                            {
                                for article in articles.into_iter().take(result.new_articles) {
                                    if !article.read {
                                        new_articles.push((feed_title.clone(), article));
                                    }
                                }
                            }
                        }
                    }

                    // Sort by published date (newest first) and limit to 20
                    new_articles.sort_by(|a, b| b.1.published.cmp(&a.1.published));
                    new_articles.truncate(20);

                    if !new_articles.is_empty() {
                        app.whats_new = new_articles;
                        app.whats_new_selected = 0;
                        app.show_whats_new = true;
                        app.focus = Focus::WhatsNew;
                    }
                }
            }
        }
        AppEvent::ContentLoaded(article_id, result) => match result {
            Ok(content) => {
                app.content_state = ContentState::Loaded {
                    article_id,
                    content,
                };
            }
            Err(e) => {
                let fallback = app.reader_article.as_ref().and_then(|a| a.summary.clone());
                app.content_state = ContentState::Failed {
                    article_id,
                    error: e.to_string(),
                    fallback,
                };
            }
        },
    }
}
