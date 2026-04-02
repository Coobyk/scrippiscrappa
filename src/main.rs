use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use scraper::{Html, Selector};
use tokio::fs;
use tokio::sync::{mpsc, Mutex, Semaphore};

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

#[derive(Clone, Copy)]
enum UrlType {
    Html,
    Css,
    Script,
    Image,
    Audio,
    Other,
}

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

fn classify_url(content_type: &str, url: &str) -> UrlType {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("javascript") || ct.contains("typescript") || ct.contains("ecmascript") || ct.contains("wasm") {
        return UrlType::Script;
    }
    if ct.contains("text/html") { return UrlType::Html; }
    if ct.contains("text/css") || ct.contains("css") { return UrlType::Css; }
    if ct.contains("image/") { return UrlType::Image; }
    if ct.contains("audio/") { return UrlType::Audio; }

    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".js") || path.ends_with(".mjs") || path.ends_with(".ts") || path.ends_with(".wasm") {
        return UrlType::Script;
    }
    if path.ends_with(".html") || path.ends_with(".htm") { return UrlType::Html; }
    if path.ends_with(".css") { return UrlType::Css; }
    if path.ends_with(".png") || path.ends_with(".jpg") || path.ends_with(".jpeg")
        || path.ends_with(".gif") || path.ends_with(".svg") || path.ends_with(".webp")
        || path.ends_with(".ico") || path.ends_with(".avif") {
        return UrlType::Image;
    }
    if path.ends_with(".mp3") || path.ends_with(".wav") || path.ends_with(".ogg")
        || path.ends_with(".flac") || path.ends_with(".aac") || path.ends_with(".m4a") {
        return UrlType::Audio;
    }
    UrlType::Other
}

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
#[command(name = "scrippiscrappa", version, about = "A fast, concurrent website scraper.")]
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
}

struct CrawlState {
    visited: Mutex<HashSet<String>>,
    stats: Stats,
    active_tasks: AtomicUsize,
    queued: AtomicUsize,
    speed_log: StdMutex<VecDeque<(Instant, u64)>>,
}

struct Stats {
    downloaded: AtomicU64,
    failed: AtomicU64,
    bytes: AtomicU64,
    skipped: AtomicU64,
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

fn fmt_size(bytes: usize) -> String {
    let b = bytes as f64;
    if b < 1024.0 { format!("{} B", bytes) }
    else if b < 1048576.0 { format!("{:.1} KB", b / 1024.0) }
    else if b < 1073741824.0 { format!("{:.1} MB", b / 1048576.0) }
    else { format!("{:.2} GB", b / 1073741824.0) }
}

fn fmt_speed(bps: f64) -> String {
    if bps < 1024.0 { format!("{:.0} B/s", bps) }
    else if bps < 1048576.0 { format!("{:.0} KB/s", bps / 1024.0) }
    else { format!("{:.1} MB/s", bps / 1048576.0) }
}

fn normalize_url(input: &str) -> Result<String> {
    let s = if input.starts_with("http://") || input.starts_with("https://") {
        input.to_string()
    } else {
        format!("https://{}", input)
    };
    Ok(url::Url::parse(&s).context("Invalid URL")?.to_string())
}

fn resolve_url(base: &url::Url, href: &str) -> Option<url::Url> {
    let h = href.trim();
    if h.is_empty() || h.starts_with('#') || h.starts_with("javascript:")
        || h.starts_with("data:") || h.starts_with("mailto:")
        || h.starts_with("tel:") || h.starts_with("about:") {
        return None;
    }
    base.join(h).ok()
}

fn url_to_filepath(url: &url::Url) -> PathBuf {
    let host = url.host_str().unwrap_or("unknown");
    let mut parts: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() { return PathBuf::from(host).join("index.html"); }
    if !parts.last().copied().unwrap_or("").contains('.') { parts.push("index.html"); }
    let mut p = PathBuf::from(host);
    for part in parts { p.push(part); }
    p
}

fn extract_links(html: &str, base: &url::Url, domain: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut urls = Vec::new();
    let sels: &[(&str, &str)] = &[
        ("a","href"),("link","href"),("script","src"),("img","src"),
        ("img","srcset"),("source","src"),("source","srcset"),
        ("video","src"),("video","poster"),("audio","src"),
        ("embed","src"),("object","data"),("iframe","src"),
        ("frame","src"),("meta[http-equiv='refresh']","content"),
    ];
    for (tag, attr) in sels {
        let sel_str = if tag.contains('[') { tag.to_string() } else { format!("{}[{}]", tag, attr) };
        let sel = match Selector::parse(&sel_str) { Ok(s) => s, Err(_) => continue };
        for el in doc.select(&sel) {
            if let Some(val) = el.value().attr(attr) {
                let matches = if *attr == "srcset" {
                    val.split(',').filter_map(|p| {
                        p.trim().split_whitespace().next()
                    }).filter_map(|u| resolve_url(base, u))
                     .filter(|r| r.host_str() == Some(domain))
                     .map(|r| r.to_string())
                     .collect::<Vec<_>>()
                } else if *attr == "content" && *tag == "meta[http-equiv='refresh']" {
                    val.split(';').find(|s| s.trim().starts_with("url="))
                      .and_then(|u| {
                          let href = u.trim().strip_prefix("url=").unwrap_or("");
                          resolve_url(base, href).filter(|r| r.host_str() == Some(domain))
                      })
                      .map(|r| r.to_string())
                      .into_iter().collect()
                } else {
                    resolve_url(base, val)
                        .filter(|r| r.host_str() == Some(domain))
                        .map(|r| r.to_string())
                        .into_iter().collect()
                };
                urls.extend(matches);
            }
        }
    }
    urls
}

fn extract_css_urls(css: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rem = css;
    while let Some(s) = rem.find("url(") {
        rem = &rem[s + 4..];
        let e = rem.find(')').unwrap_or(rem.len());
        let inner = rem[..e].trim().trim_matches('\'').trim_matches('"').trim();
        if !inner.is_empty() { urls.push(inner.to_string()); }
        rem = &rem[e..];
    }
    urls
}

async fn download_url(client: &reqwest::Client, url: &str) -> Result<(Vec<u8>, String)> {
    let resp = client.get(url).header("Accept", "*/*").send().await
        .with_context(|| format!("GET {}", url))?;
    let ct = resp.headers().get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = resp.bytes().await.with_context(|| format!("body of {}", url))?;
    Ok((body.to_vec(), ct))
}

async fn save_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(p) = path.parent() { fs::create_dir_all(p).await?; }
    fs::write(path, data).await?;
    Ok(())
}

