//! Store 0.6.0, R8 and R11: the catalogue entry's `source_name` and the bounded `extensions_of`.
//! Both backends. Both are in the unreleased migration 14 (SQLite 10).
//!
//! API assumed (ADDITIVE; the structs are `#[non_exhaustive]`):
//!
//! ```text
//! // CatalogEntryExt gains:  pub source_name: Option<String>   (CatalogEntryExt::new: None)
//! // FacetedEntry gains:     pub source_name: Option<String>
//! // on Store:
//! fn extensions_of_limit(&self, class: &str, limit: usize) -> Result<Vec<CatalogEntryExt>>;
//! ```
//!
//! Rules:
//! - `extensions_of_limit(class, n)` is the first `n` of `extensions_of(class)` (functions
//!   whose `extends` is `class`, by name in byte order): at most `n`; `n = 0` is empty; a `n`
//!   above the count gives all; a `class` that is not a full name is `Error::Invalid`.
//! - On PostgreSQL a partial index on `catalog_entries (extends, name)` exists (an index of
//!   that table whose definition names `extends` and `name` and has a WHERE clause).
//! - `source_name` is stored as given by `upsert_catalog_ext` and read back unchanged by
//!   `catalog_entry_ext` and `catalog_entries_with_facets`; absent stays `None`; republishing
//!   the entry with `None` clears it (as facets are replaced); the 0.5.2 `upsert_catalog`
//!   leaves it of an existing row unchanged.

#[macro_use]
mod common;

use std::collections::BTreeMap;

use common::*;
use datumesh_store::{CatalogEntry, CatalogEntryExt, CatalogKind, CatalogSource, Error, Facets};

const CLASS: &str = "rates.example.org/fx_pair";
const OTHER: &str = "rates.example.org/fx_other";
const F1: &str = "rates.example.org/fx_rate";

fn entry(kind: CatalogKind, name: &str) -> CatalogEntry {
    CatalogEntry {
        kind,
        name: name.to_string(),
        hash: None,
        label: "l".to_string(),
        description: "d".to_string(),
        args: BTreeMap::new(),
        examples: "[]".to_string(),
        source: CatalogSource::Publisher,
        signed_by: Some("rates.example.org".to_string()),
    }
}

fn ext(name: &str, extends: Option<&str>, source_name: Option<&str>) -> CatalogEntryExt {
    let mut e = CatalogEntryExt::new(entry(CatalogKind::Function, name));
    e.extends = extends.map(str::to_string);
    e.source_name = source_name.map(str::to_string);
    e
}

fn names(v: Vec<CatalogEntryExt>) -> Vec<String> {
    v.into_iter().map(|e| e.entry.name).collect()
}

fn extensions_of_limit_is_the_first_n_by_name(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    // Inserted out of order; two of another class and a function that extends nothing.
    let all: Vec<CatalogEntryExt> = ["e", "b", "d", "a", "c"]
        .iter()
        .map(|n| ext(&format!("rates.example.org/fx_{n}"), Some(CLASS), None))
        .chain([ext("rates.example.org/fx_z", Some(OTHER), None), ext("rates.example.org/fx_y", None, None)])
        .collect();
    s.upsert_catalog_ext(&all).unwrap();
    let want = |n: usize| -> Vec<String> {
        ["a", "b", "c", "d", "e"].iter().take(n).map(|x| format!("rates.example.org/fx_{x}")).collect()
    };
    for n in [1, 2, 3, 5] {
        assert_eq!(names(s.extensions_of_limit(CLASS, n).unwrap()), want(n), "limit {n}");
    }
    assert_eq!(names(s.extensions_of_limit(CLASS, 100).unwrap()), want(5));
    assert!(s.extensions_of_limit(CLASS, 0).unwrap().is_empty());
    assert_eq!(s.extensions_of_limit(CLASS, 3).unwrap(), s.extensions_of(CLASS).unwrap()[..3].to_vec());
    assert_eq!(names(s.extensions_of_limit(OTHER, 4).unwrap()), ["rates.example.org/fx_z"]);
    assert!(s.extensions_of_limit("rates.example.org/fx_none", 4).unwrap().is_empty());
    let e = s.extensions_of_limit("not a name", 4).unwrap_err();
    assert!(matches!(e, Error::Invalid(_)), "{e:?}");
}

