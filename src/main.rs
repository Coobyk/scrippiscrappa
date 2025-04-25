use std::{collections::HashSet, io, path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};

use clap::Parser;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    event::{self, Event, KeyEvent, KeyCode, KeyModifiers},
};
use reqwest::Client;
use sanitize_filename::sanitize;
use scraper::{Html, Selector};
use tokio::{
    fs,
    sync::{Mutex, Semaphore},
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

#[derive(Parser, Debug)]
#[clap(name = "scrippiscrappa", version)]
struct Args {
    #[clap(help = "Start URL to scrape")]
    url: String,
    #[clap(short, long, help = "Output directory name")]
    output: Option<String>,
    #[clap(short = 's', long, help = "Save remaining queue to file")]
    save_queue: Option<String>,
    #[clap(short, long, default_value_t = 8, help = "Parallel download count")]
    concurrency: usize,
}

struct AppState {
    queue: Vec<String>,
    in_progress: Vec<String>,
    completed: Vec<String>,
}

impl AppState {
    fn new() -> Self {
        Self {
            queue: Vec::new(),
            in_progress: Vec::new(),
            completed: Vec::new(),
        }
    }
    fn enqueue(&mut self, url: String) {
        self.queue.push(url);
    }
    fn dequeue(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }
    fn start(&mut self, url: String) {
        self.in_progress.push(url);
    }
    fn finish(&mut self, url: &String) {
        self.in_progress.retain(|u| u != url);
        self.completed.push(url.clone());
    }
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
            path.push(name);
            path.set_extension("html");
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let parsed = Url::parse(url)?;
    let resp = client.get(url).send().await?;
    let headers = resp.headers().clone();
    let content = resp.bytes().await?;
    let local_path = get_local_path(&parsed, base);
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&local_path, &content).await?;
    if let Some(ct) = headers.get(reqwest::header::CONTENT_TYPE) {
        if let Ok(ct_str) = ct.to_str() {
            if ct_str.contains("text/html") {
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
                                                    if link_host == start_host {
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
                                                if link_host == start_host {
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
                                            if link_host == start_host {
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
                let mut vis = visited.lock().await;
                let mut st = state.lock().await;
                for link in to_enqueue {
                    if vis.insert(link.clone()) {
                        st.enqueue(link);
                    }
                }
            }
        }
    }
    Ok(())
}

fn draw_ui<B: tui::backend::Backend>(f: &mut tui::Frame<B>, st: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)].as_ref())
        .split(f.size());
    let queue_items: Vec<ListItem> = st.queue.iter().map(|u| ListItem::new(u.clone())).collect();
    let inprog_items: Vec<ListItem> = st
        .in_progress
        .iter()
        .map(|u| ListItem::new(u.clone()))
        .collect();
    let queue_list = List::new(queue_items).block(Block::default().borders(Borders::ALL).title(
        Spans::from(Span::raw(format!("Queue ({})", st.queue.len()))),
    ));
    let inprog_list = List::new(inprog_items).block(Block::default().borders(Borders::ALL).title(
        Spans::from(Span::raw(format!("In Progress ({})", st.in_progress.len()))),
    ));
    f.render_widget(queue_list, chunks[0]);
    f.render_widget(inprog_list, chunks[1]);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let start_url = args.url.clone();
    let output_folder = args.output.clone().unwrap_or_else(|| {
        Url::parse(&start_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or("output".to_string())
    });
    fs::create_dir_all(&output_folder).await?;
    let state = Arc::new(Mutex::new(AppState::new()));
    let visited = Arc::new(Mutex::new(HashSet::new()));
    let start_host = Url::parse(&start_url)?.host_str().unwrap().to_string();
    {
        let mut vis = visited.lock().await;
        vis.insert(start_url.clone());
    }
    {
        let mut st = state.lock().await;
        st.enqueue(start_url.clone());
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
                if event::poll(Duration::from_millis(200)).unwrap() {
                    if let Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. }) = event::read().unwrap() {
                        shutdown.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }
    // setup TUI
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
                if st.queue.is_empty() && st.in_progress.is_empty() {
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
    let client = Client::builder().user_agent("scrippiscrappa").build()?;
    let semaphore = Arc::new(Semaphore::new(args.concurrency));
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let next = {
            let mut st = state.lock().await;
            st.dequeue()
        };
        if let Some(url) = next {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let state_clone = state.clone();
            let visited_clone = visited.clone();
            let client_clone = client.clone();
            let base = output_folder.clone();
            let start_host = start_host.clone();
            let shutdown_task = shutdown.clone();
            tokio::spawn(async move {
                {
                    let mut st = state_clone.lock().await;
                    st.start(url.clone());
                }
                if !shutdown_task.load(Ordering::SeqCst) {
                    let _ = process_url(
                        &client_clone,
                        &url,
                        &base,
                        state_clone.clone(),
                        visited_clone.clone(),
                        &start_host,
                    ).await;
                }
                {
                    let mut st = state_clone.lock().await;
                    st.finish(&url);
                }
                drop(permit);
            });
        } else {
            time::sleep(Duration::from_millis(100)).await;
            let st = state.lock().await;
            if st.queue.is_empty() && st.in_progress.is_empty() {
                break;
            }
        }
    }
    if let Some(path) = args.save_queue {
        let st = state.lock().await;
        let content = st.queue.join("\n");
        fs::write(path, content).await?;
    }
    // ensure terminal is restored if still in raw mode
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
