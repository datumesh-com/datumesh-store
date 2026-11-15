//! `computed_cells` (CONTRACT-1.8b S1): the cells matching a slice (as `slice_values` matches
//! them) that have a computed version answering as of `asof` (`tx <= asof` and the receipt's
//! `asof <= asof`), ascending by cell id, at most `limit`, each with its arguments. A cell
//! with only a stored version is not returned. `put_computed` registers the cell.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Cell, Type};

const T2: &str = "2026-01-02T00:00:00.000000Z";
const T3: &str = "2026-01-03T00:00:00.000000Z";
const NOON_2: &str = "2026-01-02T12:00:00.000000Z";
const T4: &str = "2026-01-04T00:00:00.000000Z";

fn open(h: &str) -> datumesh_core::Slice {
    slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#))
}

/// A function with three cells: k=1 computed at 2026-01-02, k=2 computed at 2026-01-03, k=3
/// with a stored version only. Returns the store, the function hash and the cells.
fn world(db: &TestDb) -> (TStore, String, [Cell; 3]) {
    let s = slice_store(db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let cell = |k: &str| slice_cell("f", &h, &[("k", sc(Type::Number, k))]);
    let cells = [cell("1"), cell("2"), cell("3")];
    for (c, v, tx) in [(&cells[0], "10", T2), (&cells[1], "20", T3)] {
        let hash = s.put_receipt(&slice_receipt(c, v, tx, tx)).unwrap();
        s.put_computed(&c.id(), tx, &hash).unwrap();
    }
    put_member(&s, &cells[2], "30", T2, T2);
    (s, h, cells)
}

fn ids(cells: &[Cell]) -> Vec<String> {
    cells.iter().map(|c| c.id()).collect()
}

fn sorted_ids(cells: &[&Cell]) -> Vec<String> {
    let mut v: Vec<String> = cells.iter().map(|c| c.id()).collect();
    v.sort();
    v
}

fn the_cells_with_a_computed_version_answering_as_of_asof_ascending_by_id(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h, cells) = world(&db);
    let sl = open(&h);
    // At 2026-01-04 both computed cells, not the stored-only one, ascending by cell id.
    let got = s.computed_cells(&sl, T4, 100).unwrap();
    assert_eq!(ids(&got), sorted_ids(&[&cells[0], &cells[1]]));
    // Each has its arguments (the cell itself).
    for c in &got {
        let want = cells.iter().find(|w| w.id() == c.id()).unwrap();
        assert_eq!(c, want);
        assert_eq!(c.args, want.args);
    }
    // At 2026-01-02T12:00Z only the cell computed at 2026-01-02.
    let got = s.computed_cells(&sl, NOON_2, 100).unwrap();
    assert_eq!(ids(&got), [cells[0].id()]);
    // At the computed version's own tx it answers.
    assert_eq!(ids(&s.computed_cells(&sl, T2, 100).unwrap()), [cells[0].id()]);
    // Before any computed version: none.
    assert!(s.computed_cells(&sl, "2026-01-01T00:00:00.000000Z", 100).unwrap().is_empty());
}

fn limit_keeps_the_smaller_cell_ids(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h, cells) = world(&db);
    let sl = open(&h);
    let both = sorted_ids(&[&cells[0], &cells[1]]);
    let got = s.computed_cells(&sl, T4, 1).unwrap();
    assert_eq!(ids(&got), [both[0].clone()]);
    assert_eq!(ids(&s.computed_cells(&sl, T4, 2).unwrap()), both);
    assert_eq!(ids(&s.computed_cells(&sl, T4, 3).unwrap()), both);
}

fn a_fixed_argument_in_the_slice_narrows_the_result(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h, cells) = world(&db);
    let fixed = |k: &str| {
        slice(&format!(
            r#"{{"args":{{"k":{{"type":"number","value":"{k}"}}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
        ))
    };
    assert_eq!(ids(&s.computed_cells(&fixed("1"), T4, 100).unwrap()), [cells[0].id()]);
    assert_eq!(ids(&s.computed_cells(&fixed("2"), T4, 100).unwrap()), [cells[1].id()]);
    // Narrowed to a cell that only has a stored version, or to none: nothing.
    assert!(s.computed_cells(&fixed("3"), T4, 100).unwrap().is_empty());
    assert!(s.computed_cells(&fixed("9"), T4, 100).unwrap().is_empty());
    // Narrowed to a cell whose computed version is later than asof: nothing.
    assert!(s.computed_cells(&fixed("2"), NOON_2, 100).unwrap().is_empty());
}

backends!(
    the_cells_with_a_computed_version_answering_as_of_asof_ascending_by_id,
    limit_keeps_the_smaller_cell_ids,
    a_fixed_argument_in_the_slice_narrows_the_result,
);
