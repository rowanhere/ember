use num_bigint::BigUint;
use num_traits::Zero;
use rand::Rng;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tiny_keccak::{Hasher, Keccak};

const DEFAULT_NODE: &str = "https://emberchain.org";
const DEFAULT_BATCH_SIZE: u64 = 25_000;
const STATUS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Header {
    number: u64,
    #[serde(rename = "parentHash")]
    parent_hash: String,
    timestamp: u64,
    miner: String,
    difficulty: String,
    #[serde(rename = "transactionsRoot")]
    transactions_root: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Template {
    header: Header,
    target: String,
    #[serde(rename = "pendingTxHashes")]
    pending_tx_hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SubmitBlock<'a> {
    #[serde(rename = "minerAddress")]
    miner_address: &'a str,
    header: &'a Header,
    nonce: String,
    #[serde(rename = "blockHash")]
    block_hash: String,
    #[serde(rename = "pendingTxHashes")]
    pending_tx_hashes: &'a [String],
}

#[derive(Clone, Debug)]
struct Config {
    miner: String,
    node: String,
    threads: usize,
    batch_size: u64,
    no_submit: bool,
}

#[derive(Debug)]
struct Found {
    nonce: u64,
    block_hash: String,
}

#[derive(Debug)]
enum SubmitError {
    Stale(String),
    Failed(String),
}

struct MineView {
    miner_started: Instant,
    block_started: Instant,
    block_hashes_before: u64,
    accepted: u64,
    stale: u64,
}

fn main() {
    let config = parse_args();
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("ember-cpu-miner/0.1")
        .build()
        .expect("failed to create HTTP client");

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .expect("failed to install Ctrl+C handler");
    }

    println!("[CONFIG] node: {}", config.node);
    println!("[CONFIG] miner: {}", config.miner);
    println!("[CONFIG] threads: {}", config.threads);
    println!("[CONFIG] batch size/thread: {}", config.batch_size);
    print_banner(&config);

    let started = Instant::now();
    let total_hashes = Arc::new(AtomicU64::new(0));
    let mut accepted = 0u64;
    let mut stale = 0u64;

    while !stop.load(Ordering::Relaxed) {
        let template = match fetch_template(&client, &config) {
            Ok(template) => template,
            Err(err) => {
                eprintln!("[WARN] template fetch failed: {err}");
                sleep_or_stop(&stop, Duration::from_secs(3));
                continue;
            }
        };

        let target = match parse_target(&template.target) {
            Ok(target) => target,
            Err(err) => {
                eprintln!("[WARN] bad target from node: {err}");
                sleep_or_stop(&stop, Duration::from_secs(3));
                continue;
            }
        };

        print_block_header(&template);

        let block_hashes_before = total_hashes.load(Ordering::Relaxed);
        let block_started = Instant::now();
        let found = mine_template(
            &config,
            &template,
            target,
            &total_hashes,
            &stop,
            &MineView {
                miner_started: started,
                block_started,
                block_hashes_before,
                accepted,
                stale,
            },
        );
        let block_hashes = total_hashes
            .load(Ordering::Relaxed)
            .saturating_sub(block_hashes_before);
        let block_rate = block_hashes as f64 / block_started.elapsed().as_secs_f64().max(0.001);
        let avg_rate = total_hashes.load(Ordering::Relaxed) as f64
            / started.elapsed().as_secs_f64().max(0.001);

        match found {
            Some(found) if !stop.load(Ordering::Relaxed) => {
                println!(
                    "[FOUND] block #{} nonce={} hash={} rate={} avg={}",
                    template.header.number,
                    found.nonce,
                    found.block_hash,
                    format_rate(block_rate),
                    format_rate(avg_rate)
                );

                if config.no_submit {
                    println!("[SUBMIT] skipped because --no-submit is set");
                    break;
                } else {
                    match submit_block(&client, &config, &template, &found) {
                        Ok(body) => {
                            accepted += 1;
                            println!("[SUBMIT] accepted={} response={}", accepted, body);
                        }
                        Err(SubmitError::Stale(body)) => {
                            stale += 1;
                            println!("[STALE] node already advanced before submit: {body}");
                        }
                        Err(SubmitError::Failed(err)) => eprintln!("[WARN] submit failed: {err}"),
                    }
                }
            }
            _ => break,
        }
    }

    println!(
        "[STOP] checked={} accepted={} stale={} avg={}",
        format_hashes(total_hashes.load(Ordering::Relaxed) as f64),
        accepted,
        stale,
        format_rate(
            total_hashes.load(Ordering::Relaxed) as f64
                / started.elapsed().as_secs_f64().max(0.001)
        )
    );
}

