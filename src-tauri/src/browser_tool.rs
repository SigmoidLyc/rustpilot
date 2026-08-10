use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::Client;
use serde_json::{json, Value};
use tokio::process::Command;
use uuid::Uuid;

use crate::{
    first_env_value, path_guard, string_argument, truncate_output, workspace_root, AppState,
    BrowserSession, BrowserTab,
};

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn remove_html_block(mut html: String, tag: &str) -> String {
    let lower_tag = tag.to_lowercase();
    loop {
        let lower = html.to_lowercase();
        let Some(start) = lower.find(&format!("<{lower_tag}")) else {
            break;
        };
        let Some(end_offset) = lower[start..].find(&format!("</{lower_tag}>")) else {
            html.replace_range(start.., " ");
            break;
        };
        let end = start + end_offset + lower_tag.len() + 3;
        html.replace_range(start..end, " ");
    }
    html
}

pub(crate) fn html_text(html: &str) -> String {
    let mut cleaned = remove_html_block(html.to_string(), "script");
    cleaned = remove_html_block(cleaned, "style");
    cleaned = remove_html_block(cleaned, "nav");
    cleaned = remove_html_block(cleaned, "header");
    cleaned = remove_html_block(cleaned, "footer");
    let mut text = String::with_capacity(cleaned.len());
    let mut in_tag = false;
    for character in cleaned.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn html_title(html: &str) -> String {
    let lower = html.to_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(open_end) = lower[start..].find('>') else {
        return String::new();
    };
    let content_start = start + open_end + 1;
    let Some(close_offset) = lower[content_start..].find("</title>") else {
        return String::new();
    };
    html_text(&html[content_start..content_start + close_offset])
}

fn html_attribute(fragment: &str, attribute: &str) -> Option<String> {
    let lower = fragment.to_lowercase();
    let marker = format!("{attribute}=");
    let start = lower.find(&marker)? + marker.len();
    let rest = fragment[start..].trim_start();
    let quote = rest.chars().next()?;
    if quote == '\'' || quote == '"' {
        let end = rest[1..].find(quote)? + 1;
        Some(rest[1..end].to_string())
    } else {
        Some(
            rest.split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches('>')
                .to_string(),
        )
    }
}

fn absolute_url(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if href.starts_with("//") {
        if let Some(scheme_end) = base.find("://") {
            return format!("{}:{}", &base[..scheme_end], href);
        }
    }
    let host_end = base
        .find("//")
        .and_then(|offset| base[offset + 2..].find('/').map(|end| offset + 2 + end))
        .unwrap_or(base.len());
    let origin = &base[..host_end];
    if href.starts_with('/') {
        format!("{origin}{href}")
    } else {
        let directory = base
            .rsplit_once('/')
            .map(|(prefix, _)| prefix)
            .unwrap_or(base);
        format!("{directory}/{href}")
    }
}

pub(crate) fn html_links(html: &str, base: &str) -> Vec<(String, String)> {
    let lower = html.to_lowercase();
    let mut links = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<a") {
        let start = cursor + offset;
        let Some(open_offset) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + open_offset;
        let Some(close_offset) = lower[open_end + 1..].find("</a>") else {
            break;
        };
        let close_end = open_end + 1 + close_offset;
        let fragment = &html[start..=open_end];
        if let Some(href) = html_attribute(fragment, "href") {
            let label = html_text(&html[open_end + 1..close_end]);
            if !href.starts_with('#') && !href.starts_with("javascript:") {
                links.push((label, absolute_url(base, &href)));
            }
        }
        cursor = close_end + 4;
        if links.len() >= 80 {
            break;
        }
    }
    links
}

fn html_select_options(html: &str) -> Vec<Value> {
    let lower = html.to_lowercase();
    let mut options = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<option") {
        let start = cursor + offset;
        let Some(tag_end_offset) = lower[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end_offset;
        let Some(close_offset) = lower[tag_end + 1..].find("</option>") else {
            break;
        };
        let close = tag_end + 1 + close_offset;
        let fragment = &html[start..=tag_end];
        let label = html_text(&html[tag_end + 1..close]);
        options.push(json!({
            "text": label,
            "value": html_attribute(fragment, "value").unwrap_or_default(),
            "index": options.len()
        }));
        cursor = close + "</option>".len();
        if options.len() >= 100 {
            break;
        }
    }
    options
}

