//! `schlange`: one static binary for the merge queue: configuration validation, scenarios in
//! the simulated forge, the support matrix, the doctor, and `serve` (the GitHub App).
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

fn doctor(
    json_out: bool,
    schema_path: Option<&Path>,
    offline: bool,
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
    if offline || !github.configured() {
        let why = if offline {
            "--offline"
        } else {
            "no GitHub app configured (SCHLANGE_GITHUB_APP_ID, SCHLANGE_GITHUB_PRIVATE_KEY)"
        };
        for name in GITHUB_CHECKS {
            checks.push(Check {
                name,
                status: "SKIP",
                detail: why.into(),
            });
        }
    } else {
        match github.client().and_then(|c| github.repos().map(|r| (c, r))) {
            Ok((client, repos)) => {
                for c in schlange_github::doctor::github_checks(
                    &client,
                    &repos,
                    github.installation_id,
                    24 * 3600,
                    config_check.as_ref(),
                ) {
                    checks.push(Check {
                        name: c.name,
                        status: c.status,
                        detail: c.detail,
                    });
                }
            }
            Err(e) => checks.push(Check {
                name: "github-app",
                status: "FAIL",
                detail: e,
            }),
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

struct ServeArgs {
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
    };
    let clock = std::sync::Arc::new(schlange_github::time::SystemClock);
    match schlange_github::Service::start(cfg, clock) {
        Ok(s) => {
            eprintln!("schlange serve: webhooks on {}", s.webhook_url());
            s.wait();
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
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
        Command::Doctor {
            json,
            schema,
            offline,
            github,
        } => doctor(json, schema.as_deref(), offline, &github),
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
        } => serve(ServeArgs {
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
