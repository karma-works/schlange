//! Builds the feature inventory (`_inventory.json`) from the pinned Mergify sources.
//!
//! The inventory holds identifiers only (configuration paths, attribute names, action names,
//! enum values, API operations, command and CLI names) plus types and default values. It never
//! copies Mergify's prose, so it can be checked in (clean room, see AGENTS.md).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const DEFAULT_OUT: &str = ".agents/skills/verify-schlange/features/_inventory.json";

#[derive(Parser)]
#[command(about = "Builds features/_inventory.json from the pinned Mergify sources")]
struct Args {
    /// Directory with the files of tools/fetch-mergify-sources.
    #[arg(long, default_value = ".research/mergify")]
    sources: PathBuf,
    /// Output file.
    #[arg(long, default_value = DEFAULT_OUT)]
    out: PathBuf,
    /// Do not write; exit 1 when the checked-in file differs from a fresh build.
    #[arg(long)]
    check: bool,
}

#[derive(Serialize, Clone, Debug)]
struct Entry {
    id: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operators: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    deprecated: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    experimental: bool,
}

impl Entry {
    fn new(kind: &'static str, id: String) -> Self {
        Entry {
            id: format!("{kind}:{id}"),
            kind,
            r#type: None,
            value: None,
            operators: None,
            tag: None,
            deprecated: false,
            experimental: false,
        }
    }
}

/// Definitions that describe conditions. Their inner structure is the condition grammar,
/// not configuration paths; attributes come from `PullRequestAttributes` instead.
const CONDITION_DEFS: &[&str] = &[
    "RuleConditionModel",
    "AndCondition",
    "OrCondition",
    "NegationCondition",
    "ListOfRuleConditions",
    "PullRequestAttributes",
];

struct Schema<'a> {
    defs: &'a Map<String, Value>,
}

impl<'a> Schema<'a> {
    fn resolve(&self, node: &'a Value) -> (Option<&'a str>, &'a Value) {
        if let Some(r) = node.get("$ref").and_then(Value::as_str) {
            let name = r.rsplit('/').next().unwrap_or(r);
            if let Some(def) = self.defs.get(name) {
                return (Some(name), def);
            }
        }
        (None, node)
    }

    /// Short type of a schema node: `string`, `integer?` (nullable), `array<string>`,
    /// `conditions`, `object`, or a union `boolean|object`.
    fn type_of(&self, node: &'a Value, depth: usize) -> String {
        let (name, n) = self.resolve(node);
        if let Some(name) = name {
            if name.ends_with("ConditionsModel") || CONDITION_DEFS.contains(&name) {
                return "conditions".into();
            }
        }
        if depth > 6 {
            return "object".into();
        }
        let mut types: BTreeSet<String> = BTreeSet::new();
        let mut nullable = false;
        match n.get("type") {
            Some(Value::String(t)) if t == "array" => {
                let item = n
                    .get("items")
                    .map(|i| self.type_of(i, depth + 1))
                    .unwrap_or_else(|| "any".into());
                types.insert(format!("array<{item}>"));
            }
            Some(Value::String(t)) if t == "null" => nullable = true,
            Some(Value::String(t)) => {
                types.insert(t.clone());
            }
            Some(Value::Array(ts)) => {
                for t in ts.iter().filter_map(Value::as_str) {
                    if t == "null" {
                        nullable = true;
                    } else {
                        types.insert(t.to_string());
                    }
                }
            }
            _ => {}
        }
        if n.get("enum").is_some() && types.is_empty() {
            types.insert("enum".into());
        }
        if n.get("const").is_some() && types.is_empty() {
            types.insert("const".into());
        }
        for key in ["anyOf", "oneOf"] {
            for sub in n.get(key).and_then(Value::as_array).into_iter().flatten() {
                let t = self.type_of(sub, depth + 1);
                if t == "null" {
                    nullable = true;
                } else if let Some(stripped) = t.strip_suffix('?') {
                    nullable = true;
                    types.extend(stripped.split('|').map(str::to_string));
                } else {
                    types.extend(t.split('|').map(str::to_string));
                }
            }
        }
        if types.is_empty() {
            if nullable {
                return "null".into();
            }
            if n.get("properties").is_some() {
                types.insert("object".into());
            } else {
                types.insert("any".into());
            }
        }
        let joined = types.into_iter().collect::<Vec<_>>().join("|");
        if nullable { format!("{joined}?") } else { joined }
    }