fn print_result_line(r: &DownloadResult) {
    let c = r.url_type.color();
    if r.success {
        eprintln!("  {}↓{} {}{}{}  {}{:.2}s  {}  {}{}",
            GREEN, RESET, c, r.url, RESET,
            DIM, r.elapsed, fmt_speed(r.speed), fmt_size(r.size), RESET);
    } else {
        let err = r.error.as_deref().unwrap_or("unknown");
        eprintln!("  {}✗{} {}{}{}  {}{}{}",
            RED, RESET, c, r.url, RESET, RED, err, RESET);
    }
}

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
        while log.front().map_or(false, |(t, _)| now.duration_since(*t).as_secs_f64() > 10.0) {
            log.pop_front();
        }
        if log.len() >= 2 {
            let total: u64 = log.iter().map(|(_, b)| b).sum();
            let span = now.duration_since(log.front().unwrap().0).as_secs_f64();
            if span > 0.0 { total as f64 / span } else { 0.0 }
        } else { 0.0 }
    };

    eprint!("\r\x1b[K  ");
    eprint!("{}dl: {}{}{} | ", GREEN, BOLD, dl, RESET);
    eprint!("{}q: {}{}{} | ", CYAN, BOLD, q, RESET);
    if a > 0 { eprint!("{}a: {}{}{} | ", YELLOW, BOLD, a, RESET); }
    else { eprint!("a: 0 | "); }
    if f > 0 { eprint!("{}f: {}{}{} | ", RED, BOLD, f, RESET); }
    else { eprint!("f: 0 | "); }
    if sk > 0 { eprint!("{}s: {}{}{} | ", DIM, BOLD, sk, RESET); }
    eprint!("{:.1} MB | ", mb);
    eprint!("{}{:.0} f/s{} | ", DIM, sp, RESET);
    eprint!("{} | ", fmt_speed(dl_speed));
    eprint!("{:.0}s", e);
}

