use reqwest::Client;
use serde_json::Value;

use crate::{string_argument, truncate_output};

pub(crate) async fn run(arguments: &Value) -> Result<String, String> {
    let url =
        string_argument(arguments, "url").ok_or_else(|| "rust_http requires a URL".to_string())?;
    let method_name = string_argument(arguments, "method").unwrap_or_else(|| "GET".to_string());
    let method = reqwest::Method::from_bytes(method_name.as_bytes())
        .map_err(|error| format!("Invalid HTTP method: {error}"))?;
    let client = Client::builder()
        .user_agent("RustPilot/0.1")
        .build()
        .map_err(|error| format!("Unable to create HTTP client: {error}"))?;
    let mut request = client.request(method, &url);
    if let Some(headers) = arguments.get("headers").and_then(Value::as_object) {
        for (key, value) in headers {
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            request = request.header(key, value);
        }
    }
    if let Some(body) = string_argument(arguments, "body") {
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("HTTP request failed: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Unable to read HTTP response: {error}"))?;
    Ok(truncate_output(&format!("HTTP {status}\n\n{body}")))
}
