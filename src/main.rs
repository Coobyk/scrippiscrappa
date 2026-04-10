use std::collections::HashSet;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use scraper::{Html, Selector};
use tokio::fs;
use tokio::sync::{mpsc, Mutex, Semaphore};

// ANSI escape codes for colored terminal output
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const PURPLE: &str = "\x1b[35m";
const ORANGE: &str = "\x1b[38;5;208m";
const BLUE: &str = "\x1b[34m";
const WHITE: &str = "\x1b[37m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

// URL content type classification for colored output
#[derive(Clone, Copy)]
enum UrlType {
    Html,
    Css,
    Script,
    Image,
    Audio,
    Other,
}

// Apply colors to different types of URLs
impl UrlType {
    fn color(self) -> &'static str {
        match self {
            UrlType::Html => WHITE,
            UrlType::Css => PURPLE,
            UrlType::Script => YELLOW,
            UrlType::Image => ORANGE,
            UrlType::Audio => BLUE,
            UrlType::Other => CYAN,
        }
    }
}

// Classify URL based on content-type header and URL path extension
fn classify_url(content_type: &str, url: &str) -> UrlType {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("javascript")
        || ct.contains("typescript")
        || ct.contains("ecmascript")
        || ct.contains("wasm")
    {
        return UrlType::Script;
    }
    if ct.contains("text/html") {
        return UrlType::Html;
    }
    if ct.contains("text/css") || ct.contains("css") {
        return UrlType::Css;
    }
    if ct.contains("image/") {
        return UrlType::Image;
    }
    if ct.contains("audio/") {
        return UrlType::Audio;
    }

    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".js")
        || path.ends_with(".mjs")
        || path.ends_with(".ts")
        || path.ends_with(".wasm")
        || path.ends_with(".jsx")
        || path.ends_with(".tsx")
    {
        return UrlType::Script;
    }
    if path.ends_with(".html") || path.ends_with(".htm") {
        return UrlType::Html;
    }
    if path.ends_with(".css") {
        return UrlType::Css;
    }
    if path.ends_with(".png")
        || path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".gif")
        || path.ends_with(".svg")
        || path.ends_with(".webp")
        || path.ends_with(".ico")
        || path.ends_with(".avif")
        || path.ends_with(".heic")
    {
        return UrlType::Image;
    }
    if path.ends_with(".mp3")
        || path.ends_with(".wav")
        || path.ends_with(".ogg")
        || path.ends_with(".flac")
        || path.ends_with(".aac")
        || path.ends_with(".m4a")
        || path.ends_with(".opus")
    {
        return UrlType::Audio;
    }
    UrlType::Other
}

// Result of a URL download attempt (used for statistics display)
#[allow(dead_code)]
struct DownloadResult {
    url: String,
    size: usize,
    speed: f64,
    elapsed: f64,
    success: bool,
    error: Option<String>,
    url_type: UrlType,
}

#[derive(Parser, Debug)]
#[command(
    name = "scrippiscrappa",
    version,
    about = "A fast, concurrent website scraper."
)]
struct Args {
    url: String,
    #[arg(short = 'c', default_value_t = 8)]
    connections: usize,
    #[arg(short = 'd', default_value_t = 0)]
    depth: usize,
    #[arg(short = 'o', default_value = ".")]
    output: PathBuf,
    #[arg(long = "no-limits")]
    no_limits: bool,
    #[arg(short = 'z', long = "7z", help = "Create a .7z archive (requires 7z)")]
    archive_7z: bool,
    #[arg(long = "zip", help = "Create a .zip archive")]
    archive_zip: bool,
}

// Shared crawl state across all async tasks
struct CrawlState {
    visited: Mutex<HashSet<String>>, // Track visited URLs to avoid duplicates
    stats: Stats,                    // Download statistics counters
    active_tasks: AtomicUsize,       // Number of active download tasks
    queued: AtomicUsize,             // Number of URLs waiting in queue
    speed_log: StdMutex<VecDeque<(Instant, u64)>>, // Rolling window for speed calculation
}

// Statistics counters (atomic for thread-safe updates)
struct Stats {
    downloaded: AtomicU64, // Successful downloads count
    failed: AtomicU64,     // Failed downloads count
    bytes: AtomicU64,      // Total bytes downloaded
    skipped: AtomicU64,    // Skipped URLs (sanity checks failed)
}

