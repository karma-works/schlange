use std::fs;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let schema: serde_json::Value = serde_json::from_str(&fs::read_to_string(&a[1]).unwrap()).unwrap();
    let cfg: serde_json::Value = serde_yaml_ng::from_str(&fs::read_to_string(&a[2]).unwrap()).unwrap();
    let v = jsonschema::validator_for(&schema).unwrap();
    let errs: Vec<_> = v.iter_errors(&cfg).collect();
    println!("schema errors: {}", errs.len());
    for e in errs.iter().take(5) { println!("  {} at {}", e, e.instance_path()); }
    // compile every regex condition
    let mut n = 0;
    fn walk(x: &serde_json::Value, n: &mut usize) {
        match x {
            serde_json::Value::String(s) if s.contains("~=") => {
                let re = s.splitn(2, "~=").nth(1).unwrap().trim();
                match fancy_regex::Regex::new(re) { Ok(r) => { *n += 1;
                    for p in [".github/workflows/ci.yml", "src/main.rs", ".agents/skills/verify-arztagent/SKILL.md"] {
                        println!("  {:45} -> {}", p, r.is_match(p).unwrap()); } }
                    Err(e) => println!("REGEX ERROR {e}") }
            }
            serde_json::Value::Array(v) => v.iter().for_each(|y| walk(y, n)),
            serde_json::Value::Object(m) => m.values().for_each(|y| walk(y, n)),
            _ => {}
        }
    }
    walk(&cfg, &mut n);
    println!("regexes compiled: {n}");
}
