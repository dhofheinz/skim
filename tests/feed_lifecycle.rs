//! Integration tests for the feed lifecycle: subscribe, categorize, rename, delete.
//!
//! Each test creates its own in-memory SQLite database for isolation.
//! These tests exercise the storage layer end-to-end, verifying that
//! operations compose correctly across feeds, categories, and articles.

use skim::storage::{Database, ParsedArticle};

async fn test_db() -> Database {
    Database::open(":memory:").await.unwrap()
}

fn test_parsed_article(guid: &str, title: &str) -> ParsedArticle {
    ParsedArticle {
        guid: guid.to_string(),
        title: title.to_string(),
        url: Some(format!("https://example.com/{}", guid)),
        published: Some(1700000000),
        summary: Some("Test summary".to_string()),
    }
}

// ============================================================================
// Subscribe (insert_feed) Tests
// ============================================================================

#[tokio::test]
async fn test_subscribe_feed_appears_in_list() {
    let db = test_db().await;

    let feed_id = db
        .insert_feed("https://example.com/feed.xml", "Example Feed", None)
        .await
        .unwrap();
    assert!(feed_id > 0);

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds.len(), 1);
    assert_eq!(feeds[0].url, "https://example.com/feed.xml");
    assert_eq!(&*feeds[0].title, "Example Feed");
    assert_eq!(feeds[0].unread_count, 0);
}

#[tokio::test]
async fn test_subscribe_duplicate_url_updates_title() {
    let db = test_db().await;

    let id1 = db
        .insert_feed("https://example.com/feed.xml", "Old Title", None)
        .await
        .unwrap();
    let id2 = db
        .insert_feed("https://example.com/feed.xml", "New Title", None)
        .await
        .unwrap();

    // Same feed ID (ON CONFLICT DO UPDATE)
    assert_eq!(id1, id2);

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds.len(), 1);
    assert_eq!(&*feeds[0].title, "New Title");
}

#[tokio::test]
async fn test_subscribe_with_html_url() {
    let db = test_db().await;

    db.insert_feed(
        "https://example.com/feed.xml",
        "Example",
        Some("https://example.com"),
    )
    .await
    .unwrap();

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds[0].html_url.as_deref(), Some("https://example.com"));
}

// ============================================================================
// Categorize Tests
// ============================================================================

#[tokio::test]
async fn test_categorize_feed_updates_category_id() {
    let db = test_db().await;

    let feed_id = db
        .insert_feed("https://example.com/feed.xml", "Feed", None)
        .await
        .unwrap();
    let cat_id = db.create_category("Tech", None).await.unwrap();

    db.move_feed_to_category(feed_id, Some(cat_id))
        .await
        .unwrap();

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds[0].category_id, Some(cat_id));
}

#[tokio::test]
async fn test_uncategorize_feed() {
    let db = test_db().await;

    let feed_id = db
        .insert_feed("https://example.com/feed.xml", "Feed", None)
        .await
        .unwrap();
    let cat_id = db.create_category("Tech", None).await.unwrap();

    // Move to category, then back to uncategorized
    db.move_feed_to_category(feed_id, Some(cat_id))
        .await
        .unwrap();
    db.move_feed_to_category(feed_id, None).await.unwrap();

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds[0].category_id, None);
}

#[tokio::test]
async fn test_delete_category_uncategorizes_feeds() {
    let db = test_db().await;

    let feed_id = db
        .insert_feed("https://example.com/feed.xml", "Feed", None)
        .await
        .unwrap();
    let cat_id = db.create_category("Doomed", None).await.unwrap();

    db.move_feed_to_category(feed_id, Some(cat_id))
        .await
        .unwrap();
    db.delete_category(cat_id).await.unwrap();

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds.len(), 1);
    assert_eq!(
        feeds[0].category_id, None,
        "Feed should be uncategorized after category deletion"
    );

    let categories = db.get_categories_tree().await.unwrap();
    assert!(categories.is_empty());
}

// ============================================================================
// Delete Feed Tests
// ============================================================================

#[tokio::test]
async fn test_delete_feed_removes_articles() {
    let db = test_db().await;

    let feed_id = db
        .insert_feed("https://example.com/feed.xml", "Feed", None)
        .await
        .unwrap();

    // Add articles
    let articles = vec![
        test_parsed_article("guid1", "Article 1"),
        test_parsed_article("guid2", "Article 2"),
        test_parsed_article("guid3", "Article 3"),
    ];
    let inserted = db.upsert_articles(feed_id, &articles).await.unwrap();
    assert_eq!(inserted, 3);

    // Verify articles exist
    let feed_articles = db.get_articles_for_feed(feed_id, None).await.unwrap();
    assert_eq!(feed_articles.len(), 3);

    // Delete feed
    let removed = db.delete_feed(feed_id).await.unwrap();
    assert_eq!(removed, 3);

    // Verify feed is gone
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert!(feeds.is_empty());
}

#[tokio::test]
async fn test_delete_feed_cleans_fts() {
    let db = test_db().await;

    let feed_id = db
        .insert_feed("https://example.com/feed.xml", "Feed", None)
        .await
        .unwrap();

    let articles = vec![test_parsed_article("guid1", "Rust Programming Language")];
    db.upsert_articles(feed_id, &articles).await.unwrap();

    // Verify FTS finds the article
    let results = db.search_articles("Rust").await.unwrap();
    assert_eq!(results.len(), 1);

    // Delete feed (cascades to articles, triggers FTS cleanup)
    db.delete_feed(feed_id).await.unwrap();

    // FTS should no longer find the article
    let results = db.search_articles("Rust").await.unwrap();
    assert!(
        results.is_empty(),
        "FTS should be cleaned up after feed deletion"
    );
}

