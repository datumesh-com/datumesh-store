//! `Store` is sealed: code outside this crate cannot implement it. The crate's documentation
//! of `Store` shows a complete implementation that delegates every method to a `PgStore`
//! (a `compile_fail` example; stable rustdoc does not check its error code). Here that
//! example is built as a crate of its own against this one (`cargo check --offline`, the
//! workspace's lock file): it must fail, and only with `E0277` for the private supertrait.
//! The same crate without the `impl Store` block builds, and uses `Store` as a trait object.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The example in `Store`'s documentation.
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

/// `cargo check` of a crate whose lib.rs is `source`, depending on this crate; returns
/// (success, stderr).
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
        .args(["check", "--offline", "--quiet", "--message-format=short"])
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sealed-target"))
        .env_remove("RUSTFLAGS")
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn an_implementation_outside_the_crate_is_refused_only_by_the_seal() {
    let example = doc_example();
    assert!(example.contains("impl Store for Wrapper"), "{example}");
    let (ok, stderr) = check("sealed_store_impl", &format!("#![allow(dead_code)]\n{example}"));
    assert!(!ok, "an outside implementation of Store compiled:\n{stderr}");
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains(": error")).collect();
    assert!(!errors.is_empty(), "{stderr}");
    for e in &errors {
        assert!(e.contains("error[E0277]") && e.contains("Sealed"), "not the seal: {e}\n{stderr}");
    }
}

#[test]
fn without_the_impl_the_same_crate_builds_and_store_is_object_safe() {
    let example = doc_example();
    let without = &example[..example.find("impl Store for Wrapper").unwrap()];
    let source = format!(
        "#![allow(dead_code, unused_imports)]\n{without}\n\
         pub fn shared(s: PgStore) -> std::sync::Arc<dyn Store + Send + Sync> {{ std::sync::Arc::new(s) }}\n\
         pub fn binding(s: &dyn Store) -> Result<Option<Binding>> {{ s.resolve_binding(\"a.example/f\", \"2024-01-01T00:00:00.000000Z\") }}\n\
         pub fn resolver(s: &dyn Store) -> &dyn Resolver {{ s }}\n"
    );
    let (ok, stderr) = check("sealed_store_use", &source);
    assert!(ok, "{stderr}");
}
