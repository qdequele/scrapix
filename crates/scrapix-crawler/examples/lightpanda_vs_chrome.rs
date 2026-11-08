//! Benchmark: crawl renders on Lightpanda vs Chrome, on a list of URLs.
//!
//! ```bash
//! lightpanda serve --host 127.0.0.1 --port 9222 --cdp-max-connections 32 &
//! SCRAPIX_BENCH_LIGHTPANDA=ws://127.0.0.1:9222 \
//! SCRAPIX_BENCH_CHROME=/path/to/chrome \
//!   cargo run --release --example lightpanda_vs_chrome -p scrapix-crawler \
//!   --features browser-lightpanda -- crates/scrapix-crawler/examples/lightpanda_vs_chrome_urls.txt 8
//! ```
//!
//! `urls.txt` holds one URL per line (`#` comments allowed); the second
//! argument is the number of pages rendered at once (default 8). Each URL is
//! rendered once per engine, Lightpanda first (Chrome first with
//! `SCRAPIX_BENCH_CHROME_FIRST=1`, to check warm-cache bias). Reports wall time, per-page
//! latency, failures, Lightpanda's Chrome-fallback verdicts, peak memory of
//! each engine's processes (sampled with `ps`), and the URLs where
//! Lightpanda's visible text is far below Chrome's.

#[cfg(not(feature = "browser-lightpanda"))]
fn main() {
    eprintln!("This example requires the 'browser-lightpanda' feature.");
}

#[cfg(feature = "browser-lightpanda")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bench::run().await
}

#[cfg(feature = "browser-lightpanda")]
mod bench {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use futures::{stream, StreamExt};
    use scrapix_crawler::{
        visible_text_chars, CdpRendererBuilder, LightpandaConfig, LightpandaRenderer,
    };

    const TIMEOUT: Duration = Duration::from_secs(30);

    struct Outcome {
        url: String,
        elapsed: Duration,
        /// `Ok(text_chars)` or the error.
        result: Result<usize, String>,
        /// Lightpanda only: why Chrome would take over.
        fallback: Option<&'static str>,
    }

    struct Report {
        engine: &'static str,
        wall: Duration,
        peak_rss_mb: u64,
        outcomes: Vec<Outcome>,
    }

    pub async fn run() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let list = args.get(1).ok_or_else(|| {
            anyhow::anyhow!("usage: lightpanda_vs_chrome <urls.txt> [concurrency]")
        })?;
        let concurrency: usize = args.get(2).map(|c| c.parse()).transpose()?.unwrap_or(8);
        let urls: Vec<String> = std::fs::read_to_string(list)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect();
        println!("{} URLs, {} at once\n", urls.len(), concurrency);

        let mut reports = Vec::new();
        let chrome_first = std::env::var("SCRAPIX_BENCH_CHROME_FIRST").is_ok_and(|v| v == "1");
        if chrome_first {
            reports.extend(run_chrome(&urls, concurrency).await?);
            reports.extend(run_lightpanda(&urls, concurrency).await?);
            reports.reverse();
        } else {
            reports.extend(run_lightpanda(&urls, concurrency).await?);
            reports.extend(run_chrome(&urls, concurrency).await?);
        }

