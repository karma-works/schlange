//! `schlange`: one static binary for the merge queue: configuration validation and simulation,
//! scenarios in the simulated forge, the support matrix, the doctor, `serve` (the GitHub App,
//! also in shadow mode), and the clients of its API: `queue`, `explain`, `events`, `audit`,
//! `shadow report`.
//!
//! Exit codes: 0 ok, 1 invalid or failed, 2 valid for Mergify but not supported (`--strict`),
//! 3 environment problem (missing schema, unreadable file), 64 usage error.

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

// Parsed once per process; the size of `serve`'s flags does not matter.
#[allow(clippy::large_enum_variant)]
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
    /// The queue of a running `schlange serve` through its queue API (needs the admin token).
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
        /// Base URL of `schlange serve` (its `--listen` address, or the URL in front of it).
        #[arg(
            long,
            global = true,
            env = "SCHLANGE_URL",
            default_value = "http://127.0.0.1:8080"
        )]
        url: String,
        /// The repository, `owner/name`.
        #[arg(long, global = true, env = "SCHLANGE_REPO", default_value = "")]
        repo: String,
        /// File holding the admin token (or SCHLANGE_ADMIN_TOKEN).
        #[arg(long, global = true, env = "SCHLANGE_ADMIN_TOKEN_FILE")]
        admin_token_file: Option<PathBuf>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Why a pull request is (not) queued, not merged, or was dequeued: conditions with the
    /// failing sub-conditions, holds, its place in the queue, its batch, the last dequeue reason
    /// (S-EXPLAIN). From a running `serve`, or from the end of a scenario (`--scenario`).
    Explain {
        pr: u64,
        /// Explain from the end state of this scenario in the simulated forge.
        #[arg(long)]
        scenario: Option<PathBuf>,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// The event log: queue and action events, newest first (`GET …/logs` of a running
    /// `serve`, or its data directory with `--data-dir`).
    Events {
        /// Only events of this pull request (also as part of a batch).
        #[arg(long)]
        pr: Option<u64>,
        /// Only events at or after this time (RFC 3339 or Unix seconds).
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u64,
        /// Read the SQLite database in this data directory instead of calling `serve`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// The evidence of every landing (S-AUDIT): tested head and tree, expected base, landed
    /// commit, tree and parent.
    Audit {
        #[arg(long)]
        pr: Option<u64>,
        /// Read the SQLite database in this data directory instead of calling `serve`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Shadow mode (`serve --shadow`): what Schlange would have done next to Mergify.
    Shadow {
        #[command(subcommand)]
        command: ShadowCommand,
    },
    /// Which features Schlange supports, from the feature map's status.
    SupportMatrix {
        #[arg(long)]
        json: bool,
    },
    /// Checks the installation. Exit 0 without FAIL; serves as the container health check.
    /// With GitHub credentials (flags or environment) it also checks the app, its permissions,
    /// the ruleset exception, webhook deliveries and the rate limit.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Mergify JSON schema (default: $SCHLANGE_SCHEMA or .research/mergify/schema.json).
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Skip the checks that call GitHub (the container health check uses this).
        #[arg(long)]
        offline: bool,
        /// The configuration to check (default: `.mergify.yml` and the other configuration
        /// names in the working directory).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Check for shadow mode: a read-only app is enough, no ruleset exception needed.
        #[arg(long, env = "SCHLANGE_SHADOW")]
        shadow: bool,
        #[command(flatten)]
        github: GithubArgs,
    },
    /// Runs the merge queue as a GitHub App: webhook endpoint, reconciliation, landing.
    Serve {
        #[command(flatten)]
        github: GithubArgs,
        /// Webhook secret (at least 16 bytes). Prefer the environment or
        /// `--webhook-secret-file`: a value on the command line is visible to every user of
        /// the machine (`ps`), so it draws a warning.
        #[arg(long, env = "SCHLANGE_WEBHOOK_SECRET", hide_env_values = true)]
        webhook_secret: Option<String>,
        /// File holding the webhook secret.
        #[arg(long, env = "SCHLANGE_WEBHOOK_SECRET_FILE")]
        webhook_secret_file: Option<PathBuf>,
        /// Address of the endpoint (`POST /webhook`, `GET /healthz`, `GET /status`).
        #[arg(long, env = "SCHLANGE_LISTEN", default_value = "0.0.0.0:8080")]
        listen: std::net::SocketAddr,
        /// Data directory: the SQLite database (journal, snapshots, webhook dedup, audit).
        #[arg(long, env = "SCHLANGE_DATA_DIR", default_value = "/data")]
        data_dir: PathBuf,
        /// Mergify JSON schema for validating the configuration (default: $SCHLANGE_SCHEMA or
        /// .research/mergify/schema.json). `serve` does not start without it.
        #[arg(long)]
        schema: Option<PathBuf>,
        /// File holding the admin token for `GET /status` (or SCHLANGE_ADMIN_TOKEN). Without
        /// one, `/status` is off.
        #[arg(long, env = "SCHLANGE_ADMIN_TOKEN_FILE")]
        admin_token_file: Option<PathBuf>,
        /// Seconds between full reconciliations.
        #[arg(long, env = "SCHLANGE_RECONCILE_EVERY", default_value_t = 300)]
        reconcile_every: u64,
        /// A local configuration instead of `.mergify.yml` from each repository.
        #[arg(long, env = "SCHLANGE_CONFIG")]
        config: Option<PathBuf>,
        /// Name of the queue's own check run on queued pull requests.
        #[arg(
            long,
            env = "SCHLANGE_CHECK_NAME",
            default_value = "Schlange merge queue"
        )]
        check_name: String,
        /// Shadow mode: decide as usual, write nothing to GitHub, compare with Mergify
        /// (`schlange shadow report`).
        #[arg(long, env = "SCHLANGE_SHADOW")]
        shadow: bool,
        /// Shadow mode: Mergify's bot login.
        #[arg(long, env = "SCHLANGE_MERGIFY_LOGIN", default_value = "mergify[bot]")]
        mergify_login: String,
        /// Shadow mode: Mergify's app slug (its check runs).
        #[arg(long, env = "SCHLANGE_MERGIFY_APP", default_value = "mergify")]
        mergify_app: String,
        /// Seconds a repository's lease lasts without renewal (MQ-32: one writer per
        /// repository; renewed after half of it).
        #[arg(long, env = "SCHLANGE_LEASE_TTL", default_value_t = 60)]
        lease_ttl: u64,
        /// This process's name in the lease (default: host name, process id).
        #[arg(long, env = "SCHLANGE_LEASE_HOLDER")]
        lease_holder: Option<String>,
        /// No lease: only when exactly one process serves the repositories.
        #[arg(long, env = "SCHLANGE_NO_LEASE")]
        no_lease: bool,
    },
}

