use std::{
    collections::{BTreeMap, HashSet},
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use clap::Parser;
use reqwest::Client;
use sanitize_filename::sanitize;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    sync::{Mutex, Semaphore, mpsc},
    time,
};
use url::Url;

fn default_repetition_threshold() -> usize {
    4
}

#[derive(Parser, Debug, Serialize, Deserialize, Clone)]
#[clap(name = "scrippiscrappa", version)]
struct Args {
    #[clap(help = "Start URL to scrape")]
    url: String,
    #[clap(
        long,
        help = "Comma-separated list of URLs to scrape in batch mode",
        value_delimiter = ','
    )]
    batch: Option<Vec<String>>,
    #[clap(short, long, help = "Output directory name")]
    output: Option<String>,
    #[clap(short = 's', long, help = "Save remaining queue to file")]
    save_queue: Option<String>,
    #[clap(short, long, default_value_t = 8, help = "Parallel download count")]
    concurrency: usize,
    #[clap(short = 'r', long, help = "Resume from saved state file")]
    resume: Option<String>,
    #[clap(long, help = "Force CI output (now default behavior)")]
    ci: bool,
    #[clap(
        short = 'f',
        long,
        help = "Force scraping of URLs with fragments (parts after #)"
    )]
    force_fragments: bool,
    #[clap(
        short = 'q',
        long,
        help = "Force scraping of URLs with query parameters (parts after ?)"
    )]
    force_queries: bool,
    #[clap(
        short = 'i',
        long,
        help = "Ignore URLs containing these patterns (can be specified multiple times)",
        num_args = 1..,
        value_delimiter = ','
    )]
    ignore: Vec<String>,
    #[clap(
        short = 'd',
        long,
        help = "Allowed subdomains (comma-separated, e.g. 'blog,docs'). If not specified, only the main domain is allowed.",
        num_args = 1..,
        value_delimiter = ','
    )]
    subdomains: Vec<String>,
    #[clap(
        long,
        default_value_t = 4,
        help = "Repetition threshold for path segments"
    )]
    #[serde(default = "default_repetition_threshold")]
    repetition_threshold: usize,
    #[clap(
        short = 'a',
        long,
        help = "Allowed alternative domains (comma-separated).",
        num_args = 1..,
        value_delimiter = ','
    )]
    alternative_domains: Vec<String>,
}

struct AppState {
    queue: Vec<(String, String)>,             // (url, base_folder)
    alternative_queue: Vec<(String, String)>, // (url, base_folder)
    in_progress: Vec<String>,
    completed: Vec<String>,
    completion_times: Vec<Instant>,
}

