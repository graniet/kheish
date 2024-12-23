use crate::api::routes;
use crate::db::Database;
use std::net::SocketAddr;

/// Starts and runs the HTTP server using Axum web framework
///
/// # Arguments
/// * `port` - Port number to listen on for incoming HTTP connections
///
/// # Returns
/// * `Result<(), Box<dyn std::error::Error>>` - Ok if server starts successfully, Error if it fails
///
/// # Example
/// ```no_run
/// use my_app::api::server;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     server::launch_server(8080).await
/// }
/// ```
pub async fn launch_server(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let database =
        Database::new(&std::env::var("DATABASE_PATH").unwrap_or("kheish.db".to_string()));

    let app = routes::app(database);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
    Ok(())
}
