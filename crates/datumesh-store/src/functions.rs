//! Function tests (0.5.1): the checks both backends apply before they store or record one.

use datumesh_core::json;
use datumesh_core::lexical::{check_hex, check_instant};
use datumesh_core::ErrorCode;

use crate::error::{Error, Result};
use crate::FunctionTest;

/// The results a run may record.
const RESULTS: &[&str] = &["pass", "fail", "error"];

fn invalid(path: &str, detail: &str) -> Error {
    Error::Invalid(datumesh_core::Error::new(ErrorCode::Schema, path, detail))
}

/// A case: 16 lowercase hex digits.
pub(crate) fn check_case(case: &str) -> Result<()> {
    check_hex(case, 16, "case")?;
    Ok(())
}

/// A recorded result: `pass`, `fail` or `error`.
pub(crate) fn check_result(result: &str) -> Result<()> {
    if RESULTS.contains(&result) {
        Ok(())
    } else {
        Err(invalid("result", "a result is pass, fail or error"))
    }
}

/// The branch counts as the database holds them (`integer`, so at most `i32::MAX`), with
/// covered not above total.
pub(crate) fn branch_counts(total: u32, covered: u32) -> Result<(i32, i32)> {
    if covered > total {
        return Err(invalid("branches_covered", "more than branches_total"));
    }
    let as_int = |n: u32, path: &str| i32::try_from(n).map_err(|_| invalid(path, "more than 2147483647"));
    Ok((as_int(total, "branches_total")?, as_int(covered, "branches_covered")?))
}

/// Every check of a function test to store: the hash, the case, the test line (canonical
/// JSON), the signer, and whatever run and branch fields it carries. Returns the branch
/// counts as the database holds them.
pub(crate) fn check_test(t: &FunctionTest) -> Result<(i32, i32)> {
    check_hex(&t.fn_hash, 64, "fn_hash")?;
    check_case(&t.case)?;
    if json::parse(&t.test_json)?.to_jcs() != t.test_json {
        return Err(invalid("test_json", "the test line is not canonical JSON"));
    }
    if t.signed_by.is_empty() || t.signed_by.len() > 256 {
        return Err(invalid("signed_by", "a signer of 1 to 256 bytes"));
    }
    if let Some(at) = &t.last_run_at {
        check_instant(at, "last_run_at")?;
    }
    if let Some(r) = &t.last_result {
        check_result(r)?;
    }
    branch_counts(t.branches_total, t.branches_covered)
}