/// How to reach a running `serve` (its admin API).
#[derive(clap::Args, Debug, Clone)]
struct ApiArgs {
    /// Base URL of `schlange serve` (its `--listen` address, or the URL in front of it).
    #[arg(long, env = "SCHLANGE_URL", default_value = "http://127.0.0.1:8080")]
    url: String,
    /// The repository, `owner/name`.
    #[arg(long, env = "SCHLANGE_REPO", default_value = "")]
    repo: String,
    /// File holding the admin token (or SCHLANGE_ADMIN_TOKEN).
    #[arg(long, env = "SCHLANGE_ADMIN_TOKEN_FILE")]
    admin_token_file: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum ShadowCommand {
    /// Agreements and disagreements with Mergify per pull request: queued, landed, landing
    /// order, batch, dequeue reason. Exit 0 without disagreements, 1 with.
    Report {
        /// Read the SQLite database in this data directory instead of calling `serve`.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[command(flatten)]
        api: ApiArgs,
    },
}

#[derive(clap::Args, Debug)]
struct GithubArgs {
    /// GitHub App id (or client id).
    #[arg(long, env = "SCHLANGE_GITHUB_APP_ID")]
    app_id: Option<String>,
    /// Path of the app's private key (PEM).
    #[arg(long, env = "SCHLANGE_GITHUB_PRIVATE_KEY")]
    private_key: Option<PathBuf>,
    /// Installation id; looked up per repository when absent.
    #[arg(long, env = "SCHLANGE_GITHUB_INSTALLATION_ID")]
    installation_id: Option<u64>,
    /// Repositories (`owner/name`), comma-separated.
    #[arg(long = "repo", env = "SCHLANGE_REPOS", value_delimiter = ',')]
    repos: Vec<String>,
    /// API base URL (tests point it at a fake GitHub). Must be `https://` unless
    /// `--insecure-test-api` is given.
    #[arg(
        long,
        env = "SCHLANGE_GITHUB_API_URL",
        default_value = "https://api.github.com"
    )]
    api_url: String,
    /// Allow a plain `http://` API URL. Only for tests against a fake GitHub: the app's JWT and
    /// installation tokens would travel unencrypted.
    #[arg(long, env = "SCHLANGE_INSECURE_TEST_API")]
    insecure_test_api: bool,
}

impl GithubArgs {
    fn configured(&self) -> bool {
        self.app_id.is_some() && self.private_key.is_some()
    }