    /// Walks configuration paths. Arrays are transparent (`queue_rules.name`), maps with a
    /// value schema add `*`.
    fn walk(
        &self,
        node: &'a Value,
        path: &str,
        stack: &mut Vec<&'a str>,
        out: &mut BTreeMap<String, Entry>,
    ) {
        let (name, n) = self.resolve(node);
        if let Some(name) = name {
            if CONDITION_DEFS.contains(&name) || stack.contains(&name) {
                return;
            }
            stack.push(name);
        }
        for key in ["anyOf", "oneOf", "allOf"] {
            for sub in n.get(key).and_then(Value::as_array).into_iter().flatten() {
                self.walk(sub, path, stack, out);
            }
        }
        if let Some(items) = n.get("items").filter(|i| i.is_object()) {
            self.walk(items, path, stack, out);
        }
        if let Some(props) = n.get("properties").and_then(Value::as_object) {
            for (key, prop) in props {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let mut e = Entry::new("config", child.clone());
                e.r#type = Some(self.type_of(prop, 0));
                e.deprecated = prop.get("deprecated").and_then(Value::as_bool) == Some(true)
                    || prop.get("configuration_deprecated_attribute").is_some();
                e.experimental =
                    prop.get("x-mergify-experimental").and_then(Value::as_bool) == Some(true);
                out.entry(e.id.clone()).or_insert(e);
                if let Some(default) = prop.get("default").filter(|d| !d.is_null()) {
                    let mut d = Entry::new("default", child.clone());
                    d.value = Some(clean_room_value(default));
                    out.entry(d.id.clone()).or_insert(d);
                }
                self.walk(prop, &child, stack, out);
            }
        }
        if let Some(ap) = n.get("additionalProperties").filter(|a| a.is_object()) {
            let child = format!("{path}.*");
            self.walk(ap, &child, stack, out);
        }
        if name.is_some() {
            stack.pop();
        }
    }
}

/// Keeps a default value unless it is prose (a string of four or more words, such as a
/// comment template). Prose is Mergify's text; the inventory records only its hash.
fn clean_room_value(value: &Value) -> Value {
    match value {
        Value::String(s) if s.split_whitespace().count() >= 4 => {
            json!({ "prose_omitted": true, "sha256": sha256(s.as_bytes()) })
        }
        other => other.clone(),
    }
}

fn enum_values(node: &Value) -> Option<Vec<String>> {
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        return Some(
            values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        );
    }
    for key in ["anyOf", "oneOf"] {
        for sub in node.get(key).and_then(Value::as_array).into_iter().flatten() {
            if let Some(v) = enum_values(sub) {
                return Some(v);
            }
        }
    }
    node.get("items").and_then(enum_values)
}

fn enums(defs: &Map<String, Value>, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut add = |name: &str, values: Vec<String>| -> Result<()> {
        if let Some(prev) = seen.get(name) {
            if *prev != values {
                bail!("enum {name} appears with different values; adjust the naming scheme");
            }
            return Ok(());
        }
        for v in &values {
            let e = Entry::new("enum", format!("{name}:{v}"));
            out.insert(e.id.clone(), e);
        }
        seen.insert(name.to_string(), values);
        Ok(())
    };
    for (def_name, def) in defs {
        if let Some(values) = def.get("enum").and_then(Value::as_array) {
            let values = values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            add(def_name, values)?;
        }
        for (prop, node) in def
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if let Some(values) = enum_values(node) {
                add(prop, values)?;
            }
        }
    }
    Ok(())
}

fn attributes(defs: &Map<String, Value>, schema: &Schema, out: &mut BTreeMap<String, Entry>) {
    let props = defs
        .get("PullRequestAttributes")
        .and_then(|d| d.get("properties"))
        .and_then(Value::as_object);
    for (name, node) in props.into_iter().flatten() {
        let mut e = Entry::new("attr", name.clone());
        let mut t = schema.type_of(node, 0);
        if let Some(f) = node.get("format").and_then(Value::as_str) {
            t = format!("{t}({f})");
        }
        if let Some(r) = node.get("$ref").and_then(Value::as_str) {
            t = format!("enum<{}>", r.rsplit('/').next().unwrap_or(r));
        }
        e.r#type = Some(t);
        e.operators = node.get("x-allowed-operators").cloned();
        out.insert(e.id.clone(), e);
    }
}

fn actions(defs: &Map<String, Value>, out: &mut BTreeMap<String, Entry>) {
    let props = defs
        .get("ActionsModel")
        .and_then(|d| d.get("properties"))
        .and_then(Value::as_object);
    for (name, node) in props.into_iter().flatten() {
        let mut e = Entry::new("action", name.clone());
        e.deprecated = node.get("deprecated").and_then(Value::as_bool) == Some(true)
            || node.get("configuration_deprecated_attribute").is_some();
        out.insert(e.id.clone(), e);
    }
}