fn parse_args() -> Config {
    let mut args = env::args().skip(1);
    let mut miner = None;
    let mut node = DEFAULT_NODE.to_string();
    let mut threads = num_cpus::get().max(1);
    let mut batch_size = DEFAULT_BATCH_SIZE;
    let mut no_submit = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => usage(),
            "-j" | "--threads" => {
                let value = args.next().unwrap_or_else(|| usage());
                threads = value.parse::<usize>().unwrap_or_else(|_| usage()).max(1);
            }
            "--node" => {
                node = args
                    .next()
                    .unwrap_or_else(|| usage())
                    .trim_end_matches('/')
                    .to_string();
            }
            "--batch-size" => {
                let value = args.next().unwrap_or_else(|| usage());
                batch_size = value.parse::<u64>().unwrap_or_else(|_| usage()).max(1);
            }
            "--no-submit" => no_submit = true,
            value if value.starts_with('-') => usage(),
            value => {
                if miner.is_some() {
                    usage();
                }
                miner = Some(value.to_string());
            }
        }
    }

    let miner = miner.unwrap_or_else(|| usage());
    if !miner.starts_with("0x") || miner.len() != 42 {
        eprintln!("miner address should look like a 20-byte 0x... address");
        usage();
    }

    Config {
        miner,
        node,
        threads,
        batch_size,
        no_submit,
    }
}

fn usage() -> ! {
    eprintln!(
        "Usage: ember-cpu-miner [--node https://emberchain.org] [-j THREADS] [--batch-size N] [--no-submit] MINER_ADDRESS"
    );
    std::process::exit(2);
}

fn fetch_template(client: &Client, config: &Config) -> Result<Template, String> {
    let url = format!(
        "{}/api/mining/template?minerAddress={}",
        config.node, config.miner
    );
    client
        .get(url)
        .send()
        .map_err(|err| err.to_string())?
        .error_for_status()
        .map_err(|err| err.to_string())?
        .json::<Template>()
        .map_err(|err| err.to_string())
}

fn parse_target(target: &str) -> Result<BigUint, String> {
    BigUint::parse_bytes(target.as_bytes(), 10)
        .filter(|value| !value.is_zero())
        .ok_or_else(|| format!("invalid decimal target: {target}"))
}

fn mine_template(
    config: &Config,
    template: &Template,
    target: BigUint,
    total_hashes: &Arc<AtomicU64>,
    stop: &Arc<AtomicBool>,
    view: &MineView,
) -> Option<Found> {
    let found_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::with_capacity(config.threads);

    for worker_id in 0..config.threads {
        let header = template.header.clone();
        let target = target.clone();
        let tx = tx.clone();
        let total_hashes = Arc::clone(total_hashes);
        let stop = Arc::clone(stop);
        let found_flag = Arc::clone(&found_flag);
        let threads = config.threads as u64;
        let batch_size = config.batch_size;
        let miner_started = view.miner_started;
        let block_started = view.block_started;
        let block_hashes_before = view.block_hashes_before;
        let accepted = view.accepted;
        let stale = view.stale;
        let block_number = template.header.number;
        let difficulty = template.header.difficulty.clone();

        handles.push(thread::spawn(move || {
            let mut rng = rand::thread_rng();
            let mut nonce = rng.gen::<u64>().wrapping_add(worker_id as u64);
            let mut last_log = Instant::now();
            let mut local_hashes = 0u64;

            while !stop.load(Ordering::Relaxed) && !found_flag.load(Ordering::Relaxed) {
                for _ in 0..batch_size {
                    let (hash_hex, hash_value) = block_hash(&header, nonce);
                    local_hashes = local_hashes.wrapping_add(1);

                    if hash_value <= target {
                        if !found_flag.swap(true, Ordering::Relaxed) {
                            let _ = tx.send(Found {
                                nonce,
                                block_hash: hash_hex,
                            });
                        }
                        total_hashes.fetch_add(local_hashes, Ordering::Relaxed);
                        return;
                    }

                    nonce = nonce.wrapping_add(threads);
                }

                total_hashes.fetch_add(local_hashes, Ordering::Relaxed);
                local_hashes = 0;

                if worker_id == 0 && last_log.elapsed() >= STATUS_INTERVAL {
                    last_log = Instant::now();
                    let checked = total_hashes.load(Ordering::Relaxed);
                    let block_checked = checked.saturating_sub(block_hashes_before);
                    let current_rate =
                        block_checked as f64 / block_started.elapsed().as_secs_f64().max(0.001);
                    let average_rate =
                        checked as f64 / miner_started.elapsed().as_secs_f64().max(0.001);
                    println!(
                        "[STATS] block #{:<7} diff {:<9} speed {:>11} avg {:>11} total {:>10} acc {:<3} stale {:<3} uptime {} nonce~{}",
                        block_number,
                        difficulty,
                        format_rate(current_rate),
                        format_rate(average_rate),
                        format_hashes(checked as f64),
                        accepted,
                        stale,
                        format_duration(miner_started.elapsed()),
                        nonce,
                    );
                }
            }

            if local_hashes > 0 {
                total_hashes.fetch_add(local_hashes, Ordering::Relaxed);
            }
        }));
    }

    drop(tx);
    let found = rx.recv().ok();
    found_flag.store(true, Ordering::Relaxed);

    for handle in handles {
        let _ = handle.join();
    }

    found
}

