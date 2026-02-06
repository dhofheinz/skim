mod articles;
mod categories;
mod content_cache;
mod feeds;
mod preferences;
mod reading_history;
mod schema;
mod search;
mod types;

pub use schema::Database;
#[allow(unused_imports)]
// FeedCategory, ReadingHistoryEntry, ReadingStats consumed by downstream tasks (TASK-5, TASK-8)
pub use types::{
    Article, DatabaseError, Feed, FeedCategory, OpmlFeed, ParsedArticle, ReadingHistoryEntry,
    ReadingStats, SearchScope,
};
#[allow(unused_imports)] // CachedContent/CacheStats consumed by TASK-4, TASK-7
pub use types::{CacheStats, CachedContent};