impl AppState {
    fn new() -> Self {
        Self {
            queue: Vec::new(),
            alternative_queue: Vec::new(),
            in_progress: Vec::new(),
            completed: Vec::new(),
            completion_times: Vec::new(),
        }
    }
    fn enqueue(&mut self, url: String, base: String) {
        self.queue.push((url, base));
    }
    fn enqueue_alternative(&mut self, url: String, base: String) {
        self.alternative_queue.push((url, base));
    }
    fn dequeue(&mut self) -> Option<(String, String)> {
        if self.queue.is_empty() {
            None
        } else {
            // pick lexicographically smallest URL based on host/path
            let min_idx = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, (u, _))| {
                    u.strip_prefix("https://")
                        .or_else(|| u.strip_prefix("http://"))
                        .unwrap_or(u)
                })
                .map(|(i, _)| i)
                .unwrap();
            Some(self.queue.remove(min_idx))
        }
    }
    fn dequeue_alternative(&mut self) -> Option<(String, String)> {
        if self.alternative_queue.is_empty() {
            None
        } else {
            // pick lexicographically smallest URL based on host/path
            let min_idx = self
                .alternative_queue
                .iter()
                .enumerate()
                .min_by_key(|(_, (u, _))| {
                    u.strip_prefix("https://")
                        .or_else(|| u.strip_prefix("http://"))
                        .unwrap_or(u)
                })
                .map(|(i, _)| i)
                .unwrap();
            Some(self.alternative_queue.remove(min_idx))
        }
    }
    fn start(&mut self, url: String) {
        self.in_progress.push(url);
    }
    fn finish(&mut self, url: &String) {
        self.in_progress.retain(|u| u != url);
        self.completed.push(url.clone());
        self.completion_times.push(Instant::now());
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct SavedState {
    args: Args,
    queue: Vec<String>,
    alternative_queue: Vec<String>,
    in_progress: Vec<String>,
    completed: Vec<String>,
}

fn get_local_path(url: &Url, base: &str) -> PathBuf {
    let mut path = PathBuf::from(base);
    if let Some(host) = url.host_str() {
        path.push(sanitize(host));
    }
    let mut segments: Vec<&str> = url.path_segments().map(|c| c.collect()).unwrap_or_default();
    if segments.is_empty() || url.path().ends_with('/') {
        for seg in &segments {
            if !seg.is_empty() {
                path.push(sanitize(seg));
            }
        }
        path.push("index.html");
    } else {
        let last = segments.pop().unwrap();
        for seg in &segments {
            if !seg.is_empty() {
                path.push(sanitize(seg));
            }
        }
        let name = sanitize(last);
        if name.contains('.') {
            path.push(name);
        } else {
            // Common document extensions
            let doc_extensions = ["html", "htm", "xhtml", "xml", "php", "asp", "aspx", "jsp"];
            let mut found = false;
            for ext in doc_extensions {
                if name.ends_with(ext) {
                    found = true;
                    break;
                }
            }
            path.push(name);
            if !found {
                path.set_extension("html");
            }
        }
    }
    path
}

async fn process_url(
    client: &Client,
    url: &str,
    base: &str,
    state: Arc<Mutex<AppState>>,
    visited: Arc<Mutex<HashSet<String>>>,
    start_host: &str,
    force_fragments: bool,
    force_queries: bool,
    ignore_patterns: &[String],
    allowed_subdomains: &[String],
    alternative_domains: &[String],
    repetition_threshold: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Skip URLs with repeated path segments
    if has_repeated_segments(url, repetition_threshold) {
        return Ok(());
    }
    // Skip URLs containing any of the ignore patterns
    if ignore_patterns.iter().any(|pattern| url.contains(pattern)) {
        return Ok(());
    }
    let mut parsed = Url::parse(url)?;
    if !force_fragments {
        parsed.set_fragment(None);
    }
    if !force_queries {
        parsed.set_query(None);
    }
    let resp = client.get(parsed.as_str()).send().await?;
    let headers = resp.headers().clone();
    let content = resp.bytes().await?;
    let local_path = get_local_path(&parsed, base);
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&local_path, &content).await?;
    if let Some(ct) = headers.get(reqwest::header::CONTENT_TYPE) {
        if let Ok(ct_str) = ct.to_str() {
            // Check for various document types
            if ct_str.contains("text/html")
                || ct_str.contains("application/xhtml+xml")
                || ct_str.contains("text/xml")
                || ct_str.contains("application/xml")
                || ct_str.contains("text/plain")
            {
                let mut vis = visited.lock().await;
                // collect links before locks to avoid holding non-Send refs
                let to_enqueue = {
                    let html = String::from_utf8_lossy(&content).to_string();
                    let document = Html::parse_document(&html);
                    let selectors = vec![
                        ("a", "href"),
                        ("img", "src"),
                        ("img", "srcset"),
                        ("source", "src"),
                        ("source", "srcset"),
                        ("video", "src"),
                        ("video", "poster"),
                        ("audio", "src"),
                        ("track", "src"),
                        ("embed", "src"),
                        ("object", "data"),
                        ("iframe", "src"),
                        ("frame", "src"),
                        ("script", "src"),
                        ("link", "href"), // catch all link rel types
                        ("meta[http-equiv=\"refresh\"]", "content"),
                    ];
                    let mut links = Vec::new();
                    for (sel_str, attr) in &selectors {
                        let selector = Selector::parse(sel_str).unwrap();
                        for element in document.select(&selector) {
                            if *attr == "srcset" {
                                if let Some(srcset) = element.value().attr(attr) {
                                    for src in srcset.split(',') {
                                        let src =
                                            src.trim().split_whitespace().next().unwrap_or("");
                                        if !src.is_empty() {
                                            if let Ok(link_url) = parsed.join(src) {
                                                if let Some(link_host) = link_url.host_str() {
                                                    if is_allowed_host(
                                                        link_host,
                                                        start_host,
                                                        allowed_subdomains,
                                                        alternative_domains,
                                                    ) {
                                                        links.push(link_url.as_str().to_string());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else if *sel_str == "meta[http-equiv=\"refresh\"]"
                                && *attr == "content"
                            {
                                // Parse meta refresh for URL
                                if let Some(content) = element.value().attr(attr) {
                                    if let Some(idx) = content.find("url=") {
                                        let url_part = &content[idx + 4..];
                                        if let Ok(link_url) = parsed.join(url_part) {
                                            if let Some(link_host) = link_url.host_str() {
                                                if is_allowed_host(
                                                    link_host,
                                                    start_host,
                                                    allowed_subdomains,
                                                    alternative_domains,
                                                ) {
                                                    links.push(link_url.as_str().to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                if let Some(link) = element.value().attr(attr) {
                                    if let Ok(link_url) = parsed.join(link) {
                                        if let Some(link_host) = link_url.host_str() {
                                            if is_allowed_host(
                                                link_host,
                                                start_host,
                                                allowed_subdomains,
                                                alternative_domains,
                                            ) {
                                                links.push(link_url.as_str().to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    links
                };
                let mut st = state.lock().await;
                for link in to_enqueue {
                    if vis.insert(link.clone()) {
                        if let Ok(link_url) = Url::parse(&link) {
                            if let Some(link_host) = link_url.host_str() {
                                if alternative_domains.iter().any(|d| d == link_host) {
                                    st.enqueue_alternative(link, base.to_string());
                                } else {
                                    st.enqueue(link, base.to_string());
                                }
                            } else {
                                st.enqueue(link, base.to_string());
                            }
                        } else {
                            st.enqueue(link, base.to_string());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Check if a host is allowed based on the start host and allowed subdomains
fn is_allowed_host(
    host: &str,
    start_host: &str,
    allowed_subdomains: &[String],
    alternative_domains: &[String],
) -> bool {
    // If the host matches the start host exactly, it's allowed
    if host == start_host {
        return true;
    }

    // Check if the host is in the list of alternative domains
    if alternative_domains.iter().any(|d| d == host) {
        return true;
    }

    // If no subdomains are allowed, only the main domain is allowed
    if allowed_subdomains.is_empty() {
        return false;
    }

    // Check if the host ends with the start host (i.e., is a subdomain)
    if !host.ends_with(start_host) {
        return false;
    }

    // Extract the subdomain part
    let subdomain = host.strip_suffix(start_host).unwrap_or(host);
    let subdomain = subdomain.strip_suffix('.').unwrap_or(subdomain);

    // Check if the subdomain is in the allowed list
    allowed_subdomains.iter().any(|s| s == subdomain)
}

/// Check if a URL has repeated path segments
fn has_repeated_segments(url: &str, threshold: usize) -> bool {
    if let Ok(parsed) = Url::parse(url) {
        if let Some(segments) = parsed.path_segments() {
            let segments: Vec<_> = segments.collect();
            if segments.is_empty() {
                return false;
            }
            // Count the occurrences of each path segment
            let mut counts = BTreeMap::new();
            for segment in segments {
                *counts.entry(segment).or_insert(0) += 1;
            }
            // Check if any segment count exceeds the threshold
            for &count in counts.values() {
                if count >= threshold {
                    return true;
                }
            }
        }
    }
    false
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();

    // Always run in CI mode
    let ci_mode = true;

    // Display a helpful message when using the --ci flag
    if args.ci {
        println!("The --ci flag is now useless since CI mode is default!");
        println!("Just run the command normally without any special flags.");
        println!();
    }

    // Determine starting URLs and their output folders
    let start_urls: Vec<String> = if let Some(batch) = &args.batch {
        batch.clone()
    } else {
        vec![args.url.clone()]
    };
    // Generate a base folder for each starting URL
    let base_folders: Vec<String> = start_urls
        .iter()
        .map(|url| {
            args.output.clone().unwrap_or_else(|| {
                Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_string()))
                    .unwrap_or("output".to_string())
            })
        })
        .collect();
    // Create all output folders
    for base in &base_folders {
        fs::create_dir_all(base).await?;
    }
    let state = Arc::new(Mutex::new(AppState::new()));
    let visited = Arc::new(Mutex::new(HashSet::new()));
    let (tx, mut rx) = mpsc::channel(100);
    // Use the first URL's host as the start_host for filtering
    let start_host = Url::parse(&start_urls[0])?.host_str().unwrap().to_string();
    {
        let mut vis = visited.lock().await;
        for url in &start_urls {
            vis.insert(url.clone());
        }
    }
    {
        let mut st = state.lock().await;
        for (url, base) in start_urls.iter().zip(base_folders.iter()) {
            st.enqueue(url.clone(), base.clone());
        }
    }
    if let Some(resume_file) = &args.resume {
        // load saved state
        let data = fs::read_to_string(resume_file).await?;
        let saved: SavedState = serde_json::from_str(&data)?;
        // override args and state
        args = saved.args.clone();
        {
            let mut st = state.lock().await;
            // Recompute base folders for each URL in the saved queue
            let base_folders: Vec<String> = saved
                .queue
                .iter()
                .map(|url| {
                    args.output.clone().unwrap_or_else(|| {
                        Url::parse(url)
                            .ok()
                            .and_then(|u| u.host_str().map(|h| h.to_string()))
                            .unwrap_or("output".to_string())
                    })
                })
                .collect();
            st.queue = saved
                .queue
                .iter()
                .zip(base_folders.iter())
                .map(|(u, b)| (u.clone(), b.clone()))
                .collect();
            st.alternative_queue = saved
                .alternative_queue
                .iter()
                .zip(base_folders.iter())
                .map(|(u, b)| (u.clone(), b.clone()))
                .collect();
            st.in_progress = saved.in_progress.clone();
            st.completed = saved.completed.clone();
        }
        {
            let mut vis = visited.lock().await;
            vis.clear();
            let st = state.lock().await;
            for (u, _) in st.queue.iter() {
                vis.insert(u.clone());
            }
            for u in st.in_progress.iter() {
                vis.insert(u.clone());
            }
            for u in st.completed.iter() {
                vis.insert(u.clone());
            }
        }
    }
    // setup Ctrl+C flag
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.store(true, Ordering::SeqCst);
        });
    }
    let client = Client::builder()
        .user_agent("scrippiscrappa")
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90))
        .danger_accept_invalid_certs(true)
        .build()?;
    let semaphore = Arc::new(Semaphore::new(args.concurrency));
    let mut processing_alternative_queue = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let next = {
            let mut st = state.lock().await;
            if !processing_alternative_queue {
                if st.queue.is_empty() {
                    // Only switch to alternative queue if there are no tasks in progress
                    if st.in_progress.is_empty() {
                        processing_alternative_queue = true;
                        st.dequeue_alternative()
                    } else {
                        // Wait for tasks to finish before switching queues
                        None
                    }
                } else {
                    st.dequeue()
                }
            } else {
                st.dequeue_alternative()
            }
        };
        if let Some((url, base)) = next {
            if std::env::var("CI").is_ok() {
                let st = state.lock().await;
                let total = st.queue.len() + st.in_progress.len() + st.completed.len();
                eprintln!(
                    "Scraping {} (Queue: {}, In Progress: {}, Completed: {}, Total: {})",
                    url,
                    st.queue.len(),
                    st.in_progress.len(),
                    st.completed.len(),
                    total
                );
            }
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let state_clone = state.clone();
            let visited_clone = visited.clone();
            let client_clone = client.clone();
            let base = base.clone();
            let start_host = start_host.clone();
            let shutdown_task = shutdown.clone();
            let force_fragments = args.force_fragments;
            let force_queries = args.force_queries;
            let ignore_patterns = args.ignore.clone();
            let allowed_subdomains = args.subdomains.clone();
            let alternative_domains = args.alternative_domains.clone();
            let repetition_threshold = args.repetition_threshold;
            let url_clone = url.clone();
            let tx_clone = tx.clone();
            tokio::spawn(async move {
                {
                    let mut st = state_clone.lock().await;
                    st.start(url_clone.clone());
                }
                if !shutdown_task.load(Ordering::SeqCst) {
                    // Retry on transient failures up to 10 attempts
                    let mut attempt = 0;
                    const MAX_RETRIES: usize = 10;
                    while attempt < MAX_RETRIES && !shutdown_task.load(Ordering::SeqCst) {
                        match process_url(
                            &client_clone,
                            &url_clone,
                            &base,
                            state_clone.clone(),
                            visited_clone.clone(),
                            &start_host,
                            force_fragments,
                            force_queries,
                            &ignore_patterns,
                            &allowed_subdomains,
                            &alternative_domains,
                            repetition_threshold,
                        )
                        .await
                        {
                            Ok(_) => {
                                if ci_mode {
                                    eprintln!("Successfully saved {}", url_clone);
                                }
                                break;
                            }
                            Err(e) => {
                                attempt += 1;
                                if attempt >= MAX_RETRIES {
                                    eprintln!(
                                        "Failed processing {} after {} attempts: {}",
                                        url_clone, MAX_RETRIES, e
                                    );
                                } else {
                                    eprintln!(
                                        "Error processing {}: {}. Retrying {}/{}",
                                        url_clone, e, attempt, MAX_RETRIES
                                    );
                                    time::sleep(Duration::from_secs(2)).await;
                                }
                            }
                        }
                    }
                }
                {
                    let mut st = state_clone.lock().await;
                    st.finish(&url_clone);
                }
                let _ = tx_clone.send(()).await;
                drop(permit);
            });
            // Give the spawned task a chance to run
            time::sleep(Duration::from_millis(10)).await;
        } else {
            if let Ok(_) = rx.try_recv() {
                // Task finished, continue with the loop to process next URL
            } else {
                let st = state.lock().await;
                if st.queue.is_empty()
                    && st.alternative_queue.is_empty()
                    && st.in_progress.is_empty()
                {
                    std::process::exit(0);
                }
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    }
    if shutdown.load(Ordering::SeqCst) {
        // auto-save or prompt
        if let Some(path) = &args.save_queue {
            save_state(&state, &args, path).await?;
            std::process::exit(0);
        } else {
            println!("Save progress? (y/N): ");
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            if input.trim().eq_ignore_ascii_case("y") {
                println!("Enter file path to save state: ");
                input.clear();
                io::stdin().read_line(&mut input)?;
                let path = input.trim();
                save_state(&state, &args, path).await?;
                std::process::exit(0);
            }
        }
    }
    Ok(())
}

/// Save current state and args to JSON file
async fn save_state(
    state: &Arc<Mutex<AppState>>,
    args: &Args,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let st = state.lock().await;
    let saved = SavedState {
        args: args.clone(),
        queue: st.queue.iter().map(|(u, _)| u.clone()).collect(),
        alternative_queue: st
            .alternative_queue
            .iter()
            .map(|(u, _)| u.clone())
            .collect(),
        in_progress: st.in_progress.clone(),
        completed: st.completed.clone(),
    };
    let content = serde_json::to_string_pretty(&saved)?;
    fs::write(path, content).await?;
    println!("State saved to {}", path);
    Ok(())
}