#[tokio::test]
async fn test_delete_nonexistent_feed_is_idempotent() {
    let db = test_db().await;

    let removed = db.delete_feed(99999).await.unwrap();
    assert_eq!(removed, 0);
}

// ============================================================================
// Rename Feed Tests
// ============================================================================

#[tokio::test]
async fn test_rename_feed_updates_title() {
    let db = test_db().await;

    let feed_id = db
        .insert_feed("https://example.com/feed.xml", "Old Name", None)
        .await
        .unwrap();

    db.rename_feed(feed_id, "New Name").await.unwrap();

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(&*feeds[0].title, "New Name");
}

// ============================================================================
// Full Lifecycle Test
// ============================================================================

#[tokio::test]
async fn test_full_lifecycle_subscribe_categorize_delete() {
    let db = test_db().await;

    // Step 1: Subscribe to two feeds
    let feed1 = db
        .insert_feed("https://blog.rust-lang.org/feed.xml", "Rust Blog", None)
        .await
        .unwrap();
    let feed2 = db
        .insert_feed("https://news.ycombinator.com/rss", "Hacker News", None)
        .await
        .unwrap();

    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds.len(), 2);

    // Step 2: Create categories
    let tech = db.create_category("Tech", None).await.unwrap();
    let news = db.create_category("News", None).await.unwrap();

    // Step 3: Categorize feeds
    db.move_feed_to_category(feed1, Some(tech)).await.unwrap();
    db.move_feed_to_category(feed2, Some(news)).await.unwrap();

    // Verify categories
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    let rust_feed = feeds.iter().find(|f| f.id == feed1).unwrap();
    let hn_feed = feeds.iter().find(|f| f.id == feed2).unwrap();
    assert_eq!(rust_feed.category_id, Some(tech));
    assert_eq!(hn_feed.category_id, Some(news));

    // Step 4: Add articles to both feeds
    let rust_articles = vec![
        test_parsed_article("rust1", "Rust 2024"),
        test_parsed_article("rust2", "Async in Rust"),
    ];
    db.upsert_articles(feed1, &rust_articles).await.unwrap();

    let hn_articles = vec![test_parsed_article("hn1", "Show HN: New Tool")];
    db.upsert_articles(feed2, &hn_articles).await.unwrap();

    // Step 5: Rename a feed
    db.rename_feed(feed1, "Official Rust Blog").await.unwrap();
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    let rust_feed = feeds.iter().find(|f| f.id == feed1).unwrap();
    assert_eq!(&*rust_feed.title, "Official Rust Blog");

    // Step 6: Verify FTS works across feeds
    let results = db.search_articles("Rust").await.unwrap();
    assert_eq!(results.len(), 2, "Both Rust articles should match");

    // Step 7: Delete feed1 (Rust Blog)
    let removed = db.delete_feed(feed1).await.unwrap();
    assert_eq!(removed, 2, "Two articles should be cascade-deleted");

    // Verify feed1 is gone but feed2 remains
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds.len(), 1);
    assert_eq!(feeds[0].id, feed2);

    // Verify FTS only finds HN articles now
    let results = db.search_articles("Rust").await.unwrap();
    assert!(
        results.is_empty(),
        "Rust articles should be cleaned from FTS"
    );

    // Step 8: Delete the Tech category (feed1 is already gone, so no orphaning needed)
    db.delete_category(tech).await.unwrap();
    let categories = db.get_categories_tree().await.unwrap();
    assert_eq!(categories.len(), 1);
    assert_eq!(categories[0].name, "News");

    // Step 9: Delete News category — HN feed should become uncategorized
    db.delete_category(news).await.unwrap();
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds[0].category_id, None);
}

// ============================================================================
// Category Nesting with Feed Lifecycle
// ============================================================================

#[tokio::test]
async fn test_nested_categories_with_feeds() {
    let db = test_db().await;

    // Create nested categories: Tech > Rust
    let tech = db.create_category("Tech", None).await.unwrap();
    let rust = db.create_category("Rust", Some(tech)).await.unwrap();

    // Subscribe and categorize
    let feed_id = db
        .insert_feed("https://blog.rust-lang.org/feed.xml", "Rust Blog", None)
        .await
        .unwrap();
    db.move_feed_to_category(feed_id, Some(rust)).await.unwrap();

    // Delete parent category — child should become root, feed stays in child
    db.delete_category(tech).await.unwrap();

    let categories = db.get_categories_tree().await.unwrap();
    assert_eq!(categories.len(), 1);
    assert_eq!(categories[0].name, "Rust");
    assert_eq!(
        categories[0].parent_id, None,
        "Child should become root-level"
    );

    // Feed should still be in the Rust category
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    assert_eq!(feeds[0].category_id, Some(rust));
}

#[tokio::test]
async fn test_export_includes_all_feeds() {
    let db = test_db().await;

    db.insert_feed("https://a.com/feed.xml", "Feed A", Some("https://a.com"))
        .await
        .unwrap();
    db.insert_feed("https://b.com/feed.xml", "Feed B", None)
        .await
        .unwrap();

    let exported = db.get_feeds_for_export().await.unwrap();
    assert_eq!(exported.len(), 2);

    // Alphabetical order
    assert_eq!(exported[0].title, "Feed A");
    assert_eq!(exported[0].xml_url, "https://a.com/feed.xml");
    assert_eq!(exported[0].html_url.as_deref(), Some("https://a.com"));

    assert_eq!(exported[1].title, "Feed B");
    assert_eq!(exported[1].html_url, None);
}