    fn client(&self) -> Result<schlange_github::Client, String> {
        let (Some(app_id), Some(key_path)) = (&self.app_id, &self.private_key) else {
            return Err("needs --app-id and --private-key (or SCHLANGE_GITHUB_APP_ID and SCHLANGE_GITHUB_PRIVATE_KEY)".into());
        };
        if !self.api_url.starts_with("https://") {
            if !self.insecure_test_api {
                return Err(format!(
                    "the API URL {} is not https://; refusing to send credentials over it (for tests against a fake GitHub only: --insecure-test-api)",
                    self.api_url
                ));
            }
            eprintln!(
                "schlange: WARNING: --insecure-test-api: credentials go unencrypted to {}",
                self.api_url
            );
        }
        let pem = std::fs::read(key_path)
            .map_err(|e| format!("cannot read {}: {e}", key_path.display()))?;
        let key = schlange_github::jwt::AppKey::from_pem(&pem).map_err(|e| e.to_string())?;
        Ok(schlange_github::Client::new(
            schlange_github::ClientConfig {
                api_base: self.api_url.clone(),
                app_id: app_id.clone(),
                key: std::sync::Arc::new(key),
                retry: Default::default(),
                budget: Default::default(),
                timeout: std::time::Duration::from_secs(30),
                allow_insecure_http: self.insecure_test_api,
            },
        ))
    }

    fn repos(&self) -> Result<Vec<schlange_github::RepoName>, String> {
        self.repos
            .iter()
            .filter(|r| !r.trim().is_empty())
            .map(|r| {
                schlange_github::RepoName::parse(r.trim())
                    .ok_or_else(|| format!("`{r}` is not owner/name"))
            })
            .collect()
    }
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
    /// Evaluates a new configuration against a pull request without acting: would it be
    /// queued, under which rule, which conditions fail (S-SIMULATE). Against a running `serve`
    /// (its engine state), or the end of a scenario (`--scenario`). Exit 0, 1 when the
    /// configuration is invalid.
    Simulate {
        file: PathBuf,
        #[arg(long)]
        pr: u64,
        /// Evaluate against the end state of this scenario in the simulated forge.
        #[arg(long)]
        scenario: Option<PathBuf>,
        /// Mergify JSON schema (with `--scenario`; default: $SCHLANGE_SCHEMA or
        /// .research/mergify/schema.json).
        #[arg(long)]
        schema: Option<PathBuf>,
        #[command(flatten)]
        api: ApiArgs,
    },
}

#[derive(Subcommand)]
enum QueueCommand {
    /// The batches being checked and the pull requests waiting (`GET merge-queue/status`).
    Status,
    /// Pauses the whole merge queue: nothing is checked or merged until `unpause`.
    Pause {
        #[arg(long)]
        reason: String,
    },
    /// Resumes a paused merge queue.
    Unpause,
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

#[derive(Clone, Serialize)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

/// The doctor's check of a local configuration: `--config <file>`, or the file the
/// configuration search finds in the working directory. `None` when neither exists (then only
/// the default branch on GitHub is checked, when an app is configured). Validated by the same
/// validator `serve` uses (stages 1 to 4, stage 5 as warnings) when the schema is found,
/// without the schema otherwise (stages 1, 3 and 4).
fn local_config_check(
    explicit: Option<&Path>,
    cwd: &Path,
    validator: Option<&schlange_github::config::ConfigCheck>,
) -> Option<Check> {
    let fail = |detail: String| Check {
        name: "configuration",
        status: "FAIL",
        detail,
    };
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            let located = schlange_config::config_file::locate(|n| cwd.join(n).is_file());
            if let Some(e) = located.error() {
                return Some(fail(format!("{}: {e}", cwd.display())));
            }
            match located {
                schlange_config::config_file::Located::One(n) => cwd.join(n),
                _ => return None,
            }
        }
    };
    let shown = path.display().to_string();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return Some(fail(format!("cannot read {shown}: {e}"))),
    };
    let check = match validator {
        Some(v) => match v.check(&shown, &text) {
            Err(e) => fail(format!("{shown} is invalid: {e}")),
            Ok(l) if l.warnings.is_empty() => Check {
                name: "configuration",
                status: "OK",
                detail: format!("{shown} is valid"),
            },
            Ok(l) => Check {
                name: "configuration",
                status: "WARN",
                detail: format!(
                    "{shown} is valid; {} findings not supported by Schlange yet \
                     (`schlange config validate {shown}` lists them)",
                    l.warnings.len()
                ),
            },
        },
        None => {
            use schlange_config::validate::Severity;
            let errors: Vec<String> =
                schlange_config::config_file::validate_without_schema(&shown, &text)
                    .into_iter()
                    .filter(|f| f.severity == Severity::Error)
                    .map(|f| f.message)
                    .collect();
            if errors.is_empty() {
                Check {
                    name: "configuration",
                    status: "WARN",
                    detail: format!(
                        "{shown} is valid without the Mergify schema (stages 1, 3, 4); no schema \
                         found for the full check"
                    ),
                }
            } else {
                fail(format!(
                    "{shown} is invalid (without the Mergify schema): {}",
                    errors.join("; ")
                ))
            }
        }
    };
    Some(check)
}

