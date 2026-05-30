//! Golden-parity characterization corpus for import operations — the Stream M
//! migration safety net (plan: imports-refactor-expansion-v2.md, council finding #1).
//!
//! Captures the CURRENT add/remove/organize output for the existing import
//! languages (TS/JS/TSX, Python, Rust, Go) byte-for-byte, BEFORE the
//! `ImportStatement -> ImportForm` / `ImportSyntax` refactor. The refactor must
//! keep every golden here unchanged — that is the proof the type redesign did
//! not silently regress a working language (e.g. Rust's `pub`-in-`default_import`
//! organize merge, or Go's alias-in-`default_import`).
//!
//! Workflow:
//!   - First capture / intentional update: `UPDATE_GOLDEN=1 cargo test -p \
//!     agent-file-tools --test integration import_golden_corpus`
//!   - Normal run / CI gate: asserts each scenario's final file content matches
//!     its committed `.golden` fixture exactly.
//!
//! Each scenario applies a deterministic op sequence to a small input file and
//! snapshots the resulting file content. One behavior per scenario keeps drift
//! diagnosis precise.

use super::helpers::{cargo_manifest_dir, AftProcess};
use std::fs;
use std::path::PathBuf;

/// A single import operation in a scenario's sequence.
enum Op {
    Add {
        module: &'static str,
        names: &'static [&'static str],
        default_import: Option<&'static str>,
        type_only: bool,
    },
    Remove {
        module: &'static str,
        /// `Some(name)` removes one named import; `None` removes the whole statement.
        name: Option<&'static str>,
    },
    Organize,
}

struct Scenario {
    /// Golden key — also the `.golden` fixture filename stem.
    name: &'static str,
    /// File extension that drives language detection (`ts`, `js`, `py`, `rs`, `go`).
    ext: &'static str,
    /// Initial source written to the input file.
    input: &'static str,
    /// Ops applied in order; the final file content is the captured golden.
    ops: &'static [Op],
}

fn golden_dir() -> PathBuf {
    cargo_manifest_dir().join("tests/integration/fixtures/import_golden")
}

/// Apply a scenario's op sequence to a fresh temp file and return the final content.
fn run_scenario(aft: &mut AftProcess, scenario: &Scenario) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join(format!("input.{}", scenario.ext));
    fs::write(&file, scenario.input).expect("write input");
    let file_str = file.display().to_string();

    for (idx, op) in scenario.ops.iter().enumerate() {
        let mut params = match op {
            Op::Add {
                module,
                names,
                default_import,
                type_only,
            } => {
                let mut p = serde_json::json!({
                    "id": format!("{}-{}", scenario.name, idx),
                    "command": "add_import",
                    "file": file_str,
                    "module": module,
                });
                if !names.is_empty() {
                    p["names"] = serde_json::json!(names);
                }
                if let Some(def) = default_import {
                    p["default_import"] = serde_json::json!(def);
                }
                if *type_only {
                    p["type_only"] = serde_json::json!(true);
                }
                p
            }
            Op::Remove { module, name } => {
                let mut p = serde_json::json!({
                    "id": format!("{}-{}", scenario.name, idx),
                    "command": "remove_import",
                    "file": file_str,
                    "module": module,
                });
                if let Some(n) = name {
                    p["name"] = serde_json::json!(n);
                }
                p
            }
            Op::Organize => serde_json::json!({
                "id": format!("{}-{}", scenario.name, idx),
                "command": "organize_imports",
                "file": file_str,
            }),
        };
        params["validate"] = serde_json::json!("syntax");

        let resp = aft.send(&serde_json::to_string(&params).unwrap());
        // A hard error means the scenario itself is malformed — fail loudly so we
        // never bake a broken op into a golden. No-op successes (removed:false,
        // duplicate add) are legitimate and captured via the final content.
        assert_ne!(
            resp["success"], false,
            "scenario '{}' op {} returned an error: {resp:?}",
            scenario.name, idx
        );
    }

    fs::read_to_string(&file).expect("read final content")
}