impl Stats {
    fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        }
    }
}

// Format byte count as human-readable string (B, KB, MB, GB)
fn fmt_size(bytes: usize) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{} B", bytes)
    } else if b < 1048576.0 {
        format!("{:.1} KB", b / 1024.0)
    } else if b < 1073741824.0 {
        format!("{:.1} MB", b / 1048576.0)
    } else {
        format!("{:.2} GB", b / 1073741824.0)
    }
}

// Format download speed as human-readable string (B/s, KB/s, MB/s)
fn fmt_speed(bps: f64) -> String {
    if bps < 1024.0 {
        format!("{:.0} B/s", bps)
    } else if bps < 1048576.0 {
        format!("{:.0} KB/s", bps / 1024.0)
    } else {
        format!("{:.1} MB/s", bps / 1048576.0)
    }
}

// Normalize input URL: add https:// prefix if missing, then validate and return absolute URL
fn normalize_url(input: &str) -> Result<String> {
    let s = if input.starts_with("http://") || input.starts_with("https://") {
        input.to_string()
    } else {
        format!("https://{}", input)
    };
    Ok(url::Url::parse(&s).context("Invalid URL")?.to_string())
}

// Resolve a relative href against a base URL, filtering out invalid/special URLs
// Returns None for empty links, fragment links, javascript:, data:, mailto:, tel:, about:
fn resolve_url(base: &url::Url, href: &str) -> Option<url::Url> {
    let h = href.trim();
    if h.is_empty()
        || h.starts_with('#')
        || h.starts_with("javascript:")
        || h.starts_with("data:")
        || h.starts_with("mailto:")
        || h.starts_with("tel:")
        || h.starts_with("about:")
    {
        return None;
    }
    base.join(h).ok()
}

// Convert URL to filesystem path for saving
// Example: https://example.com/path/page -> example.com/path/page.html
fn url_to_filepath(url: &url::Url) -> PathBuf {
    let host = url.host_str().unwrap_or("unknown");
    let mut parts: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return PathBuf::from(host).join("index.html");
    }
    if !parts.last().copied().unwrap_or("").contains('.') {
        parts.push("index.html");
    }
    let mut p = PathBuf::from(host);
    for part in parts {
        p.push(part);
    }
    p
}

// Extract all valid URLs from HTML document that belong to the same domain
// Scans: <a href>, <link href>, <script src>, <img src>, <img srcset>, <source src>,
//        <source srcset>, <video src/poster>, <audio src>, <embed src>, <object data>,
//        <iframe src>, <frame src>, <meta http-equiv='refresh'>
fn extract_links(html: &str, base: &url::Url, domain: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut urls = Vec::new();

    // Fields to scrape
    let sels: &[(&str, &str)] = &[
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
    for (tag, attr) in sels {
        let sel_str = if tag.contains('[') {
            tag.to_string()
        } else {
            format!("{}[{}]", tag, attr)
        };
        let sel = match Selector::parse(&sel_str) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for el in doc.select(&sel) {
            if let Some(val) = el.value().attr(attr) {
                let matches = if *attr == "srcset" {
                    val.split(',')
                        .filter_map(|p| p.split_whitespace().next())
                        .filter_map(|u| resolve_url(base, u))
                        .filter(|r| r.host_str() == Some(domain))
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                } else if *attr == "content" && *tag == "meta[http-equiv='refresh']" {
                    val.split(';')
                        .find(|s| s.trim().starts_with("url="))
                        .and_then(|u| {
                            let href = u.trim().strip_prefix("url=").unwrap_or("");
                            resolve_url(base, href).filter(|r| r.host_str() == Some(domain))
                        })
                        .map(|r| r.to_string())
                        .into_iter()
                        .collect()
                } else {
                    resolve_url(base, val)
                        .filter(|r| r.host_str() == Some(domain))
                        .map(|r| r.to_string())
                        .into_iter()
                        .collect()
                };
                urls.extend(matches);
            }
        }
    }
    urls
}

// Extract URLs from CSS @import and url() declarations
fn extract_css_urls(css: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rem = css;
    while let Some(s) = rem.find("url(") {
        rem = &rem[s + 4..];
        let e = rem.find(')').unwrap_or(rem.len());
        let inner = rem[..e].trim().trim_matches('\'').trim_matches('"').trim();
        if !inner.is_empty() {
            urls.push(inner.to_string());
        }
        rem = &rem[e..];
    }
    urls
}

// Download a URL and return the response body and content-type header
async fn download_url(client: &reqwest::Client, url: &str) -> Result<(Vec<u8>, String)> {
    let resp = client
        .get(url)
        .header("Accept", "*/*")
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp
        .bytes()
        .await
        .with_context(|| format!("body of {}", url))?;
    Ok((body.to_vec(), ct))
}

// Asynchronously save file data to disk, creating parent directories as needed
async fn save_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).await?;
    }
    fs::write(path, data).await?;
    Ok(())
}

