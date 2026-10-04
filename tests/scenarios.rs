//! Runs every `tests/scenarios/*.txt` script against each book implementation.

use std::fs;
use std::path::PathBuf;

use lob::scenario::{first_difference, transcript};
use lob::RefBook;

fn scenario_files() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no scenarios in {}", dir.display());
    files
}

#[test]
fn reference_book_passes_every_scenario() {
    let mut failures = Vec::new();
    for path in scenario_files() {
        let script = fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        match transcript::<RefBook>(&script) {
            Ok(actual) => {
                if let Some(diff) = first_difference(&script, &actual) {
                    failures.push(format!("{name}: {diff}"));
                }
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