        print_summary(&reports);
        if let [lp, chrome] = reports.as_slice() {
            print_parity(lp, chrome);
        }
        Ok(())
    }

    async fn run_lightpanda(urls: &[String], concurrency: usize) -> anyhow::Result<Option<Report>> {
        if let Ok(cdp_url) = std::env::var("SCRAPIX_BENCH_LIGHTPANDA") {
            let renderer = Arc::new(
                LightpandaRenderer::new(
                    LightpandaConfig {
                        cdp_url,
                        max_connections: concurrency,
                        timeout: TIMEOUT,
                        ..LightpandaConfig::default()
                    },
                    None,
                )
                .await?,
            );
            let sampler =
                RssSampler::start(|cmd| cmd.contains("lightpanda") && cmd.contains("serve"));
            let start = Instant::now();
            let outcomes = stream::iter(urls.to_vec())
                .map(|url| {
                    let renderer = renderer.clone();
                    async move {
                        let t = Instant::now();
                        let r = renderer.render(&url, None, &[], Some(false)).await;
                        let elapsed = t.elapsed();
                        match r {
                            Ok(render) => Outcome {
                                fallback: render.fallback_reason(),
                                result: Ok(render.text_chars),
                                url,
                                elapsed,
                            },
                            Err(e) => Outcome {
                                result: Err(e.to_string()),
                                fallback: Some("render failed"),
                                url,
                                elapsed,
                            },
                        }
                    }
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await;
            Ok(Some(Report {
                engine: "lightpanda",
                wall: start.elapsed(),
                peak_rss_mb: sampler.stop(),
                outcomes,
            }))
        } else {
            println!("SCRAPIX_BENCH_LIGHTPANDA not set: skipping Lightpanda");
            Ok(None)
        }
    }

    async fn run_chrome(urls: &[String], concurrency: usize) -> anyhow::Result<Option<Report>> {
        if let Ok(chrome) = std::env::var("SCRAPIX_BENCH_CHROME") {
            let mut builder = CdpRendererBuilder::new()
                .headless(true)
                .timeout(TIMEOUT)
                .max_concurrent_pages(concurrency);
            if chrome != "1" {
                builder = builder.executable_path(chrome);
            }
            let renderer = Arc::new(builder.build().await?);
            // The renderer's profile directory carries this process id.
            let marker = format!("scrapix-chrome-{}-", std::process::id());
            let sampler = RssSampler::start(move |cmd| cmd.contains(&marker));
            let start = Instant::now();
            let outcomes = stream::iter(urls.to_vec())
                .map(|url| {
                    let renderer = renderer.clone();
                    async move {
                        let t = Instant::now();
                        let options = scrapix_crawler::PageOptions {
                            respect_robots: Some(false),
                            ..Default::default()
                        };
                        let r = renderer.render_page(&url, &options).await;
                        Outcome {
                            result: r
                                .map(|r| visible_text_chars(&r.html))
                                .map_err(|e| e.to_string()),
                            fallback: None,
                            elapsed: t.elapsed(),
                            url,
                        }
                    }
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await;
            let report = Report {
                engine: "chrome",
                wall: start.elapsed(),
                peak_rss_mb: sampler.stop(),
                outcomes,
            };
            if let Ok(renderer) = Arc::try_unwrap(renderer) {
                renderer.close().await;
            }
            Ok(Some(report))
        } else {
            println!("SCRAPIX_BENCH_CHROME not set: skipping Chrome");
            Ok(None)
        }
    }

    fn percentile(sorted: &[Duration], p: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx]
    }

    fn print_summary(reports: &[Report]) {
        println!(
            "\n{:<11} {:>7} {:>7} {:>7} {:>8} {:>8} {:>9} {:>9}",
            "engine", "wall s", "ok", "failed", "p50 ms", "p95 ms", "fallback", "peak MB"
        );
        for r in reports {
            let mut times: Vec<Duration> = r
                .outcomes
                .iter()
                .filter(|o| o.result.is_ok())
                .map(|o| o.elapsed)
                .collect();
            times.sort();
            let ok = times.len();
            let failed = r.outcomes.len() - ok;
            let fallback = r.outcomes.iter().filter(|o| o.fallback.is_some()).count();
            println!(
                "{:<11} {:>7.1} {:>7} {:>7} {:>8} {:>8} {:>9} {:>9}",
                r.engine,
                r.wall.as_secs_f64(),
                ok,
                failed,
                percentile(&times, 0.5).as_millis(),
                percentile(&times, 0.95).as_millis(),
                if r.engine == "lightpanda" {
                    fallback.to_string()
                } else {
                    "-".to_string()
                },
                r.peak_rss_mb
            );
        }
        for r in reports {
            let mut reasons: std::collections::BTreeMap<&str, usize> = Default::default();
            for o in &r.outcomes {
                if let Some(reason) = o.fallback {
                    *reasons.entry(reason).or_default() += 1;
                }
            }
            if !reasons.is_empty() {
                println!("\n{} fallback reasons: {:?}", r.engine, reasons);
                for o in r.outcomes.iter().filter(|o| o.result.is_ok()) {
                    if let Some(reason) = o.fallback {
                        println!("  {} -> chrome ({reason}): {}", r.engine, o.url);
                    }
                }
            }
            let errors: Vec<_> = r
                .outcomes
                .iter()
                .filter_map(|o| o.result.as_ref().err().map(|e| (&o.url, e)))
                .take(10)
                .collect();
            for (url, e) in errors {
                println!(
                    "  {} error {url}: {}",
                    r.engine,
                    e.chars().take(160).collect::<String>()
                );
            }
        }
    }

    /// URLs where Lightpanda kept the page (no fallback) but found less than
    /// half of Chrome's visible text: content the crawl would silently lose.
    fn print_parity(lp: &Report, chrome: &Report) {
        let chrome_text: std::collections::HashMap<&str, usize> = chrome
            .outcomes
            .iter()
            .filter_map(|o| o.result.as_ref().ok().map(|t| (o.url.as_str(), *t)))
            .collect();
        let mut lost = Vec::new();
        let mut ratios = Vec::new();
        let mut compared = 0;
        for o in &lp.outcomes {
            let (Ok(lp_text), None) = (&o.result, o.fallback) else {
                continue;
            };
            let Some(&c) = chrome_text.get(o.url.as_str()) else {
                continue;
            };
            compared += 1;
            if c > 0 {
                ratios.push(*lp_text as f64 / c as f64);
            }
            if c > 0 && (*lp_text as f64) < 0.5 * c as f64 {
                lost.push((o.url.as_str(), *lp_text, c));
            }
        }
        println!(
            "\nContent parity: {} of {} pages kept by Lightpanda have < 50% of Chrome's text",
            lost.len(),
            compared
        );
        if !ratios.is_empty() {
            ratios.sort_by(f64::total_cmp);
            println!(
                "Lightpanda/Chrome visible-text ratio on kept pages: min {:.2}, median {:.2}",
                ratios[0],
                ratios[ratios.len() / 2]
            );
        }
        for (url, l, c) in lost.iter().take(20) {
            println!("  {url}: lightpanda {l} chars, chrome {c} chars");
        }
    }

    /// Samples the summed RSS of the processes whose command line matches,
    /// every 200 ms, and keeps the peak.
    struct RssSampler {
        peak_kb: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    }

    impl RssSampler {
        fn start(matches: impl Fn(&str) -> bool + Send + 'static) -> Self {
            let peak_kb = Arc::new(AtomicU64::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (p, s) = (peak_kb.clone(), stop.clone());
            let handle = std::thread::spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    if let Ok(out) = std::process::Command::new("ps")
                        .args(["-axo", "rss=,command="])
                        .output()
                    {
                        let total: u64 = String::from_utf8_lossy(&out.stdout)
                            .lines()
                            .filter_map(|line| {
                                let line = line.trim_start();
                                let (rss, cmd) = line.split_once(' ')?;
                                matches(cmd).then(|| rss.parse::<u64>().ok()).flatten()
                            })
                            .sum();
                        p.fetch_max(total, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
            Self {
                peak_kb,
                stop,
                handle,
            }
        }

        /// Stop sampling and return the peak, in MB.
        fn stop(self) -> u64 {
            self.stop.store(true, Ordering::Relaxed);
            let _ = self.handle.join();
            self.peak_kb.load(Ordering::Relaxed) / 1024
        }
    }
}
