use rand::Rng;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tiny_keccak::{Hasher, Keccak};

const DEFAULT_NODE: &str = "https://emberchain.org";
const DEFAULT_BATCH_SIZE: u64 = 25_000;
const DEFAULT_CUDA_BATCH_SIZE: u64 = 67_108_864;
const STATUS_INTERVAL: Duration = Duration::from_secs(5);
static DASHBOARD_RENDER_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "cuda")]
extern "C" {
    fn ember_cuda_mine(
        prefix: *const u8,
        prefix_len: usize,
        target: *const u8,
        start_nonce: u64,
        total_nonces: u64,
        found_nonce: *mut u64,
        found_hash: *mut u8,
        checked: *mut u64,
        device_id: i32,
    ) -> i32;
}

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
    cuda: bool,
    cuda_devices: Vec<i32>,
    cuda_batch_size: u64,
    no_submit: bool,
}

#[derive(Debug)]
struct Found {
    nonce: u64,
    block_hash: String,
}

#[derive(Clone, Debug)]
struct FoundBlock {
    block: u64,
    nonce: u64,
    status: String,
}

#[derive(Clone)]
struct WorkPrefix {
    prefix: Vec<u8>,
    suffix: &'static [u8],
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
    blocks_found: u64,
    accepted: u64,
    stale: u64,
    recent_blocks: Vec<FoundBlock>,
}

struct Dashboard<'a> {
    miner: &'a str,
    node: &'a str,
    threads: usize,
    block: u64,
    difficulty: &'a str,
    current_rate: f64,
    average_rate: f64,
    checked: u64,
    uptime: Duration,
    current_nonce: u64,
    blocks_found: u64,
    accepted: u64,
    stale: u64,
    recent_blocks: &'a [FoundBlock],
}

fn main() {
    let config = parse_args();

    #[cfg(not(feature = "cuda"))]
    if config.cuda {
        eprintln!("[ERROR] CUDA requested, but this binary was built without --features cuda.");
        eprintln!("[ERROR] Build with: cargo build --release --features cuda");
        std::process::exit(2);
    }

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
    println!(
        "[CONFIG] mode: {}",
        if config.cuda { "CUDA" } else { "CPU" }
    );
    println!("[CONFIG] threads: {}", config.threads);
    println!("[CONFIG] batch size/thread: {}", config.batch_size);
    if config.cuda {
        println!(
            "[CONFIG] cuda devices: {}",
            format_cuda_devices(&config.cuda_devices)
        );
        println!("[CONFIG] cuda batch size: {}", config.cuda_batch_size);
    }
    print_banner(&config);

    let started = Instant::now();
    let total_hashes = Arc::new(AtomicU64::new(0));
    let mut accepted = 0u64;
    let mut stale = 0u64;
    let mut blocks_found = 0u64;
    let mut recent_blocks = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        let template = match fetch_template(&client, &config) {
            Ok(template) => template,
            Err(err) => {
                eprintln!("[WARN] template fetch failed: {err}");
                sleep_or_stop(&stop, Duration::from_secs(3));
                continue;
            }
        };

        let target = match parse_target_bytes(&template.target) {
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
                blocks_found,
                accepted,
                stale,
                recent_blocks: recent_blocks.clone(),
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
                    blocks_found += 1;
                    push_found_block(
                        &mut recent_blocks,
                        FoundBlock {
                            block: template.header.number,
                            nonce: found.nonce,
                            status: "dry-run".to_string(),
                        },
                    );
                    break;
                } else {
                    match submit_block(&client, &config, &template, &found) {
                        Ok(body) => {
                            blocks_found += 1;
                            accepted += 1;
                            push_found_block(
                                &mut recent_blocks,
                                FoundBlock {
                                    block: template.header.number,
                                    nonce: found.nonce,
                                    status: compact_status(&body),
                                },
                            );
                            println!("[SUBMIT] accepted={} response={}", accepted, body);
                        }
                        Err(SubmitError::Stale(body)) => {
                            blocks_found += 1;
                            stale += 1;
                            push_found_block(
                                &mut recent_blocks,
                                FoundBlock {
                                    block: template.header.number,
                                    nonce: found.nonce,
                                    status: format!("stale: {}", compact_status(&body)),
                                },
                            );
                            println!("[STALE] node already advanced before submit: {body}");
                        }
                        Err(SubmitError::Failed(err)) => eprintln!("[WARN] submit failed: {err}"),
                    }
                }
            }
            _ => break,
        }
    }

    print!("\x1b[?25h\n");
    println!(
        "[STOP] checked={} found={} accepted={} stale={} avg={}",
        format_hashes(total_hashes.load(Ordering::Relaxed) as f64),
        blocks_found,
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
    let mut cuda = false;
    let mut cuda_devices = vec![0i32];
    let mut cuda_batch_size = DEFAULT_CUDA_BATCH_SIZE;
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
            "--cuda" => cuda = true,
            "--cuda-device" => {
                let value = args.next().unwrap_or_else(|| usage());
                cuda_devices = vec![value.parse::<i32>().unwrap_or_else(|_| usage())];
            }
            "--cuda-devices" => {
                let value = args.next().unwrap_or_else(|| usage());
                cuda_devices = parse_cuda_devices(&value).unwrap_or_else(|| usage());
            }
            "--cuda-batch-size" => {
                let value = args.next().unwrap_or_else(|| usage());
                cuda_batch_size = value.parse::<u64>().unwrap_or_else(|_| usage()).max(1);
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
        cuda,
        cuda_devices,
        cuda_batch_size,
        no_submit,
    }
}