async fn load_browser_page(
    session: &mut BrowserSession,
    url: String,
) -> Result<reqwest::StatusCode, String> {
    let (status, html) = fetch_page(&url).await?;
    session.current_url = url;
    session.title = html_title(&html);
    session.html = html;
    if let Some(tab) = session
        .tabs
        .iter_mut()
        .find(|tab| tab.id == session.active_tab_id)
    {
        tab.url = session.current_url.clone();
        tab.title = session.title.clone();
    }
    Ok(status)
}

async fn capture_browser_screenshot(url: &str) -> Result<PathBuf, String> {
    let browser = first_env_value(&["RUSTPILOT_BROWSER_PATH"]).or_else(|| {
        [
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        ]
        .iter()
        .find(|path| Path::new(*path).exists())
        .map(|path| (*path).to_string())
    });
    let Some(browser) = browser else {
        return Err("No Chromium-compatible browser executable was found.".to_string());
    };
    let workspace = workspace_root();
    let output_dir = path_guard::resolve_scoped_path(&workspace, ".rustpilot/browser-artifacts")?;
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| format!("Unable to create browser artifact directory: {error}"))?;
    let output_path = output_dir.join(format!("{}.png", Uuid::new_v4()));
    let profile_dir = output_dir.join(format!("profile-{}", Uuid::new_v4()));
    let status = Command::new(browser)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
        ])
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--screenshot={}", output_path.display()))
        .arg("--window-size=1440,1000")
        .arg("--virtual-time-budget=2500")
        .arg(url)
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|error| format!("Unable to start browser screenshot: {error}"))?;
    if !status.success() || !output_path.exists() {
        return Err(format!("Browser screenshot command failed with {status}."));
    }
    Ok(output_path)
}

async fn fetch_page(url: &str) -> Result<(reqwest::StatusCode, String), String> {
    let client = Client::builder()
        .user_agent("RustPilot/0.1 (lightweight agent browser)")
        .build()
        .map_err(|error| format!("Unable to create browser client: {error}"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("Unable to fetch {url}: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Unable to read {url}: {error}"))?;
    Ok((status, body))
}

pub(crate) async fn run_web_search_tool(arguments: &Value) -> Result<String, String> {
    let query = string_argument(arguments, "query")
        .ok_or_else(|| "rust_web_search requires query".to_string())?;
    let number = arguments
        .get("num_results")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 10);
    let fetch_content = arguments
        .get("fetch_content")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let engines = [
        (
            "duckduckgo",
            format!(
                "https://html.duckduckgo.com/html/?q={}",
                percent_encode(&query)
            ),
        ),
        (
            "bing",
            format!(
                "https://www.bing.com/search?q={}&count={number}",
                percent_encode(&query)
            ),
        ),
        (
            "google",
            format!(
                "https://www.google.com/search?q={}&num={number}",
                percent_encode(&query)
            ),
        ),
        (
            "baidu",
            format!("https://www.baidu.com/s?wd={}", percent_encode(&query)),
        ),
    ];
    let mut results = Vec::new();
    let mut source = "none";
    let mut fallback_text = String::new();
    for (engine, url) in engines {
        let fetched = tokio::time::timeout(Duration::from_secs(15), fetch_page(&url)).await;
        let Ok(Ok((_, html))) = fetched else {
            continue;
        };
        fallback_text = html_text(&html);
        let mut candidates = html_links(&html, &url)
            .into_iter()
            .filter(|(title, link)| {
                !title.trim().is_empty()
                    && link.starts_with("http")
                    && !link.contains("duckduckgo.com")
                    && !link.contains("bing.com")
                    && !link.contains("google.com")
                    && !link.contains("baidu.com")
            })
            .collect::<Vec<_>>();
        candidates.dedup_by(|left, right| left.1 == right.1);
        for (title, result_url) in candidates.into_iter().take(number as usize) {
            let content = if fetch_content {
                tokio::time::timeout(Duration::from_secs(10), fetch_page(&result_url))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|(_, page)| truncate_output(&html_text(&page)))
            } else {
                None
            };
            results.push(json!({
                "position": results.len() + 1,
                "title": title,
                "url": result_url,
                "description": "",
                "source": engine,
                "raw_content": content
            }));
        }
        if !results.is_empty() {
            source = engine;
            break;
        }
    }
    if results.is_empty() {
        return Ok(format!(
            "No parsed search results for '{query}'.\n{}",
            truncate_output(&fallback_text)
        ));
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(&json!({
            "query": query,
            "source": source,
            "total_results": results.len(),
            "results": results
        }))
        .unwrap_or_default(),
    ))
}

