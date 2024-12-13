use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use tracing::{debug, info};

/// WebModule for making HTTP requests with basic cookie management
#[derive(Debug)]
pub struct WebModule {
    /// In-memory cookie store: domain -> cookie string
    pub cookies: RwLock<HashMap<String, String>>,
}

impl WebModule {
    /// Creates a new WebModule instance
    pub fn new() -> Self {
        WebModule {
            cookies: RwLock::new(HashMap::new()),
        }
    }

    /// Extract cookies from response headers and store them
    fn store_cookies(&self, domain: &str, set_cookie_headers: &[String]) {
        let combined = set_cookie_headers.join("; ");
        if !combined.is_empty() {
            self.cookies
                .write()
                .unwrap()
                .insert(domain.to_string(), combined);
        }
    }

    /// Build a cookie header if cookies are stored for this domain
    fn get_cookie_header(&self, domain: &str) -> Option<String> {
        self.cookies
            .read()
            .unwrap()
            .get(domain)
            .map(|c| c.to_string())
    }
}

#[async_trait]
impl Module for WebModule {
    fn name(&self) -> &str {
        "web"
    }

    async fn handle_action(
        &self,
        _vector_store: &mut dyn VectorStoreProvider,
        action: &str,
        params: &[String],
    ) -> Result<String, String> {
        let this = self;
        match action {
            "get.store" => {
                if params.is_empty() {
                    return Err("Usage: get <url>".into());
                }
                let url = &params[0];
                debug!("Performing GET request to {}", url);

                let domain = match url::Url::parse(url) {
                    Ok(u) => u.host_str().unwrap_or("unknown").to_string(),
                    Err(_) => "unknown".to_string(),
                };

                let client = reqwest::Client::new();
                let mut req = client.get(url);

                if let Some(cookie_str) = this.get_cookie_header(&domain) {
                    req = req.header("Cookie", cookie_str);
                }

                let resp = req.send().await.map_err(|e| e.to_string())?;
                let status = resp.status();
                let headers = resp.headers().clone();
                let body = resp.text().await.map_err(|e| e.to_string())?;

                let set_cookies: Vec<String> = headers
                    .get_all(reqwest::header::SET_COOKIE)
                    .iter()
                    .filter_map(|h| h.to_str().ok())
                    .map(|s| s.to_string())
                    .collect();

                if !set_cookies.is_empty() {
                    this.store_cookies(&domain, &set_cookies);
                }

                info!("GET {} -> status: {}", url, status);
                Ok(format!("STATUS: {}\n\n{}", status, body))
            }
            "get" => {
                if params.is_empty() {
                    return Err("Usage: get <url>".into());
                }
                let url = &params[0];
                debug!("Performing GET request to {}", url);

                let domain = match url::Url::parse(url) {
                    Ok(u) => u.host_str().unwrap_or("unknown").to_string(),
                    Err(_) => "unknown".to_string(),
                };

                let client = reqwest::Client::new();
                let mut req = client.get(url);

                if let Some(cookie_str) = this.get_cookie_header(&domain) {
                    req = req.header("Cookie", cookie_str);
                }

                let resp = req.send().await.map_err(|e| e.to_string())?;
                let status = resp.status();
                let headers = resp.headers().clone();
                let body = resp.text().await.map_err(|e| e.to_string())?;

                let set_cookies: Vec<String> = headers
                    .get_all(reqwest::header::SET_COOKIE)
                    .iter()
                    .filter_map(|h| h.to_str().ok())
                    .map(|s| s.to_string())
                    .collect();

                if !set_cookies.is_empty() {
                    this.store_cookies(&domain, &set_cookies);
                }

                info!("GET {} -> status: {}", url, status);
                Ok(format!("STATUS: {}\n\n{}", status, body))
            }
            "post" => {
                if params.len() < 2 {
                    return Err("Usage: post <url> <data>".into());
                }
                let url = &params[0];
                let data = &params[1];

                debug!("Performing POST request to {}", url);

                let domain = match url::Url::parse(url) {
                    Ok(u) => u.host_str().unwrap_or("unknown").to_string(),
                    Err(_) => "unknown".to_string(),
                };

                let client = reqwest::Client::new();
                let mut req = client.post(url).body(data.clone());

                if let Some(cookie_str) = this.get_cookie_header(&domain) {
                    req = req.header("Cookie", cookie_str);
                }

                let resp = req.send().await.map_err(|e| e.to_string())?;
                let status = resp.status();
                let headers = resp.headers().clone();
                let body = resp.text().await.map_err(|e| e.to_string())?;

                let set_cookies: Vec<String> = headers
                    .get_all(reqwest::header::SET_COOKIE)
                    .iter()
                    .filter_map(|h| h.to_str().ok())
                    .map(|s| s.to_string())
                    .collect();

                if !set_cookies.is_empty() {
                    this.store_cookies(&domain, &set_cookies);
                }

                info!("POST {} -> status: {}", url, status);
                Ok(format!("STATUS: {}\n\n{}", status, body))
            }
            _ => Err(format!("Unknown action '{}'", action)),
        }
    }

    fn get_actions(&self) -> Vec<ModuleAction> {
        vec![
            ModuleAction {
                name: "get".into(),
                arg_count: 1,
                description: "Perform a GET request to a given URL. Usage: get <url>".into(),
            },
            ModuleAction {
                name: "post".into(),
                arg_count: 2,
                description: "Perform a POST request to a given URL with provided data. Usage: post <url> <data>".into(),
            },
        ]
    }
}
