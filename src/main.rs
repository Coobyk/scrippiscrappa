use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use scraper::{Html, Selector};
use tokio::fs;
use tokio::sync::{mpsc, Mutex, Semaphore};

#[derive(Parser, Debug)]
#[command(name = "scrippiscrappa", version, about = "A fast, concurrent website scraper. Downloads everything.")]
struct Args {
    /// URL to scrape (scheme auto-prepended if missing)
    url: String,

    /// Number of concurrent connections
    #[arg(short = 'c', default_value_t = 8)]
    connections: usize,

    /// Maximum crawl depth (0 = unlimited)
    #[arg(short = 'd', default_value_t = 0)]
    depth: usize,

    /// Output directory (defaults to current directory)
    #[arg(short = 'o', default_value = ".")]
    output: PathBuf,
}

struct CrawlState {
    visited: Mutex<HashSet<String>>,
    stats: Stats,
    active_tasks: AtomicUsize,
    queued: AtomicUsize,
}

struct Stats {
    downloaded: AtomicU64,
    failed: AtomicU64,
    bytes: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }
}

fn normalize_url(input: &str) -> Result<String> {
    let with_scheme = if input.starts_with("http://") || input.starts_with("https://") {
        input.to_string()
    } else {
        format!("https://{}", input)
    };
    let parsed = url::Url::parse(&with_scheme).context("Invalid URL")?;
    Ok(parsed.to_string())
}

fn resolve_url(base: &url::Url, href: &str) -> Option<url::Url> {
    let href = href.trim();
    if href.is_empty()
        || href.starts_with('#')
        || href.starts_with("javascript:")
        || href.starts_with("data:")
        || href.starts_with("mailto:")
        || href.starts_with("tel:")
        || href.starts_with("ftp:")
        || href.starts_with("about:")
        || href.starts_with("blob:")
    {
        return None;
    }
    base.join(href).ok()
}

fn url_to_filepath(url: &url::Url) -> PathBuf {
    let host = url.host_str().unwrap_or("unknown");
    let path = url.path();

    let mut parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if parts.is_empty() {
        return PathBuf::from(host).join("index.html");
    }

    let last = parts.last().copied().unwrap_or("");
    let has_extension = last.contains('.');

    if !has_extension {
        parts.push("index.html");
    }

    let mut p = PathBuf::from(host);
    for part in parts {
        p.push(part);
    }
    p
}

fn extract_links(html: &str, base: &url::Url, base_domain: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let mut urls = Vec::new();

    let selectors: &[(&str, &str)] = &[
        ("a", "href"),
        ("link", "href"),
        ("script", "src"),
        ("img", "src"),
        ("img", "srcset"),
        ("source", "src"),
        ("source", "srcset"),
        ("video", "src"),
        ("video", "poster"),
        ("audio", "src"),
        ("embed", "src"),
        ("object", "data"),
        ("iframe", "src"),
        ("frame", "src"),
        ("meta[http-equiv='refresh']", "content"),
    ];

    for (tag, attr) in selectors {
        let selector_str = if tag.contains('[') {
            tag.to_string()
        } else {
            format!("{}[{}]", tag, attr)
        };
        let sel = match Selector::parse(&selector_str) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for element in document.select(&sel) {
            if let Some(val) = element.value().attr(attr) {
                if *attr == "srcset" {
                    for part in val.split(',') {
                        let trimmed = part.trim();
                        let url_part = trimmed.split_whitespace().next().unwrap_or("");
                        if let Some(resolved) = resolve_url(base, url_part) {
                            if resolved.domain() == Some(base_domain) {
                                urls.push(resolved.to_string());
                            }
                        }
                    }
                } else if *attr == "content" && *tag == "meta[http-equiv='refresh']" {
                    if let Some(url_part) =
                        val.split(';').find(|s| s.trim().starts_with("url="))
                    {
                        let href = url_part.trim().strip_prefix("url=").unwrap_or("");
                        if let Some(resolved) = resolve_url(base, href) {
                            if resolved.domain() == Some(base_domain) {
                                urls.push(resolved.to_string());
                            }
                        }
                    }
                } else if let Some(resolved) = resolve_url(base, val) {
                    if resolved.domain() == Some(base_domain) {
                        urls.push(resolved.to_string());
                    }
                }
            }
        }
    }

    urls
}

fn extract_css_urls(css: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut remaining = css;
    while let Some(start) = remaining.find("url(") {
        remaining = &remaining[start + 4..];
        let end = remaining.find(')').unwrap_or(remaining.len());
        let inner = remaining[..end]
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .trim();
        if !inner.is_empty() {
            urls.push(inner.to_string());
        }
        remaining = &remaining[end..];
    }
    urls
}

async fn download_url(client: &reqwest::Client, url: &str) -> Result<(Vec<u8>, String)> {
    let resp = client
        .get(url)
        .header("Accept", "*/*")
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = resp
        .bytes()
        .await
        .with_context(|| format!("reading body of {}", url))?;
    Ok((body.to_vec(), content_type))
}

async fn save_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, data).await?;
    Ok(())
}

