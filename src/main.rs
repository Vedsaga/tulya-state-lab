use std::env;
use std::process::ExitCode;
use tulya_state_lab::avl::AvlRope;
use tulya_state_lab::cdc::CdcStore;
use tulya_state_lab::workload::{run_backend, verify_pair, Config, Report, Workload};

#[derive(Clone, Debug)]
struct Cli {
    config: Config,
    leaf_bytes: usize,
    avg_chunk_bytes: usize,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            config: Config::default(),
            leaf_bytes: 4096,
            avg_chunk_bytes: 4096,
        }
    }
}

fn usage() -> &'static str {
    "state-lab [options]\n\n\
     Options:\n\
       --branches N          number of child versions (default 1000)\n\
       --base-mib N          base state size in MiB (default 2)\n\
       --base-kib N          base state size in KiB (overrides --base-mib)\n\
       --edit-bytes N        maximum inserted/deleted bytes per edit (default 96)\n\
       --read-bytes N        range-read bytes per child (default 4096)\n\
       --leaf-bytes N        AVL rope leaf size (default 4096)\n\
       --avg-chunk-bytes N   CDC target average chunk size (default 4096)\n\
       --verify-samples N    full cross-backend samples (default 16)\n\
       --seed N              deterministic u64 seed (decimal or 0xHEX)\n\
       --help                print this help\n"
}

fn parse_u64(s: &str) -> Result<u64, String> {
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|e| format!("invalid integer {s}: {e}"))
    } else {
        s.parse::<u64>()
            .map_err(|e| format!("invalid integer {s}: {e}"))
    }
}

fn parse_usize(s: &str) -> Result<usize, String> {
    let value = parse_u64(s)?;
    usize::try_from(value).map_err(|_| format!("value does not fit usize: {s}"))
}

fn checked_scale(value: usize, scale: usize, flag: &str) -> Result<usize, String> {
    value
        .checked_mul(scale)
        .ok_or_else(|| format!("{flag} is too large"))
}

fn parse_cli() -> Result<Cli, String> {
    let mut cli = Cli::default();
    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0usize;
    while i < args.len() {
        let flag = &args[i];
        if flag == "--help" || flag == "-h" {
            println!("{}", usage());
            std::process::exit(0);
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--branches" => cli.config.branches = parse_usize(value)?,
            "--base-mib" => {
                cli.config.base_bytes = checked_scale(parse_usize(value)?, 1024 * 1024, flag)?
            }
            "--base-kib" => {
                cli.config.base_bytes = checked_scale(parse_usize(value)?, 1024, flag)?
            }
            "--edit-bytes" => cli.config.max_edit_bytes = parse_usize(value)?,
            "--read-bytes" => cli.config.read_bytes = parse_usize(value)?,
            "--leaf-bytes" => cli.leaf_bytes = parse_usize(value)?,
            "--avg-chunk-bytes" => cli.avg_chunk_bytes = parse_usize(value)?,
            "--verify-samples" => cli.config.verify_samples = parse_usize(value)?,
            "--seed" => cli.config.seed = parse_u64(value)?,
            _ => return Err(format!("unknown option: {flag}\n\n{}", usage())),
        }
        i += 2;
    }

    if cli.leaf_bytes == 0 {
        return Err("--leaf-bytes must be positive".into());
    }
    if cli.avg_chunk_bytes < 64 {
        return Err("--avg-chunk-bytes must be at least 64".into());
    }
    Ok(cli)
}