// Print progress bar showing download statistics (written to stderr)
fn print_progress(state: &CrawlState, start: Instant) {
    let dl = state.stats.downloaded.load(Ordering::Relaxed);
    let q = state.queued.load(Ordering::Relaxed);
    let a = state.active_tasks.load(Ordering::Relaxed);
    let f = state.stats.failed.load(Ordering::Relaxed);
    let sk = state.stats.skipped.load(Ordering::Relaxed);
    let b = state.stats.bytes.load(Ordering::Relaxed);
    let e = start.elapsed().as_secs_f64();
    let sp = if e > 0.0 { dl as f64 / e } else { 0.0 };
    let mb = b as f64 / 1048576.0;

    let dl_speed = {
        let mut log = state.speed_log.lock().unwrap();
        let now = Instant::now();
        while log
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t).as_secs_f64() > 10.0)
        {
            log.pop_front();
        }
        if log.len() >= 2 {
            let total: u64 = log.iter().map(|(_, b)| b).sum();
            let span = now.duration_since(log.front().unwrap().0).as_secs_f64();
            if span > 0.0 {
                total as f64 / span
            } else {
                0.0
            }
        } else {
            0.0
        }
    };

    eprint!("\r\x1b[K  ");
    eprint!("{}dl: {}{}{} | ", GREEN, BOLD, dl, RESET);
    eprint!("{}q: {}{}{} | ", CYAN, BOLD, q, RESET);
    if a > 0 {
        eprint!("{}a: {}{}{} | ", YELLOW, BOLD, a, RESET);
    } else {
        eprint!("a: 0 | ");
    }
    if f > 0 {
        eprint!("{}f: {}{}{} | ", RED, BOLD, f, RESET);
    } else {
        eprint!("f: 0 | ");
    }
    if sk > 0 {
        eprint!("{}s: {}{}{} | ", DIM, BOLD, sk, RESET);
    }
    eprint!("{:.1} MB | ", mb);
    eprint!("{}{:.0} f/s{} | ", DIM, sp, RESET);
    eprint!("{} | ", fmt_speed(dl_speed));
    eprint!("{:.0}s", e);
}

// Print a completed URL with its result status and update progress bar
fn print_url_with_stats(r: &DownloadResult, state: &CrawlState, start: Instant) {
    let c = r.url_type.color();
    print!("\x1b[A\r\x1b[K");
    if r.success {
        println!("  {}↓{} {}{}{}", GREEN, RESET, c, r.url, RESET);
    } else {
        let err = r.error.as_deref().unwrap_or("unknown");
        eprintln!(
            "  {}✗{} {}{}{}  {}{}{}",
            RED, RESET, c, r.url, RESET, RED, err, RESET
        );
    }
    print_progress(state, start);
    println!("\r")
}

