//! Embeds the feature map (frontmatter files and inventory) so that the support matrix works
//! without the repository, e.g. in the container.

use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.agents/skills/verify-schlange/features");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| (n.ends_with(".md") && n != "README.md") || n == "_inventory.json")
        .collect();
    names.sort();
    let mut bundle = String::new();
    for name in names {
        let path = dir.join(&name);
        println!("cargo:rerun-if-changed={}", path.display());
        let text = std::fs::read_to_string(&path).expect("read feature file");
        // Record separator (U+001E) never appears in the files.
        write!(bundle, "\u{1e}FILE {name}\n{text}").expect("write");
    }
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("feature_bundle.txt");
    std::fs::write(out, bundle).expect("write bundle");
}