fn the_extends_index_exists_on_postgres(b: Backend) {
    let db = TestDb::migrated(b);
    if b != Backend::Pg {
        // SQLite: the same partial index is expected by name-independent means.
        let defs = db.texts(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'catalog_entries' AND sql IS NOT NULL",
        );
        assert!(
            defs.iter()
                .any(|d| d[0].contains("extends") && d[0].contains("name") && d[0].to_uppercase().contains("WHERE")),
            "{defs:?}"
        );
        return;
    }
    let defs = db.texts("SELECT indexdef FROM pg_indexes WHERE tablename = 'catalog_entries'");
    assert!(
        defs.iter().any(|d| {
            let i = d[0].as_str();
            i.contains("(extends, name)") && i.contains("WHERE")
        }),
        "{defs:?}"
    );
}

fn source_name_round_trips_and_absent_stays_none(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let with = ext(F1, None, Some("EUR/USD reference rate"));
    let without = ext("rates.example.org/fx_cross", None, None);
    s.upsert_catalog_ext(&[with.clone(), without.clone()]).unwrap();
    assert_eq!(s.catalog_entry_ext(CatalogKind::Function, F1).unwrap(), Some(with));
    let got = s.catalog_entry_ext(CatalogKind::Function, "rates.example.org/fx_cross").unwrap().unwrap();
    assert_eq!(got.source_name, None);
    assert_eq!(got, without);
    // A new entry from the 0.5.2 call has None.
    let mut plain = entry(CatalogKind::Function, "rates.example.org/fx_plain");
    plain.hash = None;
    s.upsert_catalog(&[plain]).unwrap();
    assert_eq!(
        s.catalog_entry_ext(CatalogKind::Function, "rates.example.org/fx_plain").unwrap().unwrap().source_name,
        None
    );
}

fn source_name_is_replaced_by_a_republish_and_kept_by_the_old_upsert(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    s.upsert_catalog_ext(&[ext(F1, None, Some("first"))]).unwrap();
    s.upsert_catalog_ext(&[ext(F1, None, Some("second"))]).unwrap();
    let sn = || s.catalog_entry_ext(CatalogKind::Function, F1).unwrap().unwrap().source_name;
    assert_eq!(sn().as_deref(), Some("second"));
    // The 0.5.2 upsert leaves it.
    let mut e = entry(CatalogKind::Function, F1);
    e.label = "relabelled".to_string();
    s.upsert_catalog(&[e]).unwrap();
    assert_eq!(sn().as_deref(), Some("second"));
    // A republish without it clears it.
    s.upsert_catalog_ext(&[ext(F1, None, None)]).unwrap();
    assert_eq!(sn(), None);
}

fn source_name_is_in_catalog_entries_with_facets(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let facets: Facets = [("domain".to_string(), vec!["finance".to_string()])].into_iter().collect();
    let mut a = ext(F1, None, Some("EUR/USD reference rate"));
    a.facets = facets.clone();
    let mut c = ext("rates.example.org/fx_cross", None, None);
    c.facets = facets;
    s.upsert_catalog_ext(&[a, c]).unwrap();
    let listed = s.catalog_entries_with_facets().unwrap();
    let by: BTreeMap<String, Option<String>> = listed.into_iter().map(|f| (f.name, f.source_name)).collect();
    assert_eq!(by["rates.example.org/fx_cross"], None);
    assert_eq!(by[F1].as_deref(), Some("EUR/USD reference rate"));
    assert_eq!(by.len(), 2);
}

backends!(
    extensions_of_limit_is_the_first_n_by_name,
    the_extends_index_exists_on_postgres,
    source_name_round_trips_and_absent_stays_none,
    source_name_is_replaced_by_a_republish_and_kept_by_the_old_upsert,
    source_name_is_in_catalog_entries_with_facets,
);
