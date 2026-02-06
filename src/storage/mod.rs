mod articles;
mod categories;
mod feeds;
mod preferences;
mod schema;
mod search;
mod types;

pub use schema::Database;
#[allow(unused_imports)] // FeedCategory consumed by downstream tasks (TASK-5, TASK-8)
pub use types::{Article, DatabaseError, Feed, FeedCategory, OpmlFeed, ParsedArticle};
