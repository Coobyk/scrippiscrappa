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
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use reqwest::Client;
use sanitize_filename::sanitize;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    sync::{mpsc, Mutex, Semaphore},
    time,
};
use tui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    text::{Span, Spans},
    widgets::{Block, Borders, List, ListItem},
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
    #[clap(long, help = "Force CI output")]
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
    #[clap(long, default_value_t = 4, help = "Repetition threshold for path segments")]
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
    queue: Vec<(String, String)>, // (url, base_folder)
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
fn is_allowed_host(host: &str, start_host: &str, allowed_subdomains: &[String], alternative_domains: &[String]) -> bool {
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

fn draw_ui<B: tui::backend::Backend>(f: &mut tui::Frame<B>, st: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(80), Constraint::Percentage(20)].as_ref())
        .split(f.size());

    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)].as_ref())
        .split(chunks[0]);

    // build and render queue as a tree
    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
    }
    let mut tree_map: BTreeMap<String, Node> = BTreeMap::new();
    for (url, _base) in &st.queue {
        if let Ok(parsed) = Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                let mut node = tree_map.entry(host.to_string()).or_default();
                for seg in parsed
                    .path_segments()
                    .map(|c| c.collect::<Vec<_>>())
                    .unwrap_or_default()
                    .iter()
                    .filter(|s| !s.is_empty())
                {
                    node = node.children.entry(seg.to_string()).or_default();
                }
            }
        }
    }
    fn traverse(
        children: &BTreeMap<String, Node>,
        prefix: &str,
        is_last: bool,
        lines: &mut Vec<String>,
    ) {
        let keys: Vec<_> = children.keys().collect();
        for (i, key) in keys.iter().enumerate() {
            let last = i == keys.len() - 1;
            let mut line = prefix.to_string();
            if is_last {
                line.push_str("    ");
            } else {
                line.push_str("│   ");
            }
            line.push_str(if last { "└── " } else { "├── " });
            line.push_str(key);
            lines.push(line.clone());
            traverse(
                &children[key.as_str()].children,
                &(prefix.to_string() + if is_last { "    " } else { "│   " }),
                last,
                lines,
            );
        }
    }
    let mut lines_vec = Vec::new();
    let hosts: Vec<_> = tree_map.keys().collect();
    for (i, host) in hosts.iter().enumerate() {
        let last_host = i == hosts.len() - 1;
        lines_vec.push(host.to_string());
        traverse(
            &tree_map[host.as_str()].children,
            "",
            last_host,
            &mut lines_vec,
        );
    }
    let queue_items: Vec<ListItem> = lines_vec.into_iter().map(ListItem::new).collect();
    let inprog_items: Vec<ListItem> = st
        .in_progress
        .iter()
        .map(|u| {
            let disp = u
                .strip_prefix("https://")
                .or_else(|| u.strip_prefix("http://"))
                .unwrap_or(u);
            ListItem::new(disp.to_string())
        })
        .collect();
    let queue_list = List::new(queue_items).block(Block::default().borders(Borders::ALL).title(
        Spans::from(Span::raw(format!("Queue ({})", st.queue.len()))),
    ));
    f.render_widget(queue_list, top_chunks[0]);
    // calculate rate based on last 10 seconds
    let now = Instant::now();
    let ten_seconds_ago = now - Duration::from_secs(10);
    let recent_completions = st
        .completion_times
        .iter()
        .filter(|&&t| t >= ten_seconds_ago)
        .count();
    let rate = recent_completions as f64 / 10.0;
    let inprog_list = List::new(inprog_items).block(Block::default().borders(Borders::ALL).title(
        Spans::from(Span::raw(format!(
            "In Progress ({}) {:.2} sites/s",
            st.in_progress.len(),
            rate
        ))),
    ));
    // render in-progress list in the right column
    f.render_widget(inprog_list, top_chunks[1]);

    let mut alt_tree_map: BTreeMap<String, Node> = BTreeMap::new();
    for (url, _base) in &st.alternative_queue {
        if let Ok(parsed) = Url::parse(url) {
            if let Some(host) = parsed.host_str() {
                let mut node = alt_tree_map.entry(host.to_string()).or_default();
                for seg in parsed
                    .path_segments()
                    .map(|c| c.collect::<Vec<_>>())
                    .unwrap_or_default()
                    .iter()
                    .filter(|s| !s.is_empty())
                {
                    node = node.children.entry(seg.to_string()).or_default();
                }
            }
        }
    }

    let mut alt_lines_vec = Vec::new();
    let alt_hosts: Vec<_> = alt_tree_map.keys().collect();
    for (i, host) in alt_hosts.iter().enumerate() {
        let last_host = i == alt_hosts.len() - 1;
        alt_lines_vec.push(host.to_string());
        traverse(
            &alt_tree_map[host.as_str()].children,
            "",
            last_host,
            &mut alt_lines_vec,
        );
    }

    let alt_queue_items: Vec<ListItem> = alt_lines_vec.into_iter().map(ListItem::new).collect();
    let alt_queue_list = List::new(alt_queue_items).block(Block::default().borders(Borders::ALL).title(
        Spans::from(Span::raw(format!("Alternative Queue ({})", st.alternative_queue.len()))),
    ));
    f.render_widget(alt_queue_list, chunks[1]);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();
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
    // spawn key listener for Ctrl+C in raw mode
    {
        let shutdown = shutdown.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                if event::poll(Duration::from_millis(200)).unwrap_or(false) {
                    if let Ok(Event::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    })) = event::read()
                    {
                        shutdown.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }
    // setup TUI (skip in CI or when --ci flag is set)
    if !args.ci && std::env::var("CI").is_err() {
        let ui_state = state.clone();
        let shutdown_ui = shutdown.clone();
        let mut stdout = io::stdout();
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        tokio::spawn(async move {
            loop {
                {
                    let st = ui_state.lock().await;
                    terminal.draw(|f| draw_ui(f, &st)).unwrap();
                    if st.queue.is_empty() && st.alternative_queue.is_empty() && st.in_progress.is_empty() {
                        break;
                    }
                }
                if shutdown_ui.load(Ordering::SeqCst) {
                    break;
                }
                time::sleep(Duration::from_millis(200)).await;
            }
            disable_raw_mode().unwrap();
            let mut stdout = io::stdout();
            execute!(stdout, LeaveAlternateScreen).unwrap();
        });
    }
    let client = Client::builder()
        .user_agent("scrippiscrappa")
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90))
        .danger_accept_invalid_certs(true)
        .build()?;
    let semaphore = Arc::new(Semaphore::new(args.concurrency));
    let ci_mode = args.ci;
    let mut processing_alternative_queue = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let next = {
            let mut st = state.lock().await;
            if !processing_alternative_queue {
                if st.queue.is_empty() {
                    processing_alternative_queue = true;
                    st.dequeue_alternative()
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
        } else {
            if let Ok(_) = rx.try_recv() {
                // Task finished
            } else {
                let st = state.lock().await;
                if st.queue.is_empty() && st.alternative_queue.is_empty() && st.in_progress.is_empty() {
                    eprintln!("All URLs processed. Exiting.");
                    // Properly clean up terminal state before exiting
                    if !args.ci && std::env::var("CI").is_err() {
                        // Disable raw mode and leave alternate screen
                        disable_raw_mode().unwrap_or(());
                        let mut stdout = io::stdout();
                        execute!(stdout, LeaveAlternateScreen).unwrap_or(());
                    }
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
            // Properly clean up terminal state before exiting
            if !args.ci && std::env::var("CI").is_err() {
                disable_raw_mode().unwrap_or(());
                let mut stdout = io::stdout();
                execute!(stdout, LeaveAlternateScreen).unwrap_or(());
            }
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
                // Properly clean up terminal state before exiting
                if !args.ci && std::env::var("CI").is_err() {
                    disable_raw_mode().unwrap_or(());
                    let mut stdout = io::stdout();
                    execute!(stdout, LeaveAlternateScreen).unwrap_or(());
                }
                std::process::exit(0);
            } else {
                // Clean up terminal even if user doesn't save
                if !args.ci && std::env::var("CI").is_err() {
                    disable_raw_mode().unwrap_or(());
                    let mut stdout = io::stdout();
                    execute!(stdout, LeaveAlternateScreen).unwrap_or(());
                }
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
        alternative_queue: st.alternative_queue.iter().map(|(u, _)| u.clone()).collect(),
        in_progress: st.in_progress.clone(),
        completed: st.completed.clone(),
    };
    let content = serde_json::to_string_pretty(&saved)?;
    fs::write(path, content).await?;
    println!("State saved to {}", path);
    Ok(())
}
