//! `schlange`: one static binary for the merge queue. Phase 0 offers configuration validation,
//! scenarios in the simulated forge, the support matrix and a minimal doctor.
//!
//! Exit codes: 0 ok, 1 invalid or failed, 2 valid for Mergify but not supported (`--strict`),
//! 3 environment problem (missing schema, unreadable file).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

use schlange_config::features::{FeatureMap, Priority, Status};
use schlange_config::validate::exit;
use schlange_config::{Schema, schema};

#[derive(Parser)]
#[command(
    name = "schlange",
    version,
    about = "Merge queue, drop-in for Mergify's .mergify.yml"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with a configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Scenarios in the simulated forge.
    Sim {
        #[command(subcommand)]
        command: SimCommand,
    },
    /// Which features Schlange supports, from the feature map's status.
    SupportMatrix {
        #[arg(long)]
        json: bool,
    },
    /// Checks the installation. Exit 0 without FAIL; serves as the container health check.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Mergify JSON schema (default: $SCHLANGE_SCHEMA or .research/mergify/schema.json).
        #[arg(long)]
        schema: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Validates a configuration: YAML, Mergify schema, conditions, support by Schlange.
    Validate {
        file: PathBuf,
        /// Mergify JSON schema (default: $SCHLANGE_SCHEMA or .research/mergify/schema.json).
        #[arg(long)]
        schema: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Exit 2 when the configuration uses features Schlange does not support yet.
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Subcommand)]
enum SimCommand {
    /// Runs a scenario: event log, expectations and invariant report. Exit 0 when all
    /// expectations hold.
    Run {
        file: PathBuf,
        #[arg(long)]
        json: bool,
        /// `sim`: git in memory. `localgit`: real git repositories in a temporary directory,
        /// every check a process in a checkout (needs git ≥ 2.38 and a build with the
        /// `localgit` feature).
        #[arg(long, value_enum, default_value_t = ForgeKind::Sim)]
        forge: ForgeKind,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ForgeKind {
    Sim,
    Localgit,
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("cannot serialise output: {e}"),
    }
}

fn env_error(json_out: bool, message: &str) -> ExitCode {
    if json_out {
        print_json(&json!({ "error": message, "exit_code": exit::ENVIRONMENT }));
    } else {
        eprintln!("schlange: {message}");
    }
    ExitCode::from(exit::ENVIRONMENT)
}

fn features() -> Result<FeatureMap, String> {
    FeatureMap::embedded().map_err(|e| format!("embedded feature map is broken: {e}"))
}

fn validate(file: &Path, schema_path: Option<&Path>, json_out: bool, strict: bool) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(path) = schema::locate(schema_path, &cwd) else {
        return env_error(json_out, &schema::SchemaError::NotFound.to_string());
    };
    let schema = match Schema::load(&path) {
        Ok(s) => s,
        Err(e) => return env_error(json_out, &e.to_string()),
    };
    let features = match features() {
        Ok(f) => f,
        Err(e) => return env_error(json_out, &e),
    };
    let source = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => return env_error(json_out, &format!("cannot read {}: {e}", file.display())),
    };
    let report = schlange_config::validate(
        &file.display().to_string(),
        &source,
        &schema,
        &features,
        strict,
    );
    if json_out {
        print_json(&report);
    } else {
        print!("{}", report.to_text());
    }
    ExitCode::from(report.exit_code)
}

fn sim_run(file: &Path, json_out: bool, forge: ForgeKind) -> ExitCode {
    let result = match forge {
        ForgeKind::Sim => schlange_forge::scenario::run_file(file).map_err(|e| e.to_string()),
        ForgeKind::Localgit => schlange_forge::run_file_localgit(file),
    };
    match result {
        Ok(report) => {
            if json_out {
                print_json(&report);
            } else {
                print!("{}", report.to_text());
            }
            ExitCode::from(if report.passed { 0 } else { 1 })
        }
        Err(e) => env_error(json_out, &e),
    }
}

#[derive(Serialize)]
struct MatrixRow {
    feature: String,
    title: String,
    priority: Priority,
    arztagent: bool,
    status: Status,
    supported: bool,
    entries: usize,
}