// Check if URL passes sanity checks: length limit, path segment depth, no repeating segments
// When no_limits is true, all URLs are considered valid
fn is_sane_url(url: &str, no_limits: bool) -> bool {
    if no_limits {
        return true;
    }
    if url.len() > 2048 {
        return false;
    }
    if let Ok(parsed) = url::Url::parse(url) {
        let segments: Vec<&str> = parsed
            .path_segments()
            .map(|s| s.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        if segments.len() > 32 {
            return false;
        }
        for i in 1..segments.len() {
            if segments[i] == segments[i - 1] {
                return false;
            }
        }
    }
    true
}

// Add URL to download queue if it passes sanity checks and depth limit
// Updates visited set, queued counter, and sends to channel
fn enqueue(
    url: String,
    depth: usize,
    max: usize,
    state: &CrawlState,
    tx: &mpsc::UnboundedSender<(String, usize)>,
    no_limits: bool,
) {
    if !is_sane_url(&url, no_limits) {
        state.stats.skipped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if max == 0 || depth <= max {
        state.queued.fetch_add(1, Ordering::Relaxed);
        let _ = tx.send((url, depth));
    }
}

// Check if path represents a domain directory (has dots in name, not hidden)
#[allow(dead_code)]
fn is_domain_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.contains('.') && !n.starts_with('.'))
            .unwrap_or(false)
}

// Collect all domain directories from output directory for archiving
#[allow(dead_code)]
fn collect_domain_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if is_domain_dir(&entry.path()) {
                result.push(entry.path());
            }
        }
    }
    result.sort();
    result
}