fn is_sane_url(url: &str, no_limits: bool) -> bool {
    if no_limits { return true; }
    if url.len() > 2048 { return false; }
    if let Ok(parsed) = url::Url::parse(url) {
        let segments: Vec<&str> = parsed.path_segments()
            .map(|s| s.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        if segments.len() > 32 { return false; }
        for i in 1..segments.len() {
            if segments[i] == segments[i - 1] { return false; }
        }
    }
    true
}

fn enqueue(url: String, depth: usize, max: usize, state: &CrawlState, tx: &mpsc::UnboundedSender<(String, usize)>, no_limits: bool) {
    if !is_sane_url(&url, no_limits) {
        state.stats.skipped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if max == 0 || depth <= max {
        state.queued.fetch_add(1, Ordering::Relaxed);
        let _ = tx.send((url, depth));
    }
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
    eprintln!("  {}depth:{}  {}", DIM, RESET, if args.depth == 0 { "unlimited".into() } else { args.depth.to_string() });
    eprintln!("  {}out:{}    {}", DIM, RESET, args.output.display());
    eprintln!();

    let out_dir = args.output.canonicalize().unwrap_or(args.output.clone());
    let client = reqwest::Client::builder()
        .user_agent("scrippiscrappa/0.1.0")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .gzip(true).brotli(true).deflate(true)
        .build()?;

    let state = Arc::new(CrawlState {
        visited: Mutex::new(HashSet::new()),
        stats: Stats::new(),
        active_tasks: AtomicUsize::new(0),
        queued: AtomicUsize::new(0),
        speed_log: StdMutex::new(VecDeque::new()),
    });

    let sem = Arc::new(Semaphore::new(args.connections));
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, usize)>();
    let (rtx, mut rrx) = mpsc::unbounded_channel::<DownloadResult>();

    { state.visited.lock().await.insert(start_url.clone()); }
    state.queued.fetch_add(1, Ordering::Relaxed);
    tx.send((start_url, 0))?;

    let t0 = Instant::now();

    loop {
        match rx.try_recv() {
            Ok((url, depth)) => {
                let permit = sem.clone().acquire_owned().await?;
                state.active_tasks.fetch_add(1, Ordering::Relaxed);
                let (c, o, d, s, tx, rtx, md, nl) = (
                    client.clone(), out_dir.clone(), domain.clone(),
                    state.clone(), tx.clone(), rtx.clone(), args.depth, args.no_limits,
                );
                tokio::spawn(async move {
                    let _permit = permit;
                    let t1 = Instant::now();
                    match download_url(&c, &url).await {
                        Ok((body, ct)) => {
                            let el = t1.elapsed().as_secs_f64();
                            let sz = body.len();
                            let spd = if el > 0.0 { sz as f64 / el } else { 0.0 };
                            let parsed = url::Url::parse(&url).ok();
                            let same = parsed.as_ref().and_then(|u| u.host_str()).map(|h| h == d).unwrap_or(false);
                            let fp = parsed.as_ref().map(url_to_filepath).unwrap_or_else(|| PathBuf::from("unknown"));
                            if let Err(e) = save_file(&o.join(&fp), &body).await {
                                let _ = rtx.send(DownloadResult { url: url.clone(), size: sz, speed: spd, elapsed: el, success: false, error: Some(format!("{}", e)), url_type: classify_url(&ct, &url) });
                                s.stats.failed.fetch_add(1, Ordering::Relaxed);
                            } else {
                                let ut = classify_url(&ct, &url);
                                {
                                    let mut log = s.speed_log.lock().unwrap();
                                    log.push_back((Instant::now(), sz as u64));
                                }
                                let _ = rtx.send(DownloadResult { url: url.clone(), size: sz, speed: spd, elapsed: el, success: true, error: None, url_type: ut });
                                s.stats.downloaded.fetch_add(1, Ordering::Relaxed);
                                s.stats.bytes.fetch_add(sz as u64, Ordering::Relaxed);
                            }
                            if same && ct.contains("text/html") {
                                if let (Ok(h), Some(p)) = (std::str::from_utf8(&body), parsed.as_ref()) {
                                    for link in extract_links(h, p, &d) {
                                        if !is_sane_url(&link, nl) { continue; }
                                        let mut v = s.visited.lock().await;
                                        if v.insert(link.clone()) { drop(v); enqueue(link, depth+1, md, &s, &tx, nl); }
                                    }
                                }
                            }
                            if same && ct.contains("text/css") {
                                if let (Ok(css), Some(p)) = (std::str::from_utf8(&body), parsed.as_ref()) {
                                    for href in extract_css_urls(css) {
                                        if let Some(r) = resolve_url(p, &href) {
                                            if r.host_str() == Some(d.as_str()) {
                                                let rs = r.to_string();
                                                if !is_sane_url(&rs, nl) { continue; }
                                                let mut v = s.visited.lock().await;
                                                if v.insert(rs.clone()) { drop(v); enqueue(rs, depth+1, md, &s, &tx, nl); }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let el = t1.elapsed().as_secs_f64();
                            let _ = rtx.send(DownloadResult { url: url.clone(), size: 0, speed: 0.0, elapsed: el, success: false, error: Some(format!("{:#}", e)), url_type: classify_url("", &url) });
                            s.stats.failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    s.queued.fetch_sub(1, Ordering::Relaxed);
                    s.active_tasks.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(_) => {
                while let Ok(r) = rrx.try_recv() { print_result_line(&r); }
                print_progress(&state, t0);
                if state.queued.load(Ordering::Relaxed) == 0
                    && state.active_tasks.load(Ordering::Relaxed) == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    while let Ok(r) = rrx.try_recv() { print_result_line(&r); }
    print_progress(&state, t0);
    eprintln!();

    let el = t0.elapsed();
    let dl = state.stats.downloaded.load(Ordering::Relaxed);
    let fl = state.stats.failed.load(Ordering::Relaxed);
    let bt = state.stats.bytes.load(Ordering::Relaxed);

    eprintln!();
    eprintln!("{}done{} in {}{:.1}s{}", BOLD, RESET, DIM, el.as_secs_f64(), RESET);
    eprintln!("  {}{}{} downloaded", GREEN, dl, RESET);
    if fl > 0 { eprintln!("  {}{}{} failed", RED, fl, RESET); }
    let sk = state.stats.skipped.load(Ordering::Relaxed);
    if sk > 0 { eprintln!("  {}{}{} skipped (loop/bad url)", DIM, sk, RESET); }
    eprintln!("  {}{:.2} MB{} total", BOLD, bt as f64 / 1048576.0, RESET);

    Ok(())
}
