use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use http_body_util::BodyExt;
use regex::Regex;
use reqwest::Client;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[derive(Clone)]
struct AppState {
    client: Client,
    webflow_url: String,
    prod_url: String,
}

#[tokio::main]
async fn main() {
    // Load .env file
    dotenvy::dotenv().ok();

    // Your Webflow site URL
    let webflow_url: String = match std::env::var("WEBFLOW_STAGING_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("Error: WEBFLOW_STAGING_URL environment variable is not set");
            std::process::exit(1);
        }
    };

    let prod_url: String = match std::env::var("PROD_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("Error: PROD_URL environment variable is not set");
            std::process::exit(1);
        }
    };

    let state = AppState {
        client: Client::new(),
        webflow_url,
        prod_url,
    };

    let app: Router = Router::new()
        .route("/*path", any(proxy_handler))
        .fallback(proxy_handler)
        .layer(CorsLayer::permissive())
        .with_state(Arc::new(state));

    let listener: tokio::net::TcpListener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .unwrap();
    
    println!("Proxy server running on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    method: axum::http::Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, StatusCode> {
    // Build the target URL
    let path: &str = uri.path();
    let query: String = uri.query().map(|q: &str| format!("?{}", q)).unwrap_or_default();
    let target_url: String = format!("{}{}{}", state.webflow_url, path, query);

    println!("Proxying {} {} -> {}", method, uri, target_url);

    // Convert body to bytes
    let body_bytes: axum::body::Bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // Build the proxied request
    let mut req_builder: reqwest::RequestBuilder = state.client.request(method.clone(), &target_url);

    // Forward relevant headers (skip host, connection, etc.)
    for (name, value) in headers.iter() {
        let name_str: String = name.as_str().to_lowercase();
        if !matches!(
            name_str.as_str(),
            "host" | "connection" | "transfer-encoding" | "content-length"
        ) {
            req_builder = req_builder.header(name, value);
        }
    }

    // Add body if present
    if !body_bytes.is_empty() {
        req_builder = req_builder.body(body_bytes);
    }

    // Send the request
    let response: reqwest::Response = match req_builder.send().await {
        Ok(resp) => resp,
        Err(e) => {
            eprintln!("Proxy error: {}", e);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    // Build response
    let mut resp_builder: axum::http::response::Builder = Response::builder().status(response.status());

    // Copy response headers
    for (name, value) in response.headers().iter() {
        let name_str: String = name.as_str().to_lowercase();
        // Skip headers that could cause issues
        // Note: reqwest auto-decompresses, so we must skip content-encoding
        if !matches!(
            name_str.as_str(),
            "transfer-encoding" | "content-length" | "connection" | "content-encoding"
        ) {
            resp_builder = resp_builder.header(name, value);
        }
    }

    // Get response body
    let body_bytes: axum::body::Bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return Err(StatusCode::BAD_GATEWAY),
    };

    // Modify content if it's HTML (example: inject custom script)
    let content_type: &str = resp_builder
        .headers_ref()
        .and_then(|h: &HeaderMap| h.get("content-type"))
        .and_then(|v: &axum::http::HeaderValue| v.to_str().ok())
        .unwrap_or("");

    let modified_body: Vec<u8> = if content_type.contains("text/html") {
        let html: std::borrow::Cow<'_, str> = String::from_utf8_lossy(&body_bytes);

        // Replace data-wf-domain attribute value with PROD_URL
        let wf_domain_re: Regex = Regex::new(r#"data-wf-domain="[^"]*""#).unwrap();
        let modified: std::borrow::Cow<'_, str> = wf_domain_re.replace_all(&html, format!(r#"data-wf-domain="{}""#, state.prod_url));

        modified.into_owned().into_bytes()
    } else {
        body_bytes.to_vec()
    };

    Ok(resp_builder
        .body(Body::from(modified_body))
        .unwrap()
        .into_response())
}