fn api(openapi: &Value, out: &mut BTreeMap<String, Entry>) {
    let paths = openapi.get("paths").and_then(Value::as_object);
    for (path, ops) in paths.into_iter().flatten() {
        for method in ["get", "post", "put", "patch", "delete"] {
            if let Some(op) = ops.get(method) {
                let mut e = Entry::new("api", format!("{} {path}", method.to_uppercase()));
                e.tag = op
                    .get("tags")
                    .and_then(Value::as_array)
                    .and_then(|t| t.first())
                    .and_then(Value::as_str)
                    .map(str::to_string);
                e.deprecated = op.get("deprecated").and_then(Value::as_bool) == Some(true);
                out.insert(e.id.clone(), e);
            }
        }
    }
}

/// Commands: the link targets of the command list in `commands.md`.
fn commands(md: &str, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    let mut in_list = false;
    let mut found = 0;
    for line in md.lines() {
        if line.starts_with("## ") {
            in_list = line.trim() == "## Command List";
            continue;
        }
        if !in_list {
            continue;
        }
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("- [") {
            if let Some(open) = rest.find("](") {
                let after = &rest[open + 2..];
                if let Some(close) = after.find(')') {
                    let slug = after[..close].trim_start_matches("./").trim_matches('/');
                    let slug = slug.rsplit('/').next().unwrap_or(slug);
                    let e = Entry::new("cmd", slug.to_string());
                    out.insert(e.id.clone(), e);
                    found += 1;
                }
            }
        }
    }
    if found == 0 {
        bail!("commands.md: no command list found");
    }
    Ok(())
}

/// CLI commands: every heading marked as a command, read from the synopsis in the following
/// `<code>` element (`mergify queue status [OPTIONS]` → `mergify queue status`).
fn cli(html: &str, out: &mut BTreeMap<String, Entry>) {
    let marker = "class=\"cli-command-heading\"";
    let mut rest = html;
    while let Some(pos) = rest.find(marker) {
        rest = &rest[pos + marker.len()..];
        let Some(code) = rest.find("<code>mergify ") else {
            break;
        };
        let synopsis = &rest[code + "<code>".len()..];
        let end = synopsis.find("</code>").unwrap_or(synopsis.len());
        let words: Vec<&str> = synopsis[..end]
            .split_whitespace()
            .take_while(|w| {
                w.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase())
            })
            .collect();
        if words.len() >= 2 {
            let e = Entry::new("cli", words.join(" "));
            out.insert(e.id.clone(), e);
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn build(sources: &Path) -> Result<Value> {
    let read = |name: &str| -> Result<Vec<u8>> {
        fs::read(sources.join(name)).with_context(|| {
            format!(
                "{} is missing; run tools/fetch-mergify-sources first",
                sources.join(name).display()
            )
        })
    };
    let mut hashes = BTreeMap::new();
    let schema_bytes = read("schema.json")?;
    hashes.insert("schema.json".to_string(), sha256(&schema_bytes));
    let schema: Value = serde_json::from_slice(&schema_bytes)?;
    let openapi_bytes = read("openapi.json")?;
    hashes.insert("openapi.json".to_string(), sha256(&openapi_bytes));
    let openapi: Value = serde_json::from_slice(&openapi_bytes)?;

    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .context("schema.json has no $defs")?;
    let s = Schema { defs };
    let mut out = BTreeMap::new();
    s.walk(&schema, "", &mut Vec::new(), &mut out);
    attributes(defs, &s, &mut out);
    actions(defs, &mut out);
    enums(defs, &mut out)?;
    api(&openapi, &mut out);

    let commands_md = read("commands.md")?;
    hashes.insert("commands.md".to_string(), sha256(&commands_md));
    commands(&String::from_utf8_lossy(&commands_md), &mut out)?;

    let mut cli_files: Vec<_> = fs::read_dir(sources)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("cli") && n.ends_with(".html"))
        .collect();
    cli_files.sort();
    for name in cli_files {
        let bytes = read(&name)?;
        hashes.insert(name, sha256(&bytes));
        cli(&String::from_utf8_lossy(&bytes), &mut out);
    }

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for e in out.values() {
        *counts.entry(e.kind).or_default() += 1;
    }
    let total = out.len();
    let entries: Vec<&Entry> = out.values().collect();
    Ok(json!({
        "generated_by": "cargo run -p inventory",
        "sources": hashes,
        "counts": counts,
        "total": total,
        "entries": entries,
    }))
}

fn main() -> Result<ExitCode> {
    let args = Args::parse();
    let inventory = build(&args.sources)?;
    let text = serde_json::to_string_pretty(&inventory)? + "\n";
    if args.check {
        let current = fs::read_to_string(&args.out).unwrap_or_default();
        if current != text {
            eprintln!(
                "{} is out of date: cargo run -p inventory",
                args.out.display()
            );
            return Ok(ExitCode::FAILURE);
        }
        println!("{} is up to date", args.out.display());
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.out, &text)?;
    println!(
        "{}: {} entries {}",
        args.out.display(),
        inventory["total"],
        inventory["counts"]
    );
    Ok(ExitCode::SUCCESS)
}