fn support_matrix(json_out: bool) -> ExitCode {
    let map = match features() {
        Ok(f) => f,
        Err(e) => return env_error(json_out, &e),
    };
    let rows: Vec<MatrixRow> = map
        .features
        .iter()
        .map(|f| MatrixRow {
            feature: f.id.clone(),
            title: f.title.clone(),
            priority: f.priority,
            arztagent: f.arztagent,
            status: f.status,
            supported: f.status.is_supported(),
            entries: map
                .inventory
                .entries
                .iter()
                .filter(|e| {
                    f.inventory
                        .iter()
                        .any(|p| schlange_config::features::pattern_matches(p, &e.id))
                })
                .count(),
        })
        .collect();
    if json_out {
        print_json(&json!({ "inventory": map.inventory.counts, "features": rows }));
    } else {
        let mut rows = rows;
        rows.sort_by(|a, b| (a.priority, &a.feature).cmp(&(b.priority, &b.feature)));
        println!(
            "{:<4} {:<11} {:<9} {:>7}  feature",
            "prio", "status", "arztagent", "entries"
        );
        for r in &rows {
            println!(
                "{:<4} {:<11} {:<9} {:>7}  {}",
                format!("{:?}", r.priority),
                r.status.as_str(),
                if r.arztagent { "yes" } else { "" },
                r.entries,
                r.feature
            );
        }
    }
    ExitCode::SUCCESS
}

#[derive(Serialize)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

fn doctor(json_out: bool, schema_path: Option<&Path>) -> ExitCode {
    let mut checks = vec![Check {
        name: "version",
        status: "OK",
        detail: format!(
            "schlange {} ({}-{})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH,
            std::env::consts::OS
        ),
    }];
    match features() {
        Ok(map) => {
            let problems = map.coverage_problems();
            let mut by_prio: BTreeMap<String, usize> = BTreeMap::new();
            for f in &map.features {
                *by_prio.entry(format!("{:?}", f.priority)).or_default() += 1;
            }
            let supported = map
                .features
                .iter()
                .filter(|f| f.status.is_supported())
                .count();
            checks.push(Check {
                name: "support-matrix",
                status: if problems.is_empty() { "OK" } else { "FAIL" },
                detail: if problems.is_empty() {
                    format!(
                        "{} features ({}), {} supported; {} inventory entries",
                        map.features.len(),
                        by_prio
                            .iter()
                            .map(|(k, v)| format!("{k} {v}"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        supported,
                        map.inventory.total
                    )
                } else {
                    problems.join("; ")
                },
            });
        }
        Err(e) => checks.push(Check {
            name: "support-matrix",
            status: "FAIL",
            detail: e,
        }),
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    checks.push(match schema::locate(schema_path, &cwd) {
        None => Check {
            name: "schema",
            status: "WARN",
            detail: "no Mergify schema found; `config validate` needs --schema (not embedded until its licence is cleared)".into(),
        },
        Some(p) => match Schema::load(&p) {
            Ok(_) => Check {
                name: "schema",
                status: "OK",
                detail: p.display().to_string(),
            },
            Err(e) => Check {
                name: "schema",
                status: "FAIL",
                detail: e.to_string(),
            },
        },
    });
    for name in ["github-app", "ruleset-exception", "webhooks", "rate-limit"] {
        checks.push(Check {
            name,
            status: "SKIP",
            detail: "not built yet (needs the github adapter)".into(),
        });
    }
    let failed = checks.iter().any(|c| c.status == "FAIL");
    if json_out {
        print_json(&json!({ "ok": !failed, "checks": checks }));
    } else {
        for c in &checks {
            println!("{:<5} {:<18} {}", c.status, c.name, c.detail);
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Config {
            command:
                ConfigCommand::Validate {
                    file,
                    schema,
                    json,
                    strict,
                },
        } => validate(&file, schema.as_deref(), json, strict),
        Command::Sim {
            command: SimCommand::Run { file, json, forge },
        } => sim_run(&file, json, forge),
        Command::SupportMatrix { json } => support_matrix(json),
        Command::Doctor { json, schema } => doctor(json, schema.as_deref()),
    }
}
