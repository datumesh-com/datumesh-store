//! Store 0.5.2: every new `Store` method has a default implementation, so an `impl Store`
//! that delegates only the 0.5.1 methods still compiles (CONTRACT-0.4.9, "Store 0.5.2").
//!
//! `Store` is sealed (since 0.3.0), so such a `struct W(SqliteStore)` can never be a working
//! implementation outside this crate: the only error it may give is `E0277` for the private
//! supertrait. Like `sealed.rs`, this builds the doc example's delegating wrapper (over a
//! `SqliteStore`, named `W`) as a crate of its own against this one and checks that the seal
//! is the ONLY error. A new method without a default would add `E0046` (missing trait items)
//! here. The native-versus-default equivalence (S2, S3, S6, S7 `has_key_value` against the
//! 0.5.1 fallbacks) cannot be run through an outside wrapper; it is covered inside the crate
//! by the 0.5.1 methods being the fallbacks' only inputs.

use std::path::{Path, PathBuf};
use std::process::Command;

fn doc_example() -> String {
    let lib = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    let mut out = String::new();
    let mut inside = false;
    for line in lib.lines() {
        let l = line.trim_start();
        if l == "/// ```compile_fail" {
            inside = true;
        } else if inside && l == "/// ```" {
            return out;
        } else if inside {
            out.push_str(l.strip_prefix("/// ").or_else(|| l.strip_prefix("///")).unwrap());
            out.push('\n');
        }
    }
    panic!("no compile_fail example in src/lib.rs");
}

fn check(name: &str, source: &str) -> (bool, String) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [dependencies]\ndatumesh-store = {{ path = {:?} }}\ndatumesh-core = \"0.3\"\n\n[workspace]\n",
            manifest.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::copy(manifest.join("../../Cargo.lock"), dir.join("Cargo.lock")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), source).unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["check", "--offline", "--quiet", "--message-format=short", "--color=never"])
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sealed-target"))
        .env_remove("RUSTFLAGS")
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn a_wrapper_delegating_only_the_0_5_1_methods_is_refused_only_by_the_seal() {
    let example = doc_example().replace("PgStore", "SqliteStore").replace("Wrapper", "W");
    assert!(example.contains("impl Store for W"), "{example}");
    // The example must not mention any 0.5.2 method: it delegates the 0.5.1 ones only.
    for m in [
        "name_function_tests",
        "unnamed_function_tests",
        "function_test_summary",
        "function_tests_page",
        "stored_receipts_of",
        "slice_values_limit",
        "versions_limit",
        "stored_cell_count",
        "has_key_value",
        "key_value_count",
        "mark_key_open",
        "key_is_open",
        "catalog_entry",
    ] {
        assert!(!example.contains(m), "the wrapper delegates {m}");
    }
    let (ok, stderr) = check("sealed_store_052_defaults", &format!("#![allow(dead_code)]\n{example}"));
    assert!(!ok, "an outside implementation of Store compiled:\n{stderr}");
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains(": error")).collect();
    assert!(!errors.is_empty(), "{stderr}");
    for e in &errors {
        assert!(e.contains("error[E0277]") && e.contains("Sealed"), "not only the seal (a method without a default?): {e}\n{stderr}");
        assert!(!e.contains("E0046"), "a 0.5.2 method has no default: {e}");
    }
}