pub(crate) async fn run_crawl_tool(arguments: &Value) -> Result<String, String> {
    let urls = match arguments.get("urls") {
        Some(Value::String(url)) => vec![url.clone()],
        Some(Value::Array(urls)) => urls
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => return Err("rust_crawl4ai requires urls as a string or array".to_string()),
    };
    if urls.is_empty() {
        return Err("rust_crawl4ai requires at least one URL".to_string());
    }
    let threshold = arguments
        .get("word_count_threshold")
        .and_then(Value::as_u64)
        .unwrap_or(10) as usize;
    let timeout_secs = arguments
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(5, 120);
    let bypass_cache = arguments
        .get("bypass_cache")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut pages = Vec::new();
    for url in urls.into_iter().take(8) {
        match tokio::time::timeout(Duration::from_secs(timeout_secs), fetch_page(&url)).await {
            Ok(Ok((status, html))) => {
                let text = html_text(&html);
                if text.split_whitespace().count() >= threshold {
                    pages.push(json!({
                        "url": url,
                        "success": status.is_success(),
                        "status_code": status.as_u16(),
                        "title": html_title(&html),
                        "markdown": truncate_output(&text),
                        "word_count": text.split_whitespace().count(),
                        "links_count": html_links(&html, &url).len(),
                        "cache_bypassed": bypass_cache
                    }));
                } else {
                    pages.push(json!({"url": url, "success": status.is_success(), "status_code": status.as_u16(), "word_count": text.split_whitespace().count(), "markdown": text, "cache_bypassed": bypass_cache}));
                }
            }
            Ok(Err(error)) => pages.push(json!({"url": url, "success": false, "error_message": error})),
            Err(_) => pages.push(json!({"url": url, "success": false, "error_message": format!("crawl timed out after {timeout_secs}s")})),
        }
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(
            &json!({"crawler": "RustPilot lightweight Crawl4AI-compatible", "results": pages}),
        )
        .unwrap_or_default(),
    ))
}