/// One `configuration` check from the local file and the default branch on GitHub: the worse
/// status, both details.
fn merge_config_checks(local: Check, github: Check) -> Check {
    let rank = |s: &str| match s {
        "FAIL" => 3,
        "WARN" => 2,
        "OK" => 1,
        _ => 0,
    };
    let status = if rank(local.status) >= rank(github.status) {
        local.status
    } else {
        github.status
    };
    Check {
        name: "configuration",
        status,
        detail: format!("local: {}; default branch: {}", local.detail, github.detail),
    }
}

fn doctor(
    json_out: bool,
    schema_path: Option<&Path>,
    offline: bool,
    config: Option<&Path>,
    shadow: bool,
    github: &GithubArgs,
) -> ExitCode {
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
            let count = |s: Status| map.features.iter().filter(|f| f.status == s).count();
            let (conformant, partial, missing) = (
                count(Status::Conformant),
                count(Status::Partial),
                count(Status::Missing),
            );
            checks.push(Check {
                name: "support-matrix",
                status: if problems.is_empty() { "OK" } else { "FAIL" },
                detail: if problems.is_empty() {
                    format!(
                        "{} features ({}): {conformant} conformant, {partial} partial (some sub-features missing), {missing} not built; {} inventory entries",
                        map.features.len(),
                        by_prio
                            .iter()
                            .map(|(k, v)| format!("{k} {v}"))
                            .collect::<Vec<_>>()
                            .join(", "),
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
    const GITHUB_CHECKS: [&str; 6] = [
        "github-app",
        "app-permissions",
        "ruleset-exception",
        "configuration",
        "webhooks",
        "rate-limit",
    ];
    let config_check = schlange_github::config::ConfigCheck::locate(schema_path, &cwd).ok();
    let local = local_config_check(config, &cwd, config_check.as_ref());
    if offline || !github.configured() {
        let why = if offline {
            "--offline"
        } else {
            "no GitHub app configured (SCHLANGE_GITHUB_APP_ID, SCHLANGE_GITHUB_PRIVATE_KEY)"
        };
        for name in GITHUB_CHECKS {
            if name == "configuration"
                && let Some(c) = local.clone()
            {
                checks.push(c);
                continue;
            }
            checks.push(Check {
                name,
                status: "SKIP",
                detail: why.into(),
            });
        }
    } else {
        match github.client().and_then(|c| github.repos().map(|r| (c, r))) {
            Ok((client, repos)) => {
                for c in schlange_github::doctor::github_checks_for(
                    &client,
                    &repos,
                    github.installation_id,
                    24 * 3600,
                    config_check.as_ref(),
                    shadow,
                ) {
                    let c = Check {
                        name: c.name,
                        status: c.status,
                        detail: c.detail,
                    };
                    match (&local, c.name) {
                        (Some(l), "configuration") => {
                            checks.push(merge_config_checks(l.clone(), c))
                        }
                        _ => checks.push(c),
                    }
                }
            }
            Err(e) => {
                checks.push(Check {
                    name: "github-app",
                    status: "FAIL",
                    detail: e,
                });
                checks.extend(local);
            }
        }
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

fn default_holder() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "host".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{host}/{}/{nanos:08x}", std::process::id())
}

struct ServeArgs {
    shadow: bool,
    mergify_login: String,
    mergify_app: String,
    lease_ttl: u64,
    lease_holder: Option<String>,
    no_lease: bool,
    github: GithubArgs,
    secret: Option<String>,
    secret_file: Option<PathBuf>,
    listen: std::net::SocketAddr,
    reconcile_every: u64,
    config: Option<PathBuf>,
    check_name: String,
    data_dir: PathBuf,
    schema: Option<PathBuf>,
    admin_token_file: Option<PathBuf>,
}

fn read_secret_file(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p)
        .map(|s| s.trim_end().to_string())
        .map_err(|e| format!("cannot read {}: {e}", p.display()))
}

fn serve(a: ServeArgs) -> ExitCode {
    let fail = |m: String| {
        eprintln!("schlange serve: {m}");
        ExitCode::from(exit::ENVIRONMENT)
    };
    let client = match a.github.client() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let repos = match a.github.repos() {
        Ok(r) if !r.is_empty() => r,
        Ok(_) => return fail("needs at least one --repo owner/name (or SCHLANGE_REPOS)".into()),
        Err(e) => return fail(e),
    };
    if std::env::args().any(|x| x == "--webhook-secret" || x.starts_with("--webhook-secret=")) {
        eprintln!(
            "schlange serve: WARNING: --webhook-secret on the command line is visible to other users (ps); use SCHLANGE_WEBHOOK_SECRET or --webhook-secret-file"
        );
    }
    let secret = match (a.secret, a.secret_file) {
        (Some(s), _) => s.into_bytes(),
        (None, Some(p)) => match read_secret_file(&p) {
            Ok(s) => s.into_bytes(),
            Err(e) => return fail(e),
        },
        (None, None) => {
            return fail(
                "needs a webhook secret (SCHLANGE_WEBHOOK_SECRET or --webhook-secret-file)".into(),
            );
        }
    };
    if secret.len() < schlange_github::service::MIN_SECRET_LEN {
        return fail(format!(
            "the webhook secret is empty or shorter than {} bytes",
            schlange_github::service::MIN_SECRET_LEN
        ));
    }
    let admin_token = match (
        &a.admin_token_file,
        std::env::var("SCHLANGE_ADMIN_TOKEN").ok(),
    ) {
        (Some(p), _) => match read_secret_file(p) {
            Ok(s) => Some(s),
            Err(e) => return fail(e),
        },
        (None, t) => t.filter(|t| !t.is_empty()),
    };
    if admin_token.as_ref().is_some_and(|t| t.len() < 16) {
        return fail("the admin token is shorter than 16 bytes".into());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let check = match schlange_github::config::ConfigCheck::locate(a.schema.as_deref(), &cwd) {
        Ok(c) => std::sync::Arc::new(c),
        Err(e) => return fail(format!("the configuration cannot be validated: {e}")),
    };
    let config = match a.config {
        None => schlange_github::ConfigSource::Repository,
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(t) => schlange_github::ConfigSource::Text(t),
            Err(e) => return fail(format!("cannot read {}: {e}", p.display())),
        },
    };
    let cfg = schlange_github::ServeConfig {
        client,
        repos,
        installation: a.github.installation_id,
        webhook_secret: secret,
        listen: a.listen,
        reconcile_every: a.reconcile_every,
        config,
        poll: std::time::Duration::from_millis(500),
        check_name: a.check_name,
        retry_base: std::time::Duration::from_secs(2),
        retry_max: std::time::Duration::from_secs(60),
        check,
        data_dir: Some(a.data_dir),
        admin_token,
        http: Default::default(),
        crash: None,
        shadow: a.shadow.then(|| schlange_github::shadow::ShadowConfig {
            mergify_login: a.mergify_login.clone(),
            mergify_app: a.mergify_app.clone(),
            panic_on_write: false,
        }),
        lease: (!a.no_lease && !a.shadow).then(|| schlange_github::lease::LeaseConfig {
            holder: a.lease_holder.clone().unwrap_or_else(default_holder),
            ttl: a.lease_ttl.max(10),
        }),
        pause_before_land: None,
    };
    let clock = std::sync::Arc::new(schlange_github::time::SystemClock);
    match schlange_github::Service::start(cfg, clock) {
        Ok(s) => {
            eprintln!("schlange serve: webhooks on {}", s.webhook_url());
            if s.shadow_gate.is_some() {
                eprintln!(
                    "schlange serve: SHADOW MODE: deciding without writing to GitHub; `schlange shadow report` compares with Mergify"
                );
            }
            s.wait();
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// Parses the command line. A usage error exits 64 (`EX_USAGE`), never 2, which means "valid
/// but unsupported"; with `--json` among the arguments the error is JSON on stdout. Help and
/// version print and exit 0.
fn parse_cli() -> Result<Cli, ExitCode> {
    Cli::try_parse().map_err(|e| {
        use clap::error::ErrorKind;
        if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
            let _ = e.print();
            return ExitCode::SUCCESS;
        }
        if std::env::args().any(|a| a == "--json") {
            print_json(&json!({
                "error": e.render().to_string().trim_end(),
                "exit_code": exit::USAGE,
            }));
        } else {
            let _ = e.print();
        }
        ExitCode::from(exit::USAGE)
    })
}

/// `schlange queue …`: calls the queue API of `serve` with the admin token. Exit 0 on
/// success, 1 when serve refused, 3 when it cannot be reached or no token is given.
fn queue_cli(
    command: &QueueCommand,
    url: &str,
    repo: &str,
    token_file: Option<&Path>,
    json_out: bool,
) -> ExitCode {
    let token = match token_file {
        Some(p) => match read_secret_file(p) {
            Ok(t) => t,
            Err(e) => return env_error(json_out, &e),
        },
        None => match std::env::var("SCHLANGE_ADMIN_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                return env_error(
                    json_out,
                    "no admin token: --admin-token-file or SCHLANGE_ADMIN_TOKEN",
                );
            }
        },
    };
    if !repo.contains('/') {
        return env_error(json_out, "--repo owner/name (or SCHLANGE_REPO) is required");
    }
    let base = format!("{}/repos/{repo}", url.trim_end_matches('/'));
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build(),
    );
    let auth = format!("Bearer {token}");
    let resp = match command {
        QueueCommand::Status => agent
            .get(&format!("{base}/merge-queue/status"))
            .header("authorization", &auth)
            .call(),
        QueueCommand::Pause { reason } => agent
            .put(&format!("{base}/merge-queue/pause"))
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .send(json!({ "reason": reason }).to_string()),
        QueueCommand::Unpause => agent
            .delete(&format!("{base}/merge-queue/pause"))
            .header("authorization", &auth)
            .call(),
    };
    let (status, body) = match resp {
        Ok(mut r) => (
            r.status().as_u16(),
            r.body_mut().read_to_string().unwrap_or_default(),
        ),
        Err(e) => return env_error(json_out, &format!("cannot reach {url}: {e}")),
    };
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| json!({ "message": body.trim() }));
    if json_out {
        print_json(&json!({ "status": status, "response": value }));
    } else if status != 200 {
        eprintln!("schlange: serve answered {status}: {}", body.trim());
    } else if matches!(command, QueueCommand::Status) {
        print!("{}", queue_text(&value));
    } else if value["paused"] == true {
        println!("paused: {}", value["reason"].as_str().unwrap_or(""));
    } else {
        println!("not paused");
    }
    if status == 200 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(exit::INVALID)
    }
}

/// The queue status as text: pause, batches with their pull requests, the waiting ones.
fn queue_text(v: &serde_json::Value) -> String {
    let mut out = format!("mode: {}\n", v["mode"].as_str().unwrap_or("serial"));
    if let Some(r) = v["pause"]["reason"].as_str() {
        out.push_str(&format!("PAUSED: {r}\n"));
    }
    let numbers = |list: &serde_json::Value| {
        list.as_array()
            .map(|l| {
                l.iter()
                    .map(|p| {
                        let held = p["held"]
                            .as_str()
                            .map(|h| format!(" ({h})"))
                            .unwrap_or_default();
                        format!(
                            "#{} [{}]{held}",
                            p["number"],
                            p["priority_alias"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    for b in v["batches"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "batch {} on {} ({}): {}\n",
            b["id"].as_str().unwrap_or(""),
            b["base_branch"].as_str().unwrap_or(""),
            b["status"]["code"].as_str().unwrap_or(""),
            numbers(&b["pull_requests"])
        ));
    }
    out.push_str(&format!(
        "waiting: {}\n",
        numbers(&v["waiting_pull_requests"])
    ));
    out
}

/// The admin token from `--admin-token-file` or `SCHLANGE_ADMIN_TOKEN`.
fn admin_token(api: &ApiArgs) -> Result<String, String> {
    match &api.admin_token_file {
        Some(p) => read_secret_file(p),
        None => match std::env::var("SCHLANGE_ADMIN_TOKEN") {
            Ok(t) if !t.is_empty() => Ok(t),
            _ => Err("no admin token: --admin-token-file or SCHLANGE_ADMIN_TOKEN".into()),
        },
    }
}

/// One call of `serve`'s admin API under `/repos/{owner}/{name}/`: the status and the JSON
/// answer (text answers become `{"message": …}`). Environment problems exit 3.
fn api_call(
    api: &ApiArgs,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(u16, serde_json::Value), ExitCode> {
    let token = admin_token(api).map_err(|e| env_error(api.json, &e))?;
    if !api.repo.contains('/') {
        return Err(env_error(
            api.json,
            "--repo owner/name (or SCHLANGE_REPO) is required",
        ));
    }
    let url = format!(
        "{}/repos/{}/{path}",
        api.url.trim_end_matches('/'),
        api.repo
    );
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build(),
    );
    let auth = format!("Bearer {token}");
    let resp = match (method, body) {
        ("POST", Some(b)) => agent
            .post(&url)
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .send(b.to_string()),
        _ => agent.get(&url).header("authorization", &auth).call(),
    };
    let (status, text) = match resp {
        Ok(mut r) => (
            r.status().as_u16(),
            r.body_mut().read_to_string().unwrap_or_default(),
        ),
        Err(e) => {
            return Err(env_error(
                api.json,
                &format!("cannot reach {}: {e}", api.url),
            ));
        }
    };
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "message": text.trim() }));
    Ok((status, value))
}

/// A refusal of `serve` (not 200): printed, exit 1.
fn refused(api: &ApiArgs, status: u16, value: &serde_json::Value) -> ExitCode {
    if api.json {
        print_json(&json!({ "status": status, "response": value }));
    } else {
        eprintln!(
            "schlange: serve answered {status}: {}",
            value["message"].as_str().unwrap_or(&value.to_string())
        );
    }
    ExitCode::from(exit::INVALID)
}

/// The end state of a scenario's run in the simulated forge, with its rules.
fn scenario_end(
    path: &Path,
) -> Result<(schlange_core::State, schlange_config::rules::Rules), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let s = schlange_forge::scenario::parse(&text, path).map_err(|e| e.to_string())?;
    Ok((schlange_forge::runner::end_state(&s), s.rules.clone()))
}

fn explain_cli(pr: u64, scenario: Option<&Path>, api: &ApiArgs) -> ExitCode {
    let value = match scenario {
        Some(path) => match scenario_end(path) {
            Ok((state, rules)) => {
                let ex = schlange_core::explain::explain(&state, &rules, pr);
                let mut v = serde_json::to_value(&ex).unwrap_or_default();
                v["text"] = json!(ex.to_text());
                v
            }
            Err(e) => return env_error(api.json, &e),
        },
        None => match api_call(api, "GET", &format!("pulls/{pr}/explain"), None) {
            Ok((200, v)) => v,
            Ok((status, v)) => return refused(api, status, &v),
            Err(code) => return code,
        },
    };
    if api.json {
        let mut v = value;
        if let Some(o) = v.as_object_mut() {
            o.remove("text");
        }
        print_json(&v);
    } else {
        print!("{}", value["text"].as_str().unwrap_or(""));
    }
    ExitCode::SUCCESS
}

fn open_store(dir: &Path, json_out: bool) -> Result<schlange_github::store::Store, ExitCode> {
    if !dir.join(schlange_github::store::DB_FILE).is_file() {
        return Err(env_error(
            json_out,
            &format!(
                "no {} in {}",
                schlange_github::store::DB_FILE,
                dir.display()
            ),
        ));
    }
    schlange_github::store::Store::open(dir).map_err(|e| env_error(json_out, &e.to_string()))
}

fn parse_since(s: &str) -> Option<u64> {
    s.parse::<u64>()
        .ok()
        .or_else(|| schlange_github::time::parse_rfc3339(s))
}

fn events_cli(
    pr: Option<u64>,
    since: Option<&str>,
    limit: u64,
    data_dir: Option<&Path>,
    api: &ApiArgs,
) -> ExitCode {
    let since = match since.map(|s| (s, parse_since(s))) {
        Some((s, None)) => return env_error(api.json, &format!("--since `{s}` is not a time")),
        Some((_, t)) => t,
        None => None,
    };
    let events: Vec<serde_json::Value> = match data_dir {
        Some(dir) => {
            if !api.repo.contains('/') {
                return env_error(api.json, "--repo owner/name is required");
            }
            let store = match open_store(dir, api.json) {
                Ok(s) => s,
                Err(c) => return c,
            };
            let q = schlange_github::store::EventQuery {
                pr,
                since,
                before: None,
                kind: None,
                limit,
            };
            match store.events(&api.repo, &q) {
                Ok(l) => l.iter().map(schlange_github::events::api_json).collect(),
                Err(e) => return env_error(api.json, &e.to_string()),
            }
        }
        None => {
            let mut path = format!("logs?per_page={}", limit.clamp(1, 100));
            if let Some(n) = pr {
                path.push_str(&format!("&pull_request={n}"));
            }
            if let Some(t) = since {
                path.push_str(&format!("&received_from={t}"));
            }
            match api_call(api, "GET", &path, None) {
                Ok((200, v)) => v["events"].as_array().cloned().unwrap_or_default(),
                Ok((status, v)) => return refused(api, status, &v),
                Err(code) => return code,
            }
        }
    };
    if api.json {
        print_json(&json!({ "events": events }));
    } else {
        for e in &events {
            let prs: Vec<String> = e["pull_requests"]
                .as_array()
                .map(|a| a.iter().map(|n| format!("#{n}")).collect())
                .unwrap_or_default();
            println!(
                "{} {} {} {}",
                e["received_at"].as_str().unwrap_or(""),
                e["type"].as_str().unwrap_or(""),
                if prs.is_empty() {
                    "-".to_string()
                } else {
                    prs.join(",")
                },
                e["metadata"]
            );
        }
    }
    ExitCode::SUCCESS
}

fn audit_cli(pr: Option<u64>, data_dir: Option<&Path>, api: &ApiArgs) -> ExitCode {
    let records: Vec<serde_json::Value> = match data_dir {
        Some(dir) => {
            if !api.repo.contains('/') {
                return env_error(api.json, "--repo owner/name is required");
            }
            let store = match open_store(dir, api.json) {
                Ok(s) => s,
                Err(c) => return c,
            };
            match store.audits(&api.repo) {
                Ok(l) => l
                    .into_iter()
                    .filter(|r| pr.is_none_or(|n| r.number == n))
                    .map(|r| serde_json::to_value(r).unwrap_or_default())
                    .collect(),
                Err(e) => return env_error(api.json, &e.to_string()),
            }
        }
        None => {
            let path = match pr {
                Some(n) => format!("audit?pull_request={n}"),
                None => "audit".into(),
            };
            match api_call(api, "GET", &path, None) {
                Ok((200, v)) => v["records"].as_array().cloned().unwrap_or_default(),
                Ok((status, v)) => return refused(api, status, &v),
                Err(code) => return code,
            }
        }
    };
    if api.json {
        print_json(&json!({ "records": records }));
    } else if records.is_empty() {
        println!("no landings recorded");
    } else {
        for r in &records {
            let short = |k: &str| {
                let s = r[k].as_str().unwrap_or("");
                s[..s.len().min(10)].to_string()
            };
            println!(
                "#{} car {} tested {} tree {} on {} -> landed {} tree {} parent {}{}",
                r["number"],
                r["car"],
                short("tested_head"),
                short("tested_tree"),
                short("base_before"),
                short("landed_sha"),
                short("landed_tree"),
                short("landed_parent"),
                r["note"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            );
        }
    }
    ExitCode::SUCCESS
}

fn shadow_report_cli(data_dir: Option<&Path>, api: &ApiArgs) -> ExitCode {
    let report: schlange_github::shadow::ShadowReport = match data_dir {
        Some(dir) => {
            if !api.repo.contains('/') {
                return env_error(api.json, "--repo owner/name is required");
            }
            let store = match open_store(dir, api.json) {
                Ok(s) => s,
                Err(c) => return c,
            };
            match store.shadow_facts(&api.repo) {
                Ok(f) => schlange_github::shadow::report(&api.repo, &f),
                Err(e) => return env_error(api.json, &e.to_string()),
            }
        }
        None => match api_call(api, "GET", "shadow/report", None) {
            Ok((200, v)) => match serde_json::from_value(v) {
                Ok(r) => r,
                Err(e) => return env_error(api.json, &format!("unexpected answer: {e}")),
            },
            Ok((status, v)) => return refused(api, status, &v),
            Err(code) => return code,
        },
    };
    if api.json {
        print_json(&report);
    } else {
        print!("{}", report.to_text());
    }
    if report.disagreements == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(exit::INVALID)
    }
}

fn simulate_cli(
    file: &Path,
    pr: u64,
    scenario: Option<&Path>,
    schema_path: Option<&Path>,
    api: &ApiArgs,
) -> ExitCode {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => return env_error(api.json, &format!("cannot read {}: {e}", file.display())),
    };
    let value = match scenario {
        Some(path) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let check = match schlange_github::config::ConfigCheck::locate(schema_path, &cwd) {
                Ok(c) => c,
                Err(e) => return env_error(api.json, &e),
            };
            let (state, _) = match scenario_end(path) {
                Ok(s) => s,
                Err(e) => return env_error(api.json, &e),
            };
            match schlange_github::service::simulate(&state, &check, &text, pr) {
                Ok(v) => v,
                Err(e) => return refused(api, 422, &json!({ "message": e })),
            }
        }
        None => match api_call(
            api,
            "POST",
            &format!("pulls/{pr}/simulator"),
            Some(json!({ "mergify_yml": text })),
        ) {
            Ok((200, v)) => v,
            Ok((status, v)) => return refused(api, status, &v),
            Err(code) => return code,
        },
    };
    if api.json {
        print_json(&value);
    } else {
        println!("{}", value["title"].as_str().unwrap_or(""));
        println!();
        print!("{}", value["summary"].as_str().unwrap_or(""));
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(code) => return code,
    };
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
        Command::Config {
            command:
                ConfigCommand::Simulate {
                    file,
                    pr,
                    scenario,
                    schema,
                    api,
                },
        } => simulate_cli(&file, pr, scenario.as_deref(), schema.as_deref(), &api),
        Command::Sim {
            command: SimCommand::Run { file, json, forge },
        } => sim_run(&file, json, forge),
        Command::Explain { pr, scenario, api } => explain_cli(pr, scenario.as_deref(), &api),
        Command::Events {
            pr,
            since,
            limit,
            data_dir,
            api,
        } => events_cli(pr, since.as_deref(), limit, data_dir.as_deref(), &api),
        Command::Audit { pr, data_dir, api } => audit_cli(pr, data_dir.as_deref(), &api),
        Command::Shadow {
            command: ShadowCommand::Report { data_dir, api },
        } => shadow_report_cli(data_dir.as_deref(), &api),
        Command::Queue {
            command,
            url,
            repo,
            admin_token_file,
            json,
        } => queue_cli(&command, &url, &repo, admin_token_file.as_deref(), json),
        Command::SupportMatrix { json } => support_matrix(json),
        Command::Doctor {
            json,
            schema,
            offline,
            config,
            shadow,
            github,
        } => doctor(
            json,
            schema.as_deref(),
            offline,
            config.as_deref(),
            shadow,
            &github,
        ),
        Command::Serve {
            github,
            webhook_secret,
            webhook_secret_file,
            listen,
            reconcile_every,
            config,
            check_name,
            data_dir,
            schema,
            admin_token_file,
            shadow,
            mergify_login,
            mergify_app,
            lease_ttl,
            lease_holder,
            no_lease,
        } => serve(ServeArgs {
            shadow,
            mergify_login,
            mergify_app,
            lease_ttl,
            lease_holder,
            no_lease,
            github,
            secret: webhook_secret,
            secret_file: webhook_secret_file,
            listen,
            reconcile_every,
            config,
            check_name,
            data_dir,
            schema,
            admin_token_file,
        }),
    }
}