fn usage() -> ! {
    eprintln!(
        "Usage: ember-cpu-miner [--cuda] [--cuda-device ID|--cuda-devices 0,1] [--cuda-batch-size N] [--node https://emberchain.org] [-j THREADS] [--batch-size N] [--no-submit] MINER_ADDRESS"
    );
    std::process::exit(2);
}

fn parse_cuda_devices(value: &str) -> Option<Vec<i32>> {
    let devices = value
        .split(',')
        .map(|part| part.trim().parse::<i32>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    if devices.is_empty() {
        None
    } else {
        Some(devices)
    }
}

fn format_cuda_devices(devices: &[i32]) -> String {
    devices
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",")
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

fn parse_target_bytes(target: &str) -> Result<[u8; 32], String> {
    let mut value = [0u8; 32];
    for byte in target.bytes() {
        if !byte.is_ascii_digit() {
            return Err(format!("invalid decimal target: {target}"));
        }

        let digit = byte - b'0';
        let mut carry = digit as u16;
        for slot in value.iter_mut().rev() {
            let next = (*slot as u16) * 10 + carry;
            *slot = (next & 0xff) as u8;
            carry = next >> 8;
        }

        if carry != 0 {
            return Err(format!("target does not fit in 256 bits: {target}"));
        }
    }

    if value.iter().all(|&byte| byte == 0) {
        return Err(format!("invalid decimal target: {target}"));
    }

    Ok(value)
}

fn mine_template(
    config: &Config,
    template: &Template,
    target: [u8; 32],
    total_hashes: &Arc<AtomicU64>,
    stop: &Arc<AtomicBool>,
    view: &MineView,
) -> Option<Found> {
    if config.cuda {
        return mine_template_cuda(config, template, target, total_hashes, stop, view);
    }

    let found_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::with_capacity(config.threads);
    let work_prefix = WorkPrefix::new(&template.header);

    for worker_id in 0..config.threads {
        let work_prefix = work_prefix.clone();
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
        let blocks_found = view.blocks_found;
        let recent_blocks = view.recent_blocks.clone();
        let block_number = template.header.number;
        let difficulty = template.header.difficulty.clone();
        let miner = abbreviate(&config.miner, 12, 8);
        let node = config.node.clone();
        let worker_count = config.threads;

        handles.push(thread::spawn(move || {
            let mut rng = rand::thread_rng();
            let mut nonce = rng.gen::<u64>().wrapping_add(worker_id as u64);
            let mut last_log = Instant::now()
                .checked_sub(STATUS_INTERVAL)
                .unwrap_or_else(Instant::now);
            let mut local_hashes = 0u64;

            while !stop.load(Ordering::Relaxed) && !found_flag.load(Ordering::Relaxed) {
                for _ in 0..batch_size {
                    let hash = block_hash_bytes(&work_prefix, nonce);
                    local_hashes = local_hashes.wrapping_add(1);

                    if hash_meets_target(&hash, &target) {
                        if !found_flag.swap(true, Ordering::Relaxed) {
                            let _ = tx.send(Found {
                                nonce,
                                block_hash: format_hash_hex(&hash),
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
                    render_dashboard(&Dashboard {
                        miner: &miner,
                        node: &node,
                        threads: worker_count,
                        block: block_number,
                        difficulty: &difficulty,
                        current_rate,
                        average_rate,
                        checked,
                        uptime: miner_started.elapsed(),
                        current_nonce: nonce,
                        blocks_found,
                        accepted,
                        stale,
                        recent_blocks: &recent_blocks,
                    });
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

#[cfg(feature = "cuda")]
fn mine_template_cuda(
    config: &Config,
    template: &Template,
    target: [u8; 32],
    total_hashes: &Arc<AtomicU64>,
    stop: &Arc<AtomicBool>,
    view: &MineView,
) -> Option<Found> {
    let work_prefix = WorkPrefix::new(&template.header);
    for &device_id in &config.cuda_devices {
        if let Err(err) = cuda_self_test(device_id, &work_prefix) {
            eprintln!("[CUDA] self-test failed on device {device_id}: {err}");
            eprintln!(
                "[CUDA] refusing to mine on GPU until the CUDA hash matches CPU/browser hash"
            );
            return None;
        }
    }

    let found_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::with_capacity(config.cuda_devices.len());
    let mut rng = rand::thread_rng();
    let base_nonce = rng.gen::<u64>();
    let device_count = config.cuda_devices.len() as u64;

    for (device_index, &device_id) in config.cuda_devices.iter().enumerate() {
        let work_prefix = work_prefix.clone();
        let tx = tx.clone();
        let stop = Arc::clone(stop);
        let found_flag = Arc::clone(&found_flag);
        let total_hashes = Arc::clone(total_hashes);
        let target = target;
        let cuda_batch_size = config.cuda_batch_size;
        let miner = abbreviate(&config.miner, 12, 8);
        let node = config.node.clone();
        let miner_started = view.miner_started;
        let block_started = view.block_started;
        let block_hashes_before = view.block_hashes_before;
        let blocks_found = view.blocks_found;
        let accepted = view.accepted;
        let stale = view.stale;
        let recent_blocks = view.recent_blocks.clone();
        let block_number = template.header.number;
        let difficulty = template.header.difficulty.clone();
        let dashboard_devices = config.cuda_devices.len();
        let mut start_nonce =
            base_nonce.wrapping_add((device_index as u64).wrapping_mul(cuda_batch_size));

        handles.push(thread::spawn(move || {
            let mut last_dashboard = Instant::now()
                .checked_sub(STATUS_INTERVAL)
                .unwrap_or_else(Instant::now);

            while !stop.load(Ordering::Relaxed) && !found_flag.load(Ordering::Relaxed) {
                let mut found_nonce = 0u64;
                let mut found_hash = [0u8; 32];
                let mut checked = 0u64;
                let batch_started = Instant::now();

                let status = unsafe {
                    ember_cuda_mine(
                        work_prefix.prefix.as_ptr(),
                        work_prefix.prefix.len(),
                        target.as_ptr(),
                        start_nonce,
                        cuda_batch_size,
                        &mut found_nonce,
                        found_hash.as_mut_ptr(),
                        &mut checked,
                        device_id,
                    )
                };

                if status < 0 {
                    eprintln!("[CUDA] device {device_id} kernel failed with status {status}");
                    return;
                }

                total_hashes.fetch_add(checked, Ordering::Relaxed);

                if device_index == 0 && (last_dashboard.elapsed() >= STATUS_INTERVAL || status == 1)
                {
                    last_dashboard = Instant::now();
                    let all_checked = total_hashes.load(Ordering::Relaxed);
                    let block_checked = all_checked.saturating_sub(block_hashes_before);
                    let current_rate =
                        block_checked as f64 / block_started.elapsed().as_secs_f64().max(0.001);
                    let average_rate =
                        all_checked as f64 / miner_started.elapsed().as_secs_f64().max(0.001);
                    render_dashboard(&Dashboard {
                        miner: &miner,
                        node: &node,
                        threads: dashboard_devices,
                        block: block_number,
                        difficulty: &difficulty,
                        current_rate,
                        average_rate,
                        checked: all_checked,
                        uptime: miner_started.elapsed(),
                        current_nonce: start_nonce,
                        blocks_found,
                        accepted,
                        stale,
                        recent_blocks: &recent_blocks,
                    });
                }

                if status == 1 {
                    if !found_flag.swap(true, Ordering::Relaxed) {
                        let _ = tx.send(Found {
                            nonce: found_nonce,
                            block_hash: format_hash_hex(&found_hash),
                        });
                    }
                    return;
                }

                let elapsed = batch_started.elapsed().as_secs_f64().max(0.001);
                let rate = checked as f64 / elapsed;
                if rate < 1_000.0 {
                    eprintln!(
                        "[CUDA] device {device_id} very low GPU rate detected: {}",
                        format_rate(rate)
                    );
                }

                start_nonce = start_nonce.wrapping_add(cuda_batch_size.wrapping_mul(device_count));
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

#[cfg(feature = "cuda")]
fn cuda_self_test(device_id: i32, work_prefix: &WorkPrefix) -> Result<(), String> {
    let test_nonce = 123_456_789u64;
    let pass_target = [0xffu8; 32];
    let expected_hash = block_hash_bytes(work_prefix, test_nonce);
    let mut found_nonce = 0u64;
    let mut found_hash = [0u8; 32];
    let mut checked = 0u64;

    let status = unsafe {
        ember_cuda_mine(
            work_prefix.prefix.as_ptr(),
            work_prefix.prefix.len(),
            pass_target.as_ptr(),
            test_nonce,
            1,
            &mut found_nonce,
            found_hash.as_mut_ptr(),
            &mut checked,
            device_id,
        )
    };

    if status != 1 {
        return Err(format!("expected status 1, got {status}"));
    }
    if found_nonce != test_nonce {
        return Err(format!("expected nonce {test_nonce}, got {found_nonce}"));
    }
    if found_hash != expected_hash {
        return Err(format!(
            "hash mismatch gpu={} cpu={}",
            format_hash_hex(&found_hash),
            format_hash_hex(&expected_hash)
        ));
    }

    Ok(())
}

#[cfg(not(feature = "cuda"))]
fn mine_template_cuda(
    _config: &Config,
    _template: &Template,
    _target: [u8; 32],
    _total_hashes: &Arc<AtomicU64>,
    _stop: &Arc<AtomicBool>,
    _view: &MineView,
) -> Option<Found> {
    None
}

fn block_hash_bytes(work: &WorkPrefix, nonce: u64) -> [u8; 32] {
    let mut output = [0u8; 32];
    let mut keccak = Keccak::v256();
    keccak.update(&work.prefix);
    keccak.update(nonce.to_string().as_bytes());
    keccak.update(work.suffix);
    keccak.finalize(&mut output);
    output
}

fn hash_meets_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    hash <= target
}

fn format_hash_hex(hash: &[u8; 32]) -> String {
    format!("0x{}", hex::encode(hash))
}

impl WorkPrefix {
    fn new(header: &Header) -> Self {
        let prefix = format!(
            "{{\"number\":{},\"parentHash\":\"{}\",\"timestamp\":{},\"miner\":\"{}\",\"difficulty\":\"{}\",\"transactionsRoot\":\"{}\",\"nonce\":\"",
            header.number,
            header.parent_hash,
            header.timestamp,
            header.miner,
            header.difficulty,
            header.transactions_root
        )
        .into_bytes();

        Self {
            prefix,
            suffix: b"\"}",
        }
    }
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

fn render_dashboard(dashboard: &Dashboard) {
    if DASHBOARD_RENDER_COUNT.fetch_add(1, Ordering::Relaxed) == 0 {
        print!("\x1b[2J");
    }
    print!("\x1b[?25l\x1b[H");
    dashboard_line("EMBER CPU MINER - DASHBOARD");
    dashboard_line("======================================================================");
    dashboard_line(&format!(
        "{:<18} {:<18} {:<18} {:<18}",
        "Algo", "Threads", "Miner", "Node"
    ));
    dashboard_line(&format!(
        "{:<18} {:<18} {:<18} {:<18}",
        "Keccak-256",
        dashboard.threads,
        dashboard.miner,
        truncate_for_table(dashboard.node, 18)
    ));
    dashboard_line("----------------------------------------------------------------------");
    dashboard_line(&format!(
        "{:<14} {:<18} {:<18} {:<18}",
        "Block", "Difficulty", "HashRate", "Average"
    ));
    dashboard_line(&format!(
        "{:<14} {:<18} {:<18} {:<18}",
        dashboard.block,
        truncate_for_table(dashboard.difficulty, 18),
        format_rate(dashboard.current_rate),
        format_rate(dashboard.average_rate)
    ));
    dashboard_line("----------------------------------------------------------------------");
    dashboard_line(&format!(
        "{:<16} {:<12} {:<22} {:<12}",
        "Checked", "Uptime", "Nonce", "Block Found"
    ));
    dashboard_line(&format!(
        "{:<16} {:<12} {:<22} {:<12}",
        format_hashes(dashboard.checked as f64),
        format_duration(dashboard.uptime),
        dashboard.current_nonce,
        dashboard.blocks_found
    ));
    dashboard_line("----------------------------------------------------------------------");
    dashboard_line(&format!(
        "Accepted Blocks: {} | Stale Blocks: {}",
        dashboard.accepted, dashboard.stale
    ));
    dashboard_line(&format!("{:<10} {:<22} {:<28}", "Block", "Nonce", "Status"));
    if dashboard.recent_blocks.is_empty() {
        dashboard_line(&format!("{:<10} {:<22} {:<28}", "-", "-", "none yet"));
    } else {
        for block in dashboard.recent_blocks.iter().take(8) {
            dashboard_line(&format!(
                "{:<10} {:<22} {:<28}",
                block.block,
                block.nonce,
                truncate_for_table(&block.status, 28)
            ));
        }
    }
    dashboard_line("======================================================================");
    dashboard_line("Ctrl+C to stop. Dashboard refreshes silently every 5 seconds.");
    print!("\x1b[J");
    let _ = io::stdout().flush();
}

fn dashboard_line(line: &str) {
    print!("\x1b[2K{line}\r\n");
}

fn push_found_block(blocks: &mut Vec<FoundBlock>, block: FoundBlock) {
    blocks.insert(0, block);
    blocks.truncate(8);
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

fn compact_status(status: &str) -> String {
    status.split_whitespace().collect::<Vec<&str>>().join(" ")
}

fn truncate_for_table(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        value.to_string()
    } else if max_len <= 3 {
        value[..max_len].to_string()
    } else {
        format!("{}...", &value[..max_len - 3])
    }
}
