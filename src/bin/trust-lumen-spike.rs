use lumen::bytecode::Tier;
use std::path::PathBuf;

const DEFAULT_BENCHMARK: &str = "/usr/share/cry/benchmarks/js-engine-benchmark/run.js";

fn main() {
    if let Err(error) = run() {
        eprintln!("trust-lumen-spike: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut tier = Tier::Jit;
    let mut threshold = 0u32;
    let mut benchmark = PathBuf::from(DEFAULT_BENCHMARK);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tier" => {
                let value = args.next().ok_or("--tier requires a value")?;
                tier = trust::lumen_backend::parse_tier(&value)?;
            }
            "--threshold" => {
                let value = args.next().ok_or("--threshold requires a value")?;
                threshold = value
                    .parse()
                    .map_err(|_| format!("invalid --threshold value {value:?}"))?;
            }
            "--benchmark" => {
                benchmark = PathBuf::from(args.next().ok_or("--benchmark requires a path")?);
            }
            "--help" | "-h" => {
                println!(
                    "usage: trust-lumen-spike [--tier interp|bytecode|jit] [--threshold N] [--benchmark PATH]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}; try --help")),
        }
    }

    let report = trust::lumen_backend::run_benchmark(&benchmark, tier, threshold)?;
    println!("{}", report.logs);
    println!("---- TRust/Lumen integration ----");
    println!("tier: {:?} (threshold {threshold})", report.tier);
    println!("prelude: {:.3}s", report.prelude_time.as_secs_f64());
    println!(
        "benchmark/event loop: {:.3}s",
        report.benchmark_time.as_secs_f64()
    );
    println!("timer turns: {}", report.timer_turns);
    println!(
        "idle GC: {:.3}s, {} -> {} live objects ({} reclaimed)",
        report.idle_gc_time.as_secs_f64(),
        report.pre_idle_live_objects,
        report.post_idle_live_objects,
        report.idle_gc_reclaimed
    );
    println!(
        "verification GC: {} -> {} live objects ({} reclaimed)",
        report.post_idle_live_objects, report.post_final_live_objects, report.final_gc_reclaimed
    );
    println!(
        "score: {}",
        report.score.as_deref().unwrap_or("<not reported>")
    );
    Ok(())
}