pub(crate) async fn run_browser_tool(
    state: &AppState,
    arguments: &Value,
    namespace: &str,
) -> Result<String, String> {
    let action = string_argument(arguments, "action")
        .ok_or_else(|| "rust_browser_use requires action".to_string())?;
    let raw_session_id =
        string_argument(arguments, "session_id").unwrap_or_else(|| "default".to_string());
    let session_id = format!("{namespace}:{raw_session_id}");
    let mut session = state
        .browser_sessions
        .lock()
        .map_err(|_| "Browser session lock is poisoned".to_string())?
        .get(&session_id)
        .cloned()
        .unwrap_or_default();

    let output = match action.as_str() {
        "open" | "go_to_url" | "open_tab" => {
            let url = string_argument(arguments, "url")
                .ok_or_else(|| "browser open requires url".to_string())?;
            let status = load_browser_page(&mut session, url.clone()).await?;
            if session.history_index + 1 < session.history.len() {
                session.history.truncate(session.history_index + 1);
            }
            session.history.push(url);
            session.history_index = session.history.len().saturating_sub(1);
            let tab_id = session.tabs.len();
            session.tabs.push(BrowserTab {
                id: tab_id,
                url: session.current_url.clone(),
                title: session.title.clone(),
            });
            session.active_tab_id = tab_id;
            browser_state_output(&session, status.as_u16(), true)
        }
        "refresh" => {
            if session.current_url.is_empty() {
                return Err("No browser page is open.".to_string());
            }
            let url = session.current_url.clone();
            let status = load_browser_page(&mut session, url).await?;
            browser_state_output(&session, status.as_u16(), true)
        }
        "back" | "go_back" => {
            if session.history_index == 0 || session.history.is_empty() {
                return Err("Browser history has no previous page.".to_string());
            }
            session.history_index -= 1;
            let url = session.history[session.history_index].clone();
            let status = load_browser_page(&mut session, url).await?;
            browser_state_output(&session, status.as_u16(), true)
        }
        "forward" => {
            if session.history_index + 1 >= session.history.len() {
                return Err("Browser history has no next page.".to_string());
            }
            session.history_index += 1;
            let url = session.history[session.history_index].clone();
            let status = load_browser_page(&mut session, url).await?;
            browser_state_output(&session, status.as_u16(), true)
        }
        "extract" | "extract_content" => {
            let mut output = browser_state_output(&session, 200, true)?;
            if let Some(goal) = string_argument(arguments, "goal") {
                output.push_str(&format!("\nextraction_goal: {}", truncate_output(&goal)));
            }
            Ok(output)
        }
        "click" => {
            let needle = string_argument(arguments, "selector")
                .or_else(|| string_argument(arguments, "text"))
                .ok_or_else(|| "browser click requires selector or text".to_string())?;
            let link = html_links(&session.html, &session.current_url)
                .into_iter()
                .find(|(label, url)| label.contains(&needle) || url.contains(&needle))
                .ok_or_else(|| format!("No link matched '{needle}'."))?;
            let status = load_browser_page(&mut session, link.1.clone()).await?;
            if session.history_index + 1 < session.history.len() {
                session.history.truncate(session.history_index + 1);
            }
            session.history.push(link.1);
            session.history_index = session.history.len().saturating_sub(1);
            browser_state_output(&session, status.as_u16(), true)
        }
        "click_element" => {
            let index = arguments
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "click_element requires index".to_string())? as usize;
            let link = html_links(&session.html, &session.current_url)
                .into_iter()
                .nth(index)
                .ok_or_else(|| format!("Element with index {index} not found"))?;
            let status = load_browser_page(&mut session, link.1.clone()).await?;
            if session.history_index + 1 < session.history.len() {
                session.history.truncate(session.history_index + 1);
            }
            session.history.push(link.1);
            session.history_index = session.history.len().saturating_sub(1);
            browser_state_output(&session, status.as_u16(), true)
        }
        "type" | "input_text" => {
            let field = string_argument(arguments, "field")
                .or_else(|| string_argument(arguments, "selector"))
                .or_else(|| {
                    arguments
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|index| format!("element_{index}"))
                })
                .unwrap_or_else(|| "active".to_string());
            let text = string_argument(arguments, "text").unwrap_or_default();
            if action == "input_text" && text.is_empty() {
                return Err("input_text requires text".to_string());
            }
            session.typed_values.insert(field.clone(), text.clone());
            Ok(format!("Recorded input for {field}: {}", truncate_output(&text)))
        }
        "scroll" | "scroll_down" | "scroll_up" => {
            let raw_amount = arguments
                .get("amount")
                .or_else(|| arguments.get("scroll_amount"))
                .and_then(Value::as_i64)
                .unwrap_or(1);
            let amount = if action == "scroll_up" {
                -raw_amount.abs()
            } else {
                raw_amount.abs()
            };
            session.scroll_y = (session.scroll_y + amount * 600).max(0);
            browser_state_output(&session, 200, false)
        }
        "scroll_to_text" => {
            let text = string_argument(arguments, "text")
                .ok_or_else(|| "scroll_to_text requires text".to_string())?;
            if !html_text(&session.html)
                .to_lowercase()
                .contains(&text.to_lowercase())
            {
                return Err(format!("Text not found on current page: {text}"));
            }
            session.scroll_y = session.scroll_y.max(600);
            Ok(format!("Scrolled to text: '{text}'"))
        }
        "send_keys" => {
            let keys = string_argument(arguments, "keys")
                .ok_or_else(|| "send_keys requires keys".to_string())?;
            session
                .typed_values
                .insert("keyboard".to_string(), keys.clone());
            Ok(format!("Sent keys: {keys}"))
        }
        "get_dropdown_options" => {
            let options = html_select_options(&session.html);
            serde_json::to_string_pretty(&json!({"options": options}))
                .map_err(|error| format!("Unable to encode dropdown options: {error}"))
        }
        "select_dropdown_option" => {
            let index = arguments
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "select_dropdown_option requires index".to_string())?;
            let text = string_argument(arguments, "text")
                .ok_or_else(|| "select_dropdown_option requires text".to_string())?;
            session
                .typed_values
                .insert(format!("select_{index}"), text.clone());
            Ok(format!("Selected option '{text}' from dropdown at index {index}"))
        }
        "web_search" => {
            let query = string_argument(arguments, "query")
                .ok_or_else(|| "web_search requires query".to_string())?;
            let result = run_web_search_tool(&json!({
                "query": query,
                "num_results": 5,
                "fetch_content": true
            }))
            .await?;
            if let Ok(value) = serde_json::from_str::<Value>(&result) {
                if let Some(url) = value
                    .pointer("/results/0/url")
                    .and_then(Value::as_str)
                {
                    let status = load_browser_page(&mut session, url.to_string()).await?;
                    return browser_state_output(&session, status.as_u16(), true);
                }
            }
            Ok(result)
        }
        "wait" => {
            let seconds = arguments
                .get("seconds")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(30);
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            Ok(format!("Waited {seconds} second(s)."))
        }
        "switch_tab" => {
            let tab_id = arguments
                .get("tab_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| "switch_tab requires tab_id".to_string())?
                as usize;
            let tab = session
                .tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .cloned()
                .ok_or_else(|| format!("Tab {tab_id} not found"))?;
            let status = load_browser_page(&mut session, tab.url).await?;
            session.active_tab_id = tab_id;
            browser_state_output(&session, status.as_u16(), true)
        }
        "close_tab" => {
            let tab_id = arguments
                .get("tab_id")
                .and_then(Value::as_u64)
                .unwrap_or(session.active_tab_id as u64) as usize;
            session.tabs.retain(|tab| tab.id != tab_id);
            if session.tabs.is_empty() {
                session.current_url.clear();
                session.title.clear();
                session.html.clear();
            } else if let Some(tab) = session.tabs.last().cloned() {
                session.active_tab_id = tab.id;
                let _ = load_browser_page(&mut session, tab.url).await?;
            }
            Ok(format!("Closed tab {tab_id}."))
        }
        "screenshot" => serde_json::to_string_pretty(&json!({
            "session_id": raw_session_id,
            "url": session.current_url,
            "title": session.title,
            "visual_available": true,
            "image_path": capture_browser_screenshot(&session.current_url).await?.display().to_string()
        }))
        .map_err(|error| format!("Unable to encode browser state: {error}")),
        _ => Err(format!("Unsupported browser action: {action}")),
    }?;

    state
        .browser_sessions
        .lock()
        .map_err(|_| "Browser session lock is poisoned".to_string())?
        .insert(session_id, session);
    Ok(truncate_output(&output))
}

fn browser_state_output(
    session: &BrowserSession,
    status_code: u16,
    include_text: bool,
) -> Result<String, String> {
    let links = html_links(&session.html, &session.current_url);
    let text = if include_text {
        Some(truncate_output(&html_text(&session.html)))
    } else {
        None
    };
    serde_json::to_string_pretty(&json!({
        "url": session.current_url,
        "title": session.title,
        "status_code": status_code,
        "scroll_y": session.scroll_y,
        "history_index": session.history_index,
        "active_tab_id": session.active_tab_id,
        "tabs": &session.tabs,
        "interactive_elements": links.iter().enumerate().take(50).map(|(index, (label, url))| json!({"index": index, "type": "link", "text": label, "url": url})).collect::<Vec<_>>(),
        "links": links.iter().take(30).map(|(label, url)| json!({"text": label, "url": url})).collect::<Vec<_>>(),
        "text": text
    }))
    .map_err(|error| format!("Unable to encode browser state: {error}"))
}