// Create a ZIP archive from domain directories
fn create_zip(dirs: &[PathBuf], base: &Path, out: &Path) -> Result<()> {
    let file = std::fs::File::create(out)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut buf = Vec::new();
    let mut stack = Vec::new();
    for dir in dirs {
        stack.clear();
        stack.push(dir.clone());
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d)? {
                let entry = entry?;
                let path = entry.path();
                let rel = path.strip_prefix(base)?;
                if path.is_dir() {
                    stack.push(path);
                } else {
                    buf.clear();
                    let mut f = std::fs::File::open(&path)?;
                    std::io::Read::read_to_end(&mut f, &mut buf)?;
                    zip.start_file(rel.to_string_lossy(), opts)?;
                    zip.write_all(&buf)?;
                }
            }
        }
    }
    zip.finish()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let start_url = normalize_url(&args.url)?;
    let parsed = url::Url::parse(&start_url)?;
    let domain = parsed.host_str().context("no host")?.to_string();

    eprintln!("{}{}scrippiscrappa{}", BOLD, CYAN, RESET);
    eprintln!("  {}url:{}    {}", DIM, RESET, start_url);
    eprintln!("  {}host:{}   {}", DIM, RESET, domain);
    eprintln!("  {}conn:{}   {}", DIM, RESET, args.connections);
    eprintln!(
        "  {}depth:{}  {}",
        DIM,
        RESET,
        if args.depth == 0 {
            "unlimited".into()
        } else {
            args.depth.to_string()
        }
    );
    eprintln!("  {}out:{}    {}", DIM, RESET, args.output.display());
    eprintln!();

    let out_dir = args.output.canonicalize().unwrap_or(args.output.clone());
    // Initialize HTTP client with connection pooling, compression, and timeouts
    let client = reqwest::Client::builder()
        .user_agent("scrippiscrappa/0.1.0")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()?;

    // Initialize shared crawl state (visited URLs, statistics, concurrency control)
    let state = Arc::new(CrawlState {
        visited: Mutex::new(HashSet::new()),
        stats: Stats::new(),
        active_tasks: AtomicUsize::new(0),
        queued: AtomicUsize::new(0),
        speed_log: StdMutex::new(VecDeque::new()),
    });
    // Track which domains were successfully downloaded (for archiving)
    let successful_domains: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Concurrency limiter (semaphore) and channels for URL queue and results
    let sem = Arc::new(Semaphore::new(args.connections));
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, usize)>(); // URL + depth
    let (rtx, mut rrx) = mpsc::unbounded_channel::<DownloadResult>();

    // Add starting URL to queue
    {
        state.visited.lock().await.insert(start_url.clone());
    }
    state.queued.fetch_add(1, Ordering::Relaxed);
    tx.send((start_url, 0))?;

    let t0 = Instant::now();
    let mut last_progress_print = Instant::now();

    // Main event loop: process URL downloads concurrently with tokio::select!
    // - rrx.recv(): receive completed download results
    // - rx.recv(): receive new URLs to download from queue
    // - sleep: periodic progress updates and check for completion
    loop {
        tokio::select! {
            Some(r) = rrx.recv() => {
                print_url_with_stats(&r, &state, t0);
            }

            url = rx.recv() => {
                match url {
                    Some((url, depth)) => {
                        // Acquire semaphore permit to limit concurrent downloads
                        let permit = sem.clone().acquire_owned().await?;
                        state.active_tasks.fetch_add(1, Ordering::Relaxed);

                        // Clone all required data for the spawned task
                        let (c, o, d, s, tx, rtx, md, nl, successful_domains) = (
                            client.clone(),
                            out_dir.clone(),
                            domain.clone(),
                            state.clone(),
                            tx.clone(),
                            rtx.clone(),
                            args.depth,
                            args.no_limits,
                            successful_domains.clone(),
                        );

                        // Spawn async worker to download URL and process results
                        tokio::spawn(async move {
                            let _permit = permit;  // Release permit when task completes
                            let t1 = Instant::now();

                            // Download the URL
                            match download_url(&c, &url).await {
                                Ok((body, ct)) => {
                                    let el = t1.elapsed().as_secs_f64();
                                    let sz = body.len();
                                    let spd = if el > 0.0 { sz as f64 / el } else { 0.0 };
                                    let parsed = url::Url::parse(&url).ok();
                                    let same = parsed
                                        .as_ref()
                                        .and_then(|u| u.host_str())
                                        .map(|h| h == d)
                                        .unwrap_or(false);
                                    let fp = parsed
                                        .as_ref()
                                        .map(url_to_filepath)
                                        .unwrap_or_else(|| PathBuf::from("unknown"));

                                    // Save file to disk
                                    if let Err(e) = save_file(&o.join(&fp), &body).await {
                                        let _ = rtx.send(DownloadResult {
                                            url: url.clone(),
                                            size: sz,
                                            speed: spd,
                                            elapsed: el,
                                            success: false,
                                            error: Some(format!("{}", e)),
                                            url_type: classify_url(&ct, &url),
                                        });
                                        s.stats.failed.fetch_add(1, Ordering::Relaxed);
                                    // File saved successfully - update statistics
                                    } else {
                                        let ut = classify_url(&ct, &url);
                                        // Track successful domain for archiving
                                        if let Some(host) = parsed.as_ref().and_then(|u| u.host_str()) {
                                            successful_domains.lock().await.insert(host.to_string());
                                        }
                                        // Log download for speed calculation
                                        {
                                            let mut log = s.speed_log.lock().unwrap();
                                            log.push_back((Instant::now(), sz as u64));
                                        }
                                        // Send success result to display channel
                                        let _ = rtx.send(DownloadResult {
                                            url: url.clone(),
                                            size: sz,
                                            speed: spd,
                                            elapsed: el,
                                            success: true,
                                            error: None,
                                            url_type: ut,
                                        });
                                        s.stats.downloaded.fetch_add(1, Ordering::Relaxed);
                                        s.stats.bytes.fetch_add(sz as u64, Ordering::Relaxed);
                                    }

                                    // Parse HTML to extract and enqueue discovered links
                                    if same && ct.contains("text/html") {
                                        if let (Ok(h), Some(p)) =
                                            (std::str::from_utf8(&body), parsed.as_ref())
                                        {
                                            for link in extract_links(h, p, &d) {
                                                if !is_sane_url(&link, nl) {
                                                    continue;
                                                }
                                                let mut v = s.visited.lock().await;
                                                if v.insert(link.clone()) {
                                                    drop(v);
                                                    enqueue(link, depth + 1, md, &s, &tx, nl);
                                                }
                                            }
                                        }
                                    }

                                    // Parse CSS to extract @import and url() references
                                    if same && ct.contains("text/css") {
                                        if let (Ok(css), Some(p)) =
                                            (std::str::from_utf8(&body), parsed.as_ref())
                                        {
                                            for href in extract_css_urls(css) {
                                                if let Some(r) = resolve_url(p, &href) {
                                                    if r.host_str() == Some(d.as_str()) {
                                                        let rs = r.to_string();
                                                        if !is_sane_url(&rs, nl) {
                                                            continue;
                                                        }
                                                        let mut v = s.visited.lock().await;
                                                        if v.insert(rs.clone()) {
                                                            drop(v);
                                                            enqueue(rs, depth + 1, md, &s, &tx, nl);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    let el = t1.elapsed().as_secs_f64();
                                    let _ = rtx.send(DownloadResult {
                                        url: url.clone(),
                                        size: 0,
                                        speed: 0.0,
                                        elapsed: el,
                                        success: false,
                                        error: Some(format!("{:#}", e)),
                                        url_type: classify_url("", &url),
                                    });
                                    s.stats.failed.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            s.queued.fetch_sub(1, Ordering::Relaxed);
                            s.active_tasks.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    None => {
                        while let Ok(r) = rrx.try_recv() {
                            print_url_with_stats(&r, &state, t0);
                        }
                        if state.queued.load(Ordering::Relaxed) == 0
                            && state.active_tasks.load(Ordering::Relaxed) == 0
                        {
                            break;
                        }
                    }
                }
            }

            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                while let Ok(r) = rrx.try_recv() {
                    print_url_with_stats(&r, &state, t0);
                }
                if last_progress_print.elapsed().as_millis() >= 500 {
                    print_progress(&state, t0);
                    last_progress_print = Instant::now();
                }
                if state.queued.load(Ordering::Relaxed) == 0
                    && state.active_tasks.load(Ordering::Relaxed) == 0
                {
                    break;
                }
            }
        }
    }

    while let Ok(r) = rrx.try_recv() {
        print_url_with_stats(&r, &state, t0);
    }
    print_progress(&state, t0);
    eprintln!();

    let el = t0.elapsed();
    let dl = state.stats.downloaded.load(Ordering::Relaxed);
    let fl = state.stats.failed.load(Ordering::Relaxed);
    let bt = state.stats.bytes.load(Ordering::Relaxed);

    eprintln!();
    eprintln!(
        "{}done{} in {}{:.1}s{}",
        BOLD,
        RESET,
        DIM,
        el.as_secs_f64(),
        RESET
    );
    eprintln!("  {}{}{} downloaded", GREEN, dl, RESET);
    if fl > 0 {
        eprintln!("  {}{}{} failed", RED, fl, RESET);
    }
    let sk = state.stats.skipped.load(Ordering::Relaxed);
    if sk > 0 {
        eprintln!("  {}{}{} skipped (loop/bad url)", DIM, sk, RESET);
    }
    eprintln!("  {}{:.2} MB{} total", BOLD, bt as f64 / 1048576.0, RESET);

    if args.archive_7z || args.archive_zip {
        eprintln!();
        let mut domains: Vec<PathBuf> = {
            let successful_domains = successful_domains.lock().await;
            successful_domains.iter().map(|d| out_dir.join(d)).collect()
        };
        domains.sort();
        if domains.is_empty() {
            eprintln!(
                "  {}no successful domain directories found to archive{}",
                RED, RESET
            );
        } else {
            if args.archive_7z {
                let archive_name = format!("{}.7z", domain);
                let archive_path = out_dir.join(&archive_name);
                eprint!(
                    "  {}7z{} → {}{}{} ... ",
                    BOLD, RESET, DIM, archive_name, RESET
                );
                let mut cmd = std::process::Command::new("7z");
                cmd.args(["a", "-y"]).arg(&archive_path);
                for d in &domains {
                    cmd.arg(d);
                }
                cmd.stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                match cmd.status() {
                    Ok(s) if s.success() => {
                        let size = std::fs::metadata(&archive_path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        eprintln!("{}done{} ({})", GREEN, RESET, fmt_size(size as usize));
                    }
                    Ok(s) => eprintln!("{}failed{} (exit code {})", RED, RESET, s),
                    Err(e) => eprintln!("{}failed{} ({})", RED, RESET, e),
                }
            }

            if args.archive_zip {
                let archive_name = format!("{}.zip", domain);
                let archive_path = out_dir.join(&archive_name);
                eprint!(
                    "  {}zip{} → {}{}{} ... ",
                    BOLD, RESET, DIM, archive_name, RESET
                );
                match create_zip(&domains, &out_dir, &archive_path) {
                    Ok(()) => {
                        let size = std::fs::metadata(&archive_path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        eprintln!("{}done{} ({})", GREEN, RESET, fmt_size(size as usize));
                    }
                    Err(e) => eprintln!("{}failed{} ({})", RED, RESET, e),
                }
            }
        }
    }

    Ok(())
}