fn ns_to_us(ns: u64) -> f64 {
    ns as f64 / 1_000.0
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn print_report(report: &Report, branches: usize) {
    let initial_retained = report.initial_stats.retained_bytes();
    let final_retained = report.final_stats.retained_bytes();
    let retained_growth = final_retained.saturating_sub(initial_retained);
    let initial_lifetime = report.initial_stats.lifetime_allocated_bytes();
    let final_lifetime = report.final_stats.lifetime_allocated_bytes();
    let lifetime_growth = final_lifetime.saturating_sub(initial_lifetime);
    let denom = branches.max(1) as f64;

    println!("backend: {}", report.backend);
    println!("  build_ms: {:.3}", report.build_ns as f64 / 1_000_000.0);
    println!(
        "  edit_us p50/p95/p99: {:.3} / {:.3} / {:.3}",
        ns_to_us(report.edit.p50_ns),
        ns_to_us(report.edit.p95_ns),
        ns_to_us(report.edit.p99_ns)
    );
    println!(
        "  read_us p50/p95/p99: {:.3} / {:.3} / {:.3}",
        ns_to_us(report.read.p50_ns),
        ns_to_us(report.read.p95_ns),
        ns_to_us(report.read.p99_ns)
    );
    println!(
        "  retained_payload_mib: {:.3}",
        mib(report.final_stats.retained_payload_bytes)
    );
    println!(
        "  retained_metadata_mib_est: {:.3}",
        mib(report.final_stats.retained_metadata_bytes)
    );
    println!("  retained_total_mib_est: {:.3}", mib(final_retained));
    println!(
        "  retained_growth_bytes_per_branch_est: {:.1}",
        retained_growth as f64 / denom
    );
    println!(
        "  lifetime_alloc_bytes_per_branch_est: {:.1}",
        lifetime_growth as f64 / denom
    );
    println!(
        "  live_objects / allocated_objects: {} / {}",
        report.final_stats.live_objects, report.final_stats.total_objects_allocated
    );
    println!("  checksum: {:016x}", report.checksum);
}

fn run() -> Result<(), String> {
    let cli = parse_cli()?;
    println!("tulya-state-lab phase 1");
    println!(
        "config: branches={}, base_bytes={}, max_edit_bytes={}, read_bytes={}, leaf_bytes={}, avg_chunk_bytes={}, seed=0x{:016x}",
        cli.config.branches,
        cli.config.base_bytes,
        cli.config.max_edit_bytes,
        cli.config.read_bytes,
        cli.leaf_bytes,
        cli.avg_chunk_bytes,
        cli.config.seed
    );
    println!("generating deterministic workload...");
    let workload = Workload::generate(cli.config.clone());
    println!(
        "logical bytes across retained versions: {} ({:.3} GiB)",
        workload.logical_version_bytes,
        workload.logical_version_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );

    println!("\nrunning persistent AVL rope...");
    let avl = run_backend(AvlRope::new(cli.leaf_bytes), &workload).map_err(|e| e.to_string())?;
    print_report(&avl.report, cli.config.branches);

    println!("\nrunning simple CDC dedup...");
    let cdc = run_backend(CdcStore::new(cli.avg_chunk_bytes), &workload).map_err(|e| e.to_string())?;
    print_report(&cdc.report, cli.config.branches);

    println!("\nverifying sampled versions across backends...");
    verify_pair(&avl, &cdc, &workload)?;
    println!("semantic cross-check: PASS");

    let avl_growth = avl
        .report
        .final_stats
        .retained_bytes()
        .saturating_sub(avl.report.initial_stats.retained_bytes());
    let cdc_growth = cdc
        .report
        .final_stats
        .retained_bytes()
        .saturating_sub(cdc.report.initial_stats.retained_bytes());
    println!("\ncomparison (lower is better; estimates exclude allocator/hash-table overhead):");
    println!("  retained growth: AVL={} bytes, CDC={} bytes", avl_growth, cdc_growth);
    if cdc_growth > 0 {
        println!("  AVL/CDC retained-growth ratio: {:.3}x", avl_growth as f64 / cdc_growth as f64);
    }
    println!(
        "  edit p95 ratio AVL/CDC: {:.3}x",
        avl.report.edit.p95_ns as f64 / cdc.report.edit.p95_ns.max(1) as f64
    );
    println!(
        "  read p95 ratio AVL/CDC: {:.3}x",
        avl.report.read.p95_ns as f64 / cdc.report.read.p95_ns.max(1) as f64
    );
    println!("\nNo representation is promoted by this program; interpret the numbers against the README kill criteria.");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
