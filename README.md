# ScrippiScrappa

ScrippiScrappa is a simple site scraper written in Rust. It scrapes any site and downloads every file. Simple as that!

## Usage

```
scrippiscrappa [OPTIONS] <URL>
```

### Options

| Flag | Long Flag | Description |
|---|---|---|
| | `--batch` | Comma-separated list of URLs to scrape in batch mode |
| `-o` | `--output` | Output directory name |
| `-s` | `--save-queue` | Save remaining queue to file |
| `-c` | `--concurrency` | Parallel download count (default: 8) |
| `-r` | `--resume` | Resume from saved state file |
| | `--ci` | Force CI output |
| `-f` | `--force-fragments` | Force scraping of URLs with fragments (parts after #) |
| `-q` | `--force-queries` | Force scraping of URLs with query parameters (parts after ?) |
| `-i` | `--ignore` | Ignore URLs containing these patterns (can be specified multiple times) |
| `-d` | `--subdomains` | Allowed subdomains (comma-separated, e.g. 'blog,docs'). If not specified, only the main domain is allowed. |
| `-a` | `--alternative-domains` | Allowed alternative domains (comma-separated). |
| | `--repetition-threshold` | Repetition threshold for path segments (default: 4) |