fn print_progress(state: &CrawlState, start: Instant) {
    let dl = state.stats.downloaded.load(Ordering::Relaxed);
    let q = state.queued.load(Ordering::Relaxed);
    let active = state.active_tasks.load(Ordering::Relaxed);
    let fail = state.stats.failed.load(Ordering::Relaxed);
    let bytes = state.stats.bytes.load(Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    let speed = if elapsed > 0.0 {
        dl as f64 / elapsed
    } else {
        0.0
    };
    let mb = bytes as f64 / (1024.0 * 1024.0);

    eprint!(
        "\r\x1b[K  dl: {} | queue: {} | active: {} | fail: {} | {:.1} MB | {:.1} f/s | {:.0}s",
        dl, q, active, fail, mb, speed, elapsed
    );
}

fn enqueue_url(
    url: String,
    depth: usize,
    max_depth: usize,
    state: &CrawlState,
    tx: &mpsc::UnboundedSender<(String, usize)>,
) {
    if max_depth > 0 && depth > max_depth {
        return;
    }
    state.queued.fetch_add(1, Ordering::Relaxed);
    let _ = tx.send((url, depth));
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let start_url = normalize_url(&args.url)?;
    let parsed_start = url::Url::parse(&start_url)?;
    let base_domain = parsed_start
        .domain()
        .context("URL has no domain")?
        .to_string();

    eprintln!("scrippiscrappa");
    eprintln!("  url:         {}", start_url);
    eprintln!("  domain:      {}", base_domain);
    eprintln!("  connections: {}", args.connections);
    eprintln!(
        "  depth:       {}",
        if args.depth == 0 {
            "unlimited".to_string()
        } else {
            args.depth.to_string()
        }
    );
    eprintln!("  output:      {}", args.output.display());
    eprintln!();

    let output_dir = args.output.canonicalize().unwrap_or(args.output.clone());

    let client = reqwest::Client::builder()
        .user_agent("scrippiscrappa/0.1.0")
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()?;

    let state = Arc::new(CrawlState {
        visited: Mutex::new(HashSet::new()),
        stats: Stats::new(),
        active_tasks: AtomicUsize::new(0),
        queued: AtomicUsize::new(0),
    });

    let semaphore = Arc::new(Semaphore::new(args.connections));

    let (tx, mut rx) = mpsc::unbounded_channel::<(String, usize)>();

    // Mark seed as visited and enqueue
    {
        state.visited.lock().await.insert(start_url.clone());
    }
    state.queued.fetch_add(1, Ordering::Relaxed);
    tx.send((start_url, 0))?;

    let start_time = Instant::now();

    // Progress ticker
    let state_tick = state.clone();
    let tick = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            interval.tick().await;
            print_progress(&state_tick, start_time);
            if state_tick.queued.load(Ordering::Relaxed) == 0
                && state_tick.active_tasks.load(Ordering::Relaxed) == 0
            {
                break;
            }
        }
    });

    while let Some((url, depth)) = rx.recv().await {
        let permit = semaphore.clone().acquire_owned().await?;

        state.active_tasks.fetch_add(1, Ordering::Relaxed);

        let client = client.clone();
        let output_dir = output_dir.clone();
        let base_domain = base_domain.clone();
        let state = state.clone();
        let tx = tx.clone();
        let max_depth = args.depth;

        tokio::spawn(async move {
            let _permit = permit;

            match download_url(&client, &url).await {
                Ok((body, content_type)) => {
                    let parsed = url::Url::parse(&url).ok();
                    let is_same_domain = parsed
                        .as_ref()
                        .and_then(|u| u.domain())
                        .map(|d| d == base_domain)
                        .unwrap_or(false);

                    // Save file
                    let filepath = parsed
                        .as_ref()
                        .map(url_to_filepath)
                        .unwrap_or_else(|| PathBuf::from("unknown"));
                    let full_path = output_dir.join(&filepath);
                    if let Err(e) = save_file(&full_path, &body).await {
                        eprintln!("\n  error saving {}: {}", filepath.display(), e);
                        state.stats.failed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        state.stats.downloaded.fetch_add(1, Ordering::Relaxed);
                        state.stats.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                    }

                    // Extract links from HTML
                    if is_same_domain && content_type.contains("text/html") {
                        if let (Ok(html), Some(p)) = (std::str::from_utf8(&body), parsed.as_ref())
                        {
                            let links = extract_links(html, p, &base_domain);
                            for link in links {
                                let mut visited = state.visited.lock().await;
                                if visited.insert(link.clone()) {
                                    drop(visited);
                                    enqueue_url(link, depth + 1, max_depth, &state, &tx);
                                }
                            }
                        }
                    }

                    // Extract URLs from CSS
                    if is_same_domain && content_type.contains("text/css") {
                        if let (Ok(css), Some(p)) = (std::str::from_utf8(&body), parsed.as_ref()) {
                            for href in extract_css_urls(css) {
                                if let Some(resolved) = resolve_url(p, &href) {
                                    if resolved.domain() == Some(base_domain.as_str()) {
                                        let r = resolved.to_string();
                                        let mut visited = state.visited.lock().await;
                                        if visited.insert(r.clone()) {
                                            drop(visited);
                                            enqueue_url(r, depth + 1, max_depth, &state, &tx);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("\n  failed: {} — {}", url, e);
                    state.stats.failed.fetch_add(1, Ordering::Relaxed);
                }
            }

            state.queued.fetch_sub(1, Ordering::Relaxed);
            state.active_tasks.fetch_sub(1, Ordering::Relaxed);
        });
    }

    // Wait for progress ticker to finish
    let _ = tick.await;

    print_progress(&state, start_time);
    eprintln!();

    let elapsed = start_time.elapsed();
    let dl = state.stats.downloaded.load(Ordering::Relaxed);
    let fail = state.stats.failed.load(Ordering::Relaxed);
    let bytes = state.stats.bytes.load(Ordering::Relaxed);

    eprintln!();
    eprintln!("done in {:.1}s", elapsed.as_secs_f64());
    eprintln!("  downloaded: {}", dl);
    eprintln!("  failed:     {}", fail);
    eprintln!("  total:      {:.2} MB", bytes as f64 / (1024.0 * 1024.0));

    Ok(())
}
