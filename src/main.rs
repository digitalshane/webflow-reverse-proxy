use axum::{
    body::Body,
    extract::{Host, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::any,
    Router,
};
use http_body_util::BodyExt;
use regex::Regex;
use reqwest::Client;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[derive(Clone, PartialEq)]
enum RedirectMode {
    Www, 
    Root,
}

#[derive(Clone)]
struct AppState {
    client: Client,
    webflow_url: String,
    prod_url: String,
    redirect_mode: RedirectMode,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

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

    let redirect_mode = match std::env::var("BASE_URL").as_deref() {
        Ok("www") => RedirectMode::Www,
        Ok("root") => RedirectMode::Root,
        Ok(other) => {
            eprintln!("Error: BASE_URL must be 'www' or 'root', got '{}'", other);
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("Error: BASE_URL environment variable is not set (use 'www' or 'root')");
            std::process::exit(1);
        }
    };

    let state = AppState {
        client: Client::new(),
        webflow_url,
        prod_url,
        redirect_mode,
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

fn check_redirect(host: &str, uri: &Uri, state: &AppState) -> Option<Redirect> {
    let host_without_port = host.split(':').next().unwrap_or(host);
    let is_www = host_without_port.starts_with("www.");

    let path = uri.path();
    let query = uri.query().map(|q| format!("?{}", q)).unwrap_or_default();

    match (&state.redirect_mode, is_www) {
        (RedirectMode::Www, false) => {
            let new_host = format!("www.{}", host_without_port);
            let redirect_url = format!("https://{}{}{}", new_host, path, query);
            println!("Redirecting to www: {}", redirect_url);
            Some(Redirect::permanent(&redirect_url))
        }
        (RedirectMode::Root, true) => {
            let new_host = host_without_port.strip_prefix("www.").unwrap_or(host_without_port);
            let redirect_url = format!("https://{}{}{}", new_host, path, query);
            println!("Redirecting to root: {}", redirect_url);
            Some(Redirect::permanent(&redirect_url))
        }
        _ => None,
    }
}

async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    Host(host): Host,
    uri: Uri,
    method: axum::http::Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, StatusCode> {

    if let Some(redirect) = check_redirect(&host, &uri, &state) {
        return Ok(redirect.into_response());
    }

    let path: &str = uri.path();
    let query: String = uri.query().map(|q: &str| format!("?{}", q)).unwrap_or_default();
    let target_url: String = format!("{}{}{}", state.webflow_url, path, query);

    println!("Proxying {} {} -> {}", method, uri, target_url);

    let body_bytes: axum::body::Bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    let mut req_builder: reqwest::RequestBuilder = state.client.request(method.clone(), &target_url);

    for (name, value) in headers.iter() {
        let name_str: String = name.as_str().to_lowercase();
        if !matches!(
            name_str.as_str(),
            "host" | "connection" | "transfer-encoding" | "content-length"
        ) {
            req_builder = req_builder.header(name, value);
        }
    }

    if !body_bytes.is_empty() {
        req_builder = req_builder.body(body_bytes);
    }

    let response: reqwest::Response = match req_builder.send().await {
        Ok(resp) => resp,
        Err(e) => {
            eprintln!("Proxy error: {}", e);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let mut resp_builder: axum::http::response::Builder = Response::builder().status(response.status());

    for (name, value) in response.headers().iter() {
        let name_str: String = name.as_str().to_lowercase();
        if !matches!(
            name_str.as_str(),
            "transfer-encoding" | "content-length" | "connection" | "content-encoding"
        ) {
            resp_builder = resp_builder.header(name, value);
        }
    }

    let body_bytes: axum::body::Bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return Err(StatusCode::BAD_GATEWAY),
    };

    let content_type: &str = resp_builder
        .headers_ref()
        .and_then(|h: &HeaderMap| h.get("content-type"))
        .and_then(|v: &axum::http::HeaderValue| v.to_str().ok())
        .unwrap_or("");

    let modified_body: Vec<u8> = if content_type.contains("text/html") {
        let html: std::borrow::Cow<'_, str> = String::from_utf8_lossy(&body_bytes);

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