fn scenarios() -> Vec<Scenario> {
    vec![
        // ---- TypeScript (ES engine) ----
        Scenario {
            name: "ts_add_named_merges_into_existing",
            ext: "ts",
            input: "import { useState } from \"react\";\n\nexport const x = 1;\n",
            ops: &[Op::Add {
                module: "react",
                names: &["useEffect"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_add_external_sorts_vs_internal",
            ext: "ts",
            input: "import { local } from \"./local\";\n\nexport const x = 1;\n",
            ops: &[Op::Add {
                module: "lodash",
                names: &["debounce"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_add_type_only",
            ext: "ts",
            input: "export const x = 1;\n",
            ops: &[Op::Add {
                module: "./types",
                names: &["Foo"],
                default_import: None,
                type_only: true,
            }],
        },
        Scenario {
            name: "ts_add_default",
            ext: "ts",
            input: "export const x = 1;\n",
            ops: &[Op::Add {
                module: "react",
                names: &[],
                default_import: Some("React"),
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_add_named_with_alias",
            ext: "ts",
            input: "export const x = 1;\n",
            ops: &[Op::Add {
                module: "./util",
                names: &["original as renamed"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "ts_remove_one_named",
            ext: "ts",
            input: "import { a, b, c } from \"mod\";\n\nexport const x = 1;\n",
            ops: &[Op::Remove {
                module: "mod",
                name: Some("b"),
            }],
        },
        Scenario {
            name: "ts_remove_entire",
            ext: "ts",
            input: "import { a } from \"mod\";\nimport { keep } from \"other\";\n\nexport const x = 1;\n",
            ops: &[Op::Remove {
                module: "mod",
                name: None,
            }],
        },
        Scenario {
            name: "ts_organize_mixed",
            ext: "ts",
            input: "import { z } from \"./z\";\nimport { a } from \"axios\";\nimport \"./side-effect\";\nimport { b } from \"./b\";\nimport { y } from \"zod\";\n\nexport const x = 1;\n",
            ops: &[Op::Organize],
        },
        // ---- JavaScript (shares the ES engine — proves the JS dispatch path) ----
        Scenario {
            name: "js_add_named",
            ext: "js",
            input: "import { existing } from \"pkg\";\n\nexport const x = 1;\n",
            ops: &[Op::Add {
                module: "pkg",
                names: &["added"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "js_organize_mixed",
            ext: "js",
            input: "import { b } from \"./b\";\nimport { a } from \"alpha\";\n\nexport const x = 1;\n",
            ops: &[Op::Organize],
        },
        // ---- Python ----
        Scenario {
            name: "py_add_from_import",
            ext: "py",
            input: "x = 1\n",
            ops: &[Op::Add {
                module: "os",
                names: &["path"],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "py_add_module_import",
            ext: "py",
            input: "x = 1\n",
            ops: &[Op::Add {
                module: "sys",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "py_remove_name",
            ext: "py",
            input: "from os import path, sep\n\nx = 1\n",
            ops: &[Op::Remove {
                module: "os",
                name: Some("sep"),
            }],
        },
        Scenario {
            name: "py_organize_grouped",
            ext: "py",
            input: "from . import local\nimport requests\nimport os\n\nx = 1\n",
            ops: &[Op::Organize],
        },
        // ---- Rust (the gnarliest: pub-in-default_import + use-tree merge) ----
        Scenario {
            name: "rs_add_use",
            ext: "rs",
            input: "fn main() {}\n",
            ops: &[Op::Add {
                module: "std::collections::HashMap",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "rs_organize_merges_use_tree",
            ext: "rs",
            input: "use std::collections::HashMap;\nuse std::collections::BTreeMap;\n\nfn main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "rs_organize_pub_use_preserved",
            ext: "rs",
            input: "pub use crate::a::Exported;\nuse crate::a::Internal;\n\nfn main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "rs_remove_use",
            ext: "rs",
            input: "use std::collections::HashMap;\nuse std::fmt::Debug;\n\nfn main() {}\n",
            ops: &[Op::Remove {
                module: "std::fmt::Debug",
                name: None,
            }],
        },
        // ---- Go (alias-in-default_import + grouped block) ----
        Scenario {
            name: "go_add_path",
            ext: "go",
            input: "package main\n\nfunc main() {}\n",
            ops: &[Op::Add {
                module: "fmt",
                names: &[],
                default_import: None,
                type_only: false,
            }],
        },
        Scenario {
            name: "go_organize_grouped",
            ext: "go",
            input: "package main\n\nimport (\n\t\"github.com/x/y\"\n\t\"fmt\"\n)\n\nfunc main() {}\n",
            ops: &[Op::Organize],
        },
        Scenario {
            name: "go_remove_path",
            ext: "go",
            input: "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() {}\n",
            ops: &[Op::Remove {
                module: "os",
                name: None,
            }],
        },
    ]
}

/// Golden-parity gate for the import engines. See module docs.
#[test]
fn import_golden_corpus() {
    let scenarios = scenarios();
    let update = std::env::var("UPDATE_GOLDEN").is_ok();
    let dir = golden_dir();
    if update {
        fs::create_dir_all(&dir).expect("create golden dir");
    }

    let mut aft = AftProcess::spawn();
    let mut drift: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for scenario in &scenarios {
        let actual = run_scenario(&mut aft, scenario);
        let golden_path = dir.join(format!("{}.golden", scenario.name));

        if update {
            fs::write(&golden_path, &actual).expect("write golden");
            continue;
        }

        match fs::read_to_string(&golden_path) {
            Ok(expected) if expected == actual => {}
            Ok(expected) => drift.push(format!(
                "\n=== DRIFT: {} ===\n--- expected (golden) ---\n{}\n--- actual ---\n{}",
                scenario.name, expected, actual
            )),
            Err(_) => missing.push(scenario.name.to_string()),
        }
    }

    if update {
        return;
    }

    assert!(
        missing.is_empty(),
        "missing golden fixtures (run with UPDATE_GOLDEN=1 to capture): {missing:?}"
    );
    assert!(
        drift.is_empty(),
        "import golden parity drift in {} scenario(s):{}",
        drift.len(),
        drift.join("")
    );
}
