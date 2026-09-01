//! Deterministic, no-network browser-pipeline replay runner for Lumen optimization gates.

use serde_json::json;
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

#[derive(Debug)]
struct Config {
    warmups: usize,
    samples: usize,
    fixtures: Vec<PathBuf>,
    externals: Vec<(String, PathBuf)>,
    sheets: Vec<(String, PathBuf)>,
}

fn usage() -> &'static str {
    "usage: trust-browser-replay [--warmups N] [--samples N] [--external NAME=PATH] [--sheet NAME=PATH] FIXTURE.html [...]"
}

fn named_path(value: String, option: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = value
        .split_once('=')
        .ok_or_else(|| format!("{option} requires NAME=PATH"))?;
    if name.is_empty() || path.is_empty() {
        return Err(format!("{option} requires nonempty NAME=PATH"));
    }
    Ok((name.to_string(), PathBuf::from(path)))
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut warmups = 1usize;
    let mut samples = 5usize;
    let mut fixtures = Vec::new();
    let mut externals = Vec::new();
    let mut sheets = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--warmups" => {
                warmups = args
                    .next()
                    .ok_or("--warmups requires a value")?
                    .parse()
                    .map_err(|_| "--warmups must be a non-negative integer")?;
            }
            "--samples" => {
                samples = args
                    .next()
                    .ok_or("--samples requires a value")?
                    .parse()
                    .map_err(|_| "--samples must be a positive integer")?;
            }
            "--external" => externals.push(named_path(
                args.next().ok_or("--external requires NAME=PATH")?,
                "--external",
            )?),
            "--sheet" => sheets.push(named_path(
                args.next().ok_or("--sheet requires NAME=PATH")?,
                "--sheet",
            )?),
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option {argument:?}; try --help"));
            }
            _ => fixtures.push(PathBuf::from(argument)),
        }
    }
    if samples == 0 {
        return Err("--samples must be positive".to_string());
    }
    if fixtures.is_empty() {
        return Err("at least one replay fixture is required".to_string());
    }
    Ok(Some(Config {
        warmups,
        samples,
        fixtures,
        externals,
        sheets,
    }))
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn fixture_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("replay")
        .to_string()
}

fn replay_attribute<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=\"");
    let rest = html.split_once(&prefix)?.1;
    rest.split_once('"').map(|(checksum, _)| checksum)
}

fn run() -> Result<(serde_json::Value, bool), String> {
    let Some(config) = parse_args()? else {
        println!("{}", usage());
        return Ok((serde_json::Value::Null, true));
    };
    let mut fixtures = Vec::with_capacity(config.fixtures.len());
    for path in &config.fixtures {
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        fixtures.push((
            fixture_id(path),
            path.clone(),
            sha256(source.as_bytes()),
            source,
        ));
    }
    let externals = config
        .externals
        .iter()
        .map(|(name, path)| {
            std::fs::read(path)
                .map(|bytes| {
                    let digest = sha256(&bytes);
                    (
                        name.clone(),
                        path.clone(),
                        digest,
                        std::sync::Arc::new(bytes),
                    )
                })
                .map_err(|error| format!("read external {}: {error}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let sheets = config
        .sheets
        .iter()
        .map(|(name, path)| {
            std::fs::read_to_string(path)
                .map(|text| (name.clone(), path.clone(), sha256(text.as_bytes()), text))
                .map_err(|error| format!("read sheet {}: {error}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut results = Vec::new();
    let mut failure = None;
    for (phase, rounds) in [("warmup", config.warmups), ("measure", config.samples)] {
        for round in 0..rounds {
            // Rotate the first workload so thermal/order effects do not belong to one fixture.
            for offset in 0..fixtures.len() {
                let index = (round + offset) % fixtures.len();
                let (id, path, source_sha256, source) = &fixtures[index];
                let url = format!("https://replay.invalid/{id}.html");
                let mut environment = trust::js::PageEnv::bare(&url);
                environment.externals = externals
                    .iter()
                    .map(|(name, _, _, bytes)| (name.clone(), Some(bytes.clone())))
                    .collect();
                environment.sheets = sheets
                    .iter()
                    .map(|(name, _, _, text)| (name.clone(), text.clone()))
                    .collect();
                let started = Instant::now();
                let (output, outcome) = trust::js::transform(source, &environment);
                let wall_seconds = started.elapsed().as_secs_f64();
                let checksum = replay_attribute(&output, "data-replay-checksum").map(str::to_owned);
                let expected = replay_attribute(&output, "data-replay-expected").map(str::to_owned);
                let marker_complete = output.contains("data-replay-state=\"complete\"");
                let valid = !outcome.panicked
                    && outcome.errors.is_empty()
                    && marker_complete
                    && checksum.as_deref().is_some_and(|value| !value.is_empty())
                    && checksum == expected;
                let sample_error = if valid {
                    None
                } else {
                    Some(format!(
                        "{id} did not complete: panicked={}, errors={}, marker={}, checksum={checksum:?}, expected={expected:?}",
                        outcome.panicked,
                        outcome.errors.len(),
                        marker_complete,
                    ))
                };
                if failure.is_none() {
                    failure.clone_from(&sample_error);
                }
                results.push(json!({
                    "phase": phase,
                    "round": round,
                    "workload": id,
                    "path": path,
                    "source_sha256": source_sha256,
                    "wall_seconds": wall_seconds,
                    "engine_elapsed_seconds": outcome.elapsed.as_secs_f64(),
                    "output_bytes": output.len(),
                    "output_sha256": sha256(output.as_bytes()),
                    "replay_checksum": checksum,
                    "replay_expected": expected,
                    "marker_complete": marker_complete,
                    "errors": outcome.errors,
                    "panicked": outcome.panicked,
                    "fetches": outcome.fetches,
                    "console": outcome.console,
                    "valid": valid,
                    "error": sample_error,
                }));
            }
        }
    }

    let success = failure.is_none();
    Ok((
        json!({
            "schema_version": 1,
            "status": if success { "complete" } else { "failed" },
            "offline": true,
            "virtual_time": true,
            "warmup_rounds": config.warmups,
            "sample_rounds": config.samples,
            "workloads": fixtures.iter().map(|(id, path, sha256, _)| json!({
                "id": id,
                "path": path,
                "source_sha256": sha256,
            })).collect::<Vec<_>>(),
            "external_scripts": externals.iter().map(|(name, path, sha256, _)| json!({
                "name": name,
                "path": path,
                "sha256": sha256,
            })).collect::<Vec<_>>(),
            "external_sheets": sheets.iter().map(|(name, path, sha256, _)| json!({
                "name": name,
                "path": path,
                "sha256": sha256,
            })).collect::<Vec<_>>(),
            "samples": results,
            "error": failure,
        }),
        success,
    ))
}

fn main() -> ExitCode {
    match run() {
        Ok((serde_json::Value::Null, _)) => ExitCode::SUCCESS,
        Ok((report, success)) => {
            println!("{report}");
            if success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("trust-browser-replay: {error}\n{}", usage());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::replay_attribute;

    #[test]
    fn extracts_a_nonempty_replay_checksum() {
        assert_eq!(
            replay_attribute(
                "<output data-replay-checksum=\"42:ok\"></output>",
                "data-replay-checksum"
            ),
            Some("42:ok")
        );
        assert_eq!(
            replay_attribute("<output></output>", "data-replay-checksum"),
            None
        );
    }
}