fn block_hash(header: &Header, nonce: u64) -> (String, BigUint) {
    let bytes = browser_header_json(header, nonce);
    let mut output = [0u8; 32];
    let mut keccak = Keccak::v256();
    keccak.update(bytes.as_bytes());
    keccak.finalize(&mut output);

    (
        format!("0x{}", hex::encode(output)),
        BigUint::from_bytes_be(&output),
    )
}

fn browser_header_json(header: &Header, nonce: u64) -> String {
    // Matches the worker's JSON.stringify field order exactly.
    format!(
        "{{\"number\":{},\"parentHash\":\"{}\",\"timestamp\":{},\"miner\":\"{}\",\"difficulty\":\"{}\",\"transactionsRoot\":\"{}\",\"nonce\":\"{}\"}}",
        header.number,
        header.parent_hash,
        header.timestamp,
        header.miner,
        header.difficulty,
        header.transactions_root,
        nonce
    )
}

fn submit_block(
    client: &Client,
    config: &Config,
    template: &Template,
    found: &Found,
) -> Result<String, SubmitError> {
    let payload = SubmitBlock {
        miner_address: &config.miner,
        header: &template.header,
        nonce: found.nonce.to_string(),
        block_hash: found.block_hash.clone(),
        pending_tx_hashes: &template.pending_tx_hashes,
    };
    let url = format!("{}/api/mining/submit", config.node);

    let response = client
        .post(url)
        .json(&payload)
        .send()
        .map_err(|err| SubmitError::Failed(err.to_string()))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|err| SubmitError::Failed(err.to_string()))?;

    if status.is_success() {
        return Ok(body);
    }

    if status.as_u16() == 409 {
        return Err(SubmitError::Stale(body));
    }

    Err(SubmitError::Failed(format!("HTTP {status}: {body}")))
}

fn sleep_or_stop(stop: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
}

fn print_banner(config: &Config) {
    println!("+------------------------------------------------------------+");
    println!("| Ember CPU Miner                                            |");
    println!("| Algo: Keccak-256 JSON PoW                                  |");
    println!("| Threads: {:<49}|", config.threads);
    println!("| Batch/thread: {:<44}|", config.batch_size);
    println!("| Miner: {:<50}|", abbreviate(&config.miner, 12, 8));
    println!("+------------------------------------------------------------+");
}

fn print_block_header(template: &Template) {
    println!(
        "[MINE] block #{} | diff {} | pending {} | parent {} | target {}",
        template.header.number,
        template.header.difficulty,
        template.pending_tx_hashes.len(),
        abbreviate(&template.header.parent_hash, 12, 8),
        abbreviate(&template.target, 18, 8)
    );
}

fn format_rate(rate: f64) -> String {
    format!("{}H/s", format_scaled(rate))
}

fn format_hashes(hashes: f64) -> String {
    format!("{}H", format_scaled(hashes))
}

fn format_scaled(value: f64) -> String {
    const UNITS: [&str; 6] = ["", "K", "M", "G", "T", "P"];
    let mut scaled = value.max(0.0);
    let mut unit = 0usize;

    while scaled >= 1000.0 && unit < UNITS.len() - 1 {
        scaled /= 1000.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{:>6.0} {}", scaled, UNITS[unit])
    } else if scaled >= 100.0 {
        format!("{:>6.1} {}", scaled, UNITS[unit])
    } else {
        format!("{:>6.2} {}", scaled, UNITS[unit])
    }
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn abbreviate(value: &str, head: usize, tail: usize) -> String {
    if value.len() <= head + tail + 3 {
        return value.to_string();
    }

    format!("{}...{}", &value[..head], &value[value.len() - tail..])
}
