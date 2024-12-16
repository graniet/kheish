//! Web module for making HTTP requests with cookie support.
//!
//! This module provides functionality for making HTTP GET and POST requests while
//! maintaining cookie state across requests to the same domain.

use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue, SET_COOKIE};
use std::collections::HashMap;
use std::sync::RwLock;
use tracing::{debug, info};
use url::Url;

/// Web module that handles HTTP requests with cookie persistence.
#[derive(Debug)]
pub struct HttpModule {
    /// Cookie storage mapping domains to cookie strings.
    cookies: RwLock<HashMap<String, String>>,
}

impl HttpModule {
    /// Creates a new `WebModule` instance with empty cookie storage.
    pub fn new() -> Self {
        HttpModule {
            cookies: RwLock::new(HashMap::new()),
        }
    }

    /// Stores cookies for a domain from Set-Cookie headers.
    ///
    /// # Arguments
    ///
    /// * `domain` - The domain to store cookies for
    /// * `set_cookie_headers` - Array of Set-Cookie header values
    fn store_cookies(&self, domain: &str, set_cookie_headers: &[String]) {
        if set_cookie_headers.is_empty() {
            return;
        }

        let mut write_guard = match self.cookies.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("Cookies lock poisoned, recovering...");
                poisoned.into_inner()
            }
        };

        let current_cookies = write_guard.get(domain).cloned().unwrap_or_default();

        let mut merged = current_cookies
            .split(';')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        for new_cookie in set_cookie_headers {
            let cookie_parts = new_cookie.split(';').map(|s| s.trim());
            for part in cookie_parts {
                if !part.is_empty() && !merged.contains(&part) {
                    merged.push(part);
                }
            }
        }

        let combined = merged.join("; ");
        if !combined.is_empty() {
            write_guard.insert(domain.to_string(), combined);
        }
    }

    /// Gets the Cookie header value for a domain.
    ///
    /// # Arguments
    ///
    /// * `domain` - The domain to get cookies for
    ///
    /// # Returns
    ///
    /// The Cookie header value if cookies exist for the domain
    fn get_cookie_header(&self, domain: &str) -> Option<String> {
        let read_guard = match self.cookies.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("Cookies lock poisoned, recovering...");
                poisoned.into_inner()
            }
        };
        read_guard.get(domain).map(|c| c.to_string())
    }

    /// Extracts the domain from a URL string.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL to extract the domain from
    ///
    /// # Returns
    ///
    /// The domain string or an error if URL is invalid
    fn extract_domain(url: &str) -> Result<String, String> {
        let parsed = Url::parse(url).map_err(|e| format!("Invalid URL: {}", e))?;
        parsed
            .host_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "No domain found in URL".to_string())
    }

    /// Detects the content type of request data.
    ///
    /// # Arguments
    ///
    /// * `data` - The request data to analyze
    ///
    /// # Returns
    ///
    /// The content type string - either "application/json" or "text/plain"
    fn detect_content_type(data: &str) -> &str {
        let trimmed = data.trim();
        if (trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']'))
        {
            "application/json"
        } else {
            "text/plain"
        }
    }

    /// Performs an HTTP request.
    ///
    /// # Arguments
    ///
    /// * `method` - The HTTP method ("get" or "post")
    /// * `url` - The request URL
    /// * `data` - Optional request body data
    /// * `extra_headers` - Optional additional headers
    ///
    /// # Returns
    ///
    /// The response status and body or an error message
    async fn perform_request(
        &self,
        method: &str,
        url: &str,
        data: Option<&str>,
        extra_headers: Option<&[String]>,
    ) -> Result<String, String> {
        let domain = Self::extract_domain(url)?;

        let client = reqwest::Client::new();
        let mut req_builder = match method.to_lowercase().as_str() {
            "get" => client.get(url),
            "post" => {
                let mut r = client.post(url);
                if let Some(d) = data {
                    r = r
                        .header("Content-Type", Self::detect_content_type(d))
                        .body(d.to_string());
                }
                r
            }
            _ => return Err(format!("Unsupported method '{}'", method)),
        };

        if let Some(cookie_str) = self.get_cookie_header(&domain) {
            req_builder = req_builder.header("Cookie", cookie_str);
        }

        if let Some(headers) = extra_headers {
            req_builder = add_custom_headers(req_builder, headers)?;
        }

        let resp = req_builder.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.map_err(|e| e.to_string())?;

        let set_cookies: Vec<String> = headers
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .map(|s| s.to_string())
            .collect();

        if !set_cookies.is_empty() {
            self.store_cookies(&domain, &set_cookies);
        }

        info!("{} {} -> status: {}", method.to_uppercase(), url, status);
        Ok(format!("STATUS: {}\n\n{}", status, body))
    }
}

/// Adds custom headers to a request builder.
///
/// # Arguments
///
/// * `builder` - The request builder to add headers to
/// * `headers` - Array of header strings in "Name: Value" format
///
/// # Returns
///
/// The modified request builder or an error if headers are invalid
fn add_custom_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &[String],
) -> Result<reqwest::RequestBuilder, String> {
    for h in headers {
        if let Some((name, value)) = h.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if !name.is_empty() && !value.is_empty() {
                let header_name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| format!("Invalid header name '{}'", name))?;
                let header_value = HeaderValue::from_str(value)
                    .map_err(|_| format!("Invalid header value '{}'", value))?;
                builder = builder.header(header_name, header_value);
            }
        }
    }
    Ok(builder)
}

#[async_trait]
impl Module for HttpModule {
    /// Returns the module name.
    fn name(&self) -> &str {
        "http"
    }

    /// Handles module actions.
    ///
    /// # Arguments
    ///
    /// * `_vector_store` - Vector store provider (unused)
    /// * `action` - The action to perform ("get" or "post")
    /// * `params` - Action parameters (URL, data, headers)
    ///
    /// # Returns
    ///
    /// The action result or an error message
    async fn handle_action(
        &self,
        _vector_store: &mut dyn VectorStoreProvider,
        action: &str,
        params: &[String],
    ) -> Result<String, String> {
        match action {
            "get" => {
                if params.is_empty() {
                    return Err("Usage: web get <url> [Headers:...]".into());
                }
                let url = &params[0];
                debug!("Performing GET request to {}", url);

                let extra_headers = if params.len() > 1 {
                    Some(&params[1..])
                } else {
                    None
                };

                self.perform_request("get", url, None, extra_headers).await
            }

            "post" => {
                if params.len() < 2 {
                    return Err("Usage: web post <url> <data> [Headers:...]".into());
                }
                let url = &params[0];
                let data = &params[1];
                debug!("Performing POST request to {}", url);

                let extra_headers = if params.len() > 2 {
                    Some(&params[2..])
                } else {
                    None
                };

                self.perform_request("post", url, Some(data), extra_headers)
                    .await
            }

            _ => Err(format!("Unknown action '{}' for web module", action)),
        }
    }

    /// Returns the available module actions.
    fn get_actions(&self) -> Vec<ModuleAction> {
        vec![
            ModuleAction {
                name: "get".into(),
                arg_count: 1,
                description: "Perform a GET request. Usage: web get <url> [Header:Value ...]".into(),
            },
            ModuleAction {
                name: "post".into(),
                arg_count: 2,
                description:
                    "Perform a POST request with data. Usage: web post <url> <data> [Header:Value ...]"
                        .into(),
            },
        ]
    }
}
