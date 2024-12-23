mod models;
mod task_repository;

use diesel::r2d2::{ConnectionManager, Pool, PooledConnection};
use diesel::sqlite::SqliteConnection;
use std::sync::Arc;

pub use models::*;
pub use task_repository::*;

#[derive(Clone, Debug)]
pub struct Database {
    pool: Arc<Pool<ConnectionManager<SqliteConnection>>>,
}

impl Database {
    pub fn new(db_path: &str) -> Self {
        let manager = ConnectionManager::<SqliteConnection>::new(db_path);
        let pool = Pool::builder()
            .build(manager)
            .expect("Failed to create pool.");

        Database {
            pool: Arc::new(pool),
        }
    }

    pub fn get_conn(&self) -> PooledConnection<ConnectionManager<SqliteConnection>> {
        self.pool.get().expect("Failed to get connection")
    }
}
