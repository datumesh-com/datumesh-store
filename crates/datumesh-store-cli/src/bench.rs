//! `bench`: load signed receipts and versions, then time the store's operations.
//!
//! Load: the key documents of rates.example.org and calc.example.net (those of the
//! specification vectors) as namespaces; then `--versions` versions spread evenly over
//! `--cells` stored `fx_rate` cells (each version a receipt signed, with feature `sign`, by
//! the vectors' test key of rates.example.org, and verified by `put_receipt` against the
//! stored key document), stored through `put_receipt` and `put_version` from `--threads`
//! threads, then `--dependents` computed `convert` receipts (each of a cell of its own) that
//! all list the first version's receipt as their input. Every version's receipt is also
//! recorded as a computed version of its cell with `put_computed`, per cell newest first
//! (as a historical backfill would), and so is every dependent. The loading connections use
//! `synchronous_commit = off` (to load faster; durability of a benchmark load does not
//! matter); the timed operations use the server's setting.
//!
//! Timed, one call at a time, `--samples` each: `version_asof` (random cell, random asof),
//! `receipt` (random stored hash), `put_version` (a new latest version of a random cell; the
//! receipt is stored beforehand, untimed), `dependents` of the receipt with the dependents;
//! `computed_asof` and `stored_asof` (random cell, random asof), each alone, followed by
//! `receipt` of the answer, and as `*_with_receipt` (one query); `put_computed` (a receipt of
//! a random cell at a random earlier tx, sometimes late-signed, stored beforehand, untimed);
//! `cell_dependents` of the cell with the dependents; `resolve_binding` and `resolve_name`
//! (random name, random asof) over `NAMES` names with `BINDINGS` bindings each, alternating
//! between the two functions (bound once, untimed, when not bound yet).
//! Slices (0.3.1): `--slice-cells` cells of `fx_rate` with quote `SLA` (dates from 2000-01-01
//! on) and at most 1,000 with quote `SLB`, one version each, loaded like the others; then
//! `slice_members` of a range of 1,000 dates (bounded), of every `SLB` cell (unbounded, up
//! to 1,000 members), of every `SLA` cell, unbounded and as a range over all their dates;
//! the same four with `slice_values` and with `slice_changed_after` (0.3.2; half its samples
//! as of A, unchanged, half as of before the load, changed);
//! `versions_between` (random cell, random range of its history, random asof); `put_manifest`
//! of a new manifest (another range, the same members) of 1,000 and of `--slice-cells`
//! members, and `manifest` of each. The slower ones take fewer samples (stated).
//! Then disk use per table and `put_blob` / `blob` for sizes from 1 KiB to `--blob-max`.
//!
//! `bench --wait` polls until a fresh connection answers a `version_asof`, for timing a
//! cold restart (see tools/bench.sh).

use std::time::{Duration, Instant};

use datumesh_core::json::{self, Value};
use datumesh_core::{Cell, Manifest, Receipt, ReceiptBuilder, Scalar, SigningKey, Slice, Type};
use datumesh_store::{pg_unstable, PgStore, Resolver, Store};

use crate::{connect, Fail};

/// `receipt/stored-number`'s recompute descriptor (fx_rate, stored), canonical.
const FX_RATE: &str = r#"{"code":{"body":"","lang":"stored"},"inputs":[],"signature":{"args":{"base":{"type":"string","valid":false},"date":{"type":"date","valid":true},"quote":{"type":"string","valid":false}},"returns":{"type":"number","unit":{"of":{"arg":"quote"},"per":{"arg":"base"}}}}}"#;
/// `function/computed-convert` (convert, expr), canonical.
const CONVERT: &str = r#"{"code":{"body":"amount * rates.example.org/fx_rate(base: base, date: date, quote: quote)\n","lang":"expr"},"inputs":["rates.example.org/fx_rate"],"signature":{"args":{"amount":{"type":"number","unit":{"arg":"base"},"valid":false},"base":{"type":"string","valid":false},"date":{"type":"date","valid":true},"quote":{"type":"string","valid":false}},"returns":{"type":"number","unit":{"arg":"quote"}}}}"#;
/// The vectors' key documents for those seeds, canonical: receipts are verified against them.
const RATES_KEYS: &str = r#"{"host":"rates.example.org","keys":[{"alg":"ed25519","id":"d712b32a2cc5cb7c","public":"843baeb22bfd0364fb1b7262b367fb3202e7886c69a070fecf8e343fedb1eabf","valid_from":"2020-01-01T00:00:00.000000Z","valid_to":"2024-06-01T00:00:00.000000Z"},{"alg":"ed25519","id":"d9a2df65559c4444","public":"3ecf7b3ff834cb6782f5405dfcb213b5e78c0f30188068c25568adc25a739b52","valid_from":"2024-06-01T00:00:00.000000Z"}],"proto":"datumesh/1"}"#;
const CALC_KEYS: &str = r#"{"host":"calc.example.net","keys":[{"alg":"ed25519","id":"f7ec60a92abf4774","public":"ccd4faa3a0b8338bc796d3bfc30bda2f6e87d336a576b70dc9d47cbfe0e8830f","valid_from":"2020-01-01T00:00:00.000000Z"}],"proto":"datumesh/1"}"#;
/// When the benchmark's key documents count as validated.
const VALIDATED_AT: &str = "2021-01-01T00:00:00.000000Z";
/// The vectors' test seeds (public test keys, valid from 2020-01-01; rates' until 2024-06-01).
const RATES_SEED: &str = "d939265c141c467e77fb079486a3bb8b932c1d471c8c25e5a2eed06c42db8ad0";
const CALC_SEED: &str = "81589e69e00a82e983f0274d169ffa6c2ea7bc26833833826e57ce690125d5b5";

/// 2021-01-01T00:00:00Z in microseconds since the Unix epoch: the first version's tx.
const T0: i64 = 1_609_459_200_000_000;
const HOUR: i64 = 3_600_000_000;
/// Names bound for timing `resolve_binding`, and bindings per name (one an hour from T0).
const NAMES: usize = 100;
const BINDINGS: usize = 100;

pub(crate) struct Options {
    cells: usize,
    versions: usize,
    dependents: usize,
    samples: usize,
    threads: usize,
    blob_max: usize,
    slice_cells: usize,
    skip_load: bool,
    wait: bool,
}

impl Options {
    pub(crate) fn parse(args: &[String]) -> Result<Options, String> {
        let mut o = Options {
            cells: 10_000,
            versions: 1_000_000,
            dependents: 1_000,
            samples: 10_000,
            threads: 6,
            blob_max: 64 << 20,
            slice_cells: 100_000,
            skip_load: false,
            wait: false,
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut num = |name: &str| -> Result<usize, String> {
                it.next().and_then(|v| v.parse().ok()).ok_or(format!("{name} needs a number"))
            };
            match a.as_str() {
                "--cells" => o.cells = num(a)?.max(1),
                "--versions" => o.versions = num(a)?,
                "--dependents" => o.dependents = num(a)?,
                "--samples" => o.samples = num(a)?.max(1),
                "--threads" => o.threads = num(a)?.max(1),
                "--blob-max" => o.blob_max = num(a)?,
                "--slice-cells" => o.slice_cells = num(a)?.max(1),
                "--skip-load" => o.skip_load = true,
                "--wait" => o.wait = true,
                other => return Err(format!("unknown bench option {other:?}")),
            }
        }
        if o.versions < o.cells {
            return Err("--versions must be at least --cells".into());
        }
        Ok(o)
    }
}

pub(crate) fn run(url: &str, o: &Options) -> Result<u8, Fail> {
    if o.wait {
        return wait(url);
    }
    let rates = key(RATES_SEED);
    let calc = key(CALC_SEED);
    let per_cell = o.versions / o.cells;
    let measure = crate::store(url)?;
    measure.put_namespace("rates.example.org", RATES_KEYS.as_bytes(), VALIDATED_AT)?;
    measure.put_namespace("calc.example.net", CALC_KEYS.as_bytes(), VALIDATED_AT)?;
    let fx_rate = measure.put_descriptor(FX_RATE.as_bytes())?;
    let convert = measure.put_descriptor(CONVERT.as_bytes())?;

    let state = if o.skip_load {
        State::read(url)?
    } else {
        let connector = crate::connector(crate::config(url)?)?;
        let loader = pg_unstable::with_connector(
            move || {
                let mut c = connector.connect()?;
                c.batch_execute("SET synchronous_commit = off")?;
                Ok(c)
            },
            crate::options()?.max_size(o.threads),
        )?;
        let state = load(&loader, o, per_cell, &fx_rate, &convert, &rates, &calc)?;
        let start = Instant::now();
        let a = load_slice_cells(&loader, o.threads, &fx_rate, &rates, "SLA", o.slice_cells)?;
        let b = load_slice_cells(&loader, o.threads, &fx_rate, &rates, "SLB", o.slice_cells.min(1000))?;
        println!(
            "load: {} + {} slice cells (quotes SLA, SLB), a version each, in {:.1} s",
            a,
            b,
            start.elapsed().as_secs_f64()
        );
        state
    };
    if state.cells.is_empty() {
        return Err(Fail(1, "nothing loaded".into()));
    }

    let mut rng = Rng(0x5eed);
    println!("{:<26} {:>8} {:>10} {:>10} {:>10}", "op", "samples", "p50 µs", "p99 µs", "mean µs");

    // version_asof: a random cell as of a random time within (and a little past) its history.
    let mut t = Vec::with_capacity(o.samples);
    let mut found = 0;
    for _ in 0..o.samples {
        let c = &state.cells[rng.below(state.cells.len())];
        let asof = instant(c.first + rng.below_i64(c.last - c.first + HOUR));
        let s = Instant::now();
        let v = measure.version_asof(&c.id, &asof);
        t.push(s.elapsed());
        let v = v?;
        found += usize::from(v.is_some());
    }
    row("version_asof", &mut t, &format!("{found} found"));

    // receipt: a random stored receipt.
    let mut t = Vec::with_capacity(o.samples);
    for _ in 0..o.samples {
        let h = &state.hashes[rng.below(state.hashes.len())];
        let s = Instant::now();
        let r = measure.receipt(h);
        t.push(s.elapsed());
        if r.is_none() {
            return Err(Fail(1, format!("receipt {h} not found")));
        }
    }
    row("receipt", &mut t, "");

    // put_version: a new latest version of a random cell, its receipt stored first.
    let base = state.cells.iter().map(|c| c.last).max().unwrap_or(T0) + HOUR;
    let mut t = Vec::with_capacity(o.samples);
    for j in 0..o.samples {
        let c = &state.cells[j % state.cells.len()];
        let tx = instant(base + j as i64 * 1_000_000);
        let bytes = fx_receipt(&fx_rate, c.day, &tx, &tx, &format!("{}", 2000 + j), &rates)?;
        let h = measure.put_receipt(&bytes)?;
        let s = Instant::now();
        measure.put_version(&c.id, &tx, &h)?;
        t.push(s.elapsed());
    }
    row("put_version", &mut t, "one transaction each, synchronous_commit as configured");

    // dependents of the receipt with the most.
    if let Some((h, n)) = &state.most_depended {
        let mut t = Vec::with_capacity(o.samples);
        for _ in 0..o.samples {
            let s = Instant::now();
            let d = measure.dependents(h);
            t.push(s.elapsed());
            let d = d?;
            if d.len() != *n {
                return Err(Fail(1, format!("dependents: {} of {n}", d.len())));
            }
        }
        row("dependents", &mut t, &format!("{n} dependents each"));
    }

    // Answers as of A: a random cell as of a random time within (and a little past) its
    // history; alone, followed by `receipt` of the answer, and in one query.
    type Read<'a> = &'a dyn Fn(&str, &str) -> Result<bool, Fail>;
    let then_receipt =
        |v: Option<datumesh_store::Version>| v.is_some_and(|v| measure.receipt(&v.receipt_hash).is_some());
    let reads: [(&str, Read<'_>); 6] = [
        ("computed_asof", &|c, a| Ok(measure.computed_asof(c, a)?.is_some())),
        ("computed_asof+receipt", &|c, a| Ok(then_receipt(measure.computed_asof(c, a)?))),
        ("computed_asof_with_receipt", &|c, a| Ok(measure.computed_asof_with_receipt(c, a)?.is_some())),
        ("stored_asof", &|c, a| Ok(measure.stored_asof(c, a)?.is_some())),
        ("stored_asof+receipt", &|c, a| Ok(then_receipt(measure.stored_asof(c, a)?))),
        ("stored_asof_with_receipt", &|c, a| Ok(measure.stored_asof_with_receipt(c, a)?.is_some())),
    ];
    for (op, read) in reads {
        let mut t = Vec::with_capacity(o.samples);
        let mut found = 0;
        for _ in 0..o.samples {
            let c = &state.cells[rng.below(state.cells.len())];
            let asof = instant(c.first + rng.below_i64(c.last - c.first + HOUR));
            let s = Instant::now();
            let r = read(&c.id, &asof);
            t.push(s.elapsed());
            found += usize::from(r?);
        }
        row(op, &mut t, &format!("{found} found"));
    }

    // put_computed: a receipt of a random cell at a random earlier tx (a microsecond offset
    // keeps every key new), as of up to two hours later, stored first.
    let mut t = Vec::with_capacity(o.samples);
    for j in 0..o.samples {
        let c = &state.cells[rng.below(state.cells.len())];
        let tx_us = c.first + rng.below_i64(c.last - c.first + 1) + 1 + j as i64;
        let (tx, asof) = (instant(tx_us), instant(tx_us + rng.below_i64(2 * HOUR)));
        let bytes = fx_receipt(&fx_rate, c.day, &tx, &asof, &format!("{}", 3000 + j), &rates)?;
        let h = measure.put_receipt(&bytes)?;
        let s = Instant::now();
        measure.put_computed(&c.id, &tx, &h)?;
        t.push(s.elapsed());
    }
    row("put_computed", &mut t, "one transaction each, any order, synchronous_commit as configured");

    // cell_dependents of the cell with the most.
    if let Some((cell, n)) = &state.most_read_cell {
        let mut t = Vec::with_capacity(o.samples);
        for _ in 0..o.samples {
            let s = Instant::now();
            let d = measure.cell_dependents(cell);
            t.push(s.elapsed());
            let d = d?;
            if d.len() != *n {
                return Err(Fail(1, format!("cell_dependents: {} of {n}", d.len())));
            }
        }
        row("cell_dependents", &mut t, &format!("{n} dependent cells each"));
    }

    // resolve_binding / resolve_name: a random name as of a random time within (and a
    // little past) its bindings.
    let names: Vec<String> = (0..NAMES).map(|i| format!("bench.example.org/name_{i}")).collect();
    let end = T0 + BINDINGS as i64 * HOUR;
    for name in &names {
        if measure.resolve_binding(name, &instant(end))?.is_none() {
            for k in 0..BINDINGS {
                let h = if k % 2 == 0 { &fx_rate } else { &convert };
                measure.bind_name(name, h, &instant(T0 + k as i64 * HOUR))?;
            }
        }
    }
    type Name<'a> = &'a dyn Fn(&str, &str) -> Result<bool, Fail>;
    let lookups: [(&str, Name<'_>); 2] = [
        ("resolve_binding", &|n, a| Ok(measure.resolve_binding(n, a)?.is_some())),
        ("resolve_name", &|n, a| Ok(measure.resolve_name(n, a)?.is_some())),
    ];
    for (op, lookup) in lookups {
        let mut t = Vec::with_capacity(o.samples);
        let mut found = 0;
        for _ in 0..o.samples {
            let n = &names[rng.below(names.len())];
            let asof = instant(T0 - HOUR + rng.below_i64(end - T0 + 2 * HOUR));
            let s = Instant::now();
            let r = lookup(n, &asof);
            t.push(s.elapsed());
            found += usize::from(r?);
        }
        row(op, &mut t, &format!("{found} found; {NAMES} names x {BINDINGS} bindings"));
    }

    slices(&measure, url, o, &fx_rate, &state, &mut rng)?;

    disk(url)?;
    blobs(&measure, o.blob_max, &mut rng)?;
    Ok(0)
}

/// The slice of fx_rate cells with quote `quote`, dates in `[lo..hi]` (days), or all of them.
fn fx_slice(fx_rate: &str, quote: &str, range: Option<(i64, i64)>) -> Result<Slice, Fail> {
    let date = match range {
        Some((lo, hi)) => format!(r#"{{"hi":"{}","lo":"{}","type":"date"}}"#, date(hi), date(lo)),
        None => r#""*""#.to_string(),
    };
    Ok(Slice::from_json(
        format!(
            r#"{{"args":{{"base":{{"type":"string","value":"EUR"}},"date":{date},"quote":{{"type":"string","value":"{quote}"}}}},"fn":"fx_rate","fn_hash":"{fx_rate}","ns":"rates.example.org"}}"#
        )
        .as_bytes(),
    )?)
}

/// Makes the slice of one sample.
type SliceOf<'a> = Box<dyn FnMut(&mut Rng) -> Result<Slice, Fail> + 'a>;

/// The slice benchmarks (see the module docs).
fn slices(store: &PgStore, url: &str, o: &Options, fx_rate: &str, state: &State, rng: &mut Rng) -> Result<(), Fail> {
    let mut c = connect(url)?;
    let count = |c: &mut postgres::Client, quote: &str| -> Result<usize, Fail> {
        let n: i64 = c
            .query_one(
                "SELECT count(*) FROM cell_args WHERE fn_hash = $1 AND name = 'quote' AND value = $2",
                &[&fx_rate, &quote],
            )?
            .get(0);
        Ok(usize::try_from(n).unwrap_or(0))
    };
    let (na, nb) = (count(&mut c, "SLA")?, count(&mut c, "SLB")?);
    if na == 0 {
        println!("slices: no slice cells loaded");
        return Ok(());
    }
    let asof = instant(T0 + HOUR);
    let few = o.samples.min(10);
    let some = o.samples.min(200);
    let bounded = na.min(1000) as i64;
    let mut timed = |op: &str, samples: usize, mut f: SliceOf<'_>| -> Result<Vec<String>, Fail> {
        let mut t = Vec::with_capacity(samples);
        let mut members = Vec::new();
        let mut n = 0;
        let mut changed = 0;
        for _ in 0..samples {
            let sl = f(rng)?;
            let s = Instant::now();
            if op.starts_with("slice_values") {
                let vs = store.slice_values(&sl, &asof)?;
                t.push(s.elapsed());
                n = vs.len();
            } else if op.starts_with("slice_changed_after") {
                // Unchanged since the load (as of A), and changed (as of before the load).
                let at = if changed % 2 == 0 { asof.clone() } else { instant(T0 - HOUR) };
                let ch = store.slice_changed_after(&sl, &at)?;
                t.push(s.elapsed());
                if ch != (changed % 2 == 1) {
                    return Err(Fail(1, format!("slice_changed_after as of {at}: {ch}")));
                }
                changed += 1;
                n = store.slice_values(&sl, &asof)?.len();
            } else {
                let ms = store.slice_members(&sl, &asof)?;
                t.push(s.elapsed());
                n = ms.len();
                members = ms.into_iter().map(|m| m.version.receipt_hash).collect();
            }
        }
        row(op, &mut t, &format!("{n} members"));
        Ok(members)
    };
    let mut first = timed(
        "slice_members bounded",
        some,
        Box::new(|r: &mut Rng| {
            let lo = r.below_i64(na as i64 - bounded + 1);
            fx_slice(fx_rate, "SLA", Some((lo, lo + bounded - 1)))
        }),
    )?;
    timed("slice_members unbounded", some, Box::new(|_: &mut Rng| fx_slice(fx_rate, "SLB", None)))?;
    let mut all = timed("slice_members unbounded", few, Box::new(|_: &mut Rng| fx_slice(fx_rate, "SLA", None)))?;
    timed("slice_members bounded", few, Box::new(|_: &mut Rng| fx_slice(fx_rate, "SLA", Some((0, na as i64 - 1)))))?;
    let _ = nb;
    // The same shapes for slice_values (0.3.2) and slice_changed_after (half the samples
    // unchanged after A, half changed; the latter stops at the first match).
    for op in ["slice_values", "slice_changed_after"] {
        let many = if op == "slice_values" { few } else { some };
        timed(
            &format!("{op} bounded"),
            some,
            Box::new(|r: &mut Rng| {
                let lo = r.below_i64(na as i64 - bounded + 1);
                fx_slice(fx_rate, "SLA", Some((lo, lo + bounded - 1)))
            }),
        )?;
        timed(&format!("{op} unbounded"), some, Box::new(|_: &mut Rng| fx_slice(fx_rate, "SLB", None)))?;
        timed(&format!("{op} unbounded"), many, Box::new(|_: &mut Rng| fx_slice(fx_rate, "SLA", None)))?;
        timed(
            &format!("{op} bounded"),
            many,
            Box::new(|_: &mut Rng| fx_slice(fx_rate, "SLA", Some((0, na as i64 - 1)))),
        )?;
    }

    // versions_between: a random cell, a random range of its history, a random asof in it.
    let mut t = Vec::with_capacity(o.samples);
    let mut found = 0;
    for _ in 0..o.samples {
        let cell = &state.cells[rng.below(state.cells.len())];
        let span = cell.last - cell.first + HOUR;
        let from = cell.first + rng.below_i64(span);
        let to = from + rng.below_i64(span);
        let at = from + rng.below_i64(to - from + HOUR);
        let (from, to, at) = (instant(from), instant(to), instant(at));
        let s = Instant::now();
        let r = store.versions_between(&cell.id, &from, &to, &at);
        t.push(s.elapsed());
        found += r?.len();
    }
    row("versions_between", &mut t, &format!("{:.1} versions on average", found as f64 / o.samples.max(1) as f64));

    // put_manifest: a new manifest each time (another range beyond the data, the same
    // members), then manifest of each.
    first.sort();
    all.sort();
    let mut k = 0i64;
    for (members, samples) in [(first, some), (all, few)] {
        let n = members.len();
        let mut entries = Vec::with_capacity(samples);
        let mut t = Vec::with_capacity(samples);
        for _ in 0..samples {
            k += 1;
            let m = Manifest::new(fx_slice(fx_rate, "SLA", Some((0, na as i64 + k)))?, members.clone())?;
            let bytes = m.canonical();
            let s = Instant::now();
            let e = store.put_manifest(&bytes)?;
            t.push(s.elapsed());
            entries.push(e);
        }
        row("put_manifest", &mut t, &format!("{n} members, one transaction each"));
        let mut t = Vec::with_capacity(o.samples);
        for _ in 0..o.samples.min(samples * 20) {
            let e = &entries[rng.below(entries.len())];
            let s = Instant::now();
            let got = store.manifest(e);
            t.push(s.elapsed());
            if got.is_none() {
                return Err(Fail(1, format!("manifest {e} not found")));
            }
        }
        row("manifest", &mut t, &format!("{n} members"));
    }
    Ok(())
}

/// Loads `n` fx_rate cells with quote `quote` (dates from 2000-01-01 on), one version each
/// at T0, from `threads` threads; returns `n`.
fn load_slice_cells(
    store: &PgStore,
    threads: usize,
    fx_rate: &str,
    key: &SigningKey,
    quote: &str,
    n: usize,
) -> Result<usize, Fail> {
    let results: Vec<Result<(), Fail>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || -> Result<(), Fail> {
                    let tx = instant(T0);
                    for i in (t..n).step_by(threads) {
                        let bytes = fx_receipt_quote(fx_rate, quote, i as i64, &tx, &tx, &format!("{i}"), key)?;
                        let h = store.put_receipt(&bytes)?;
                        store.put_version(&Receipt::from_json(&bytes)?.cell().id(), &tx, &h)?;
                    }
                    Ok(())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(Fail(1, "a loader thread panicked".into()))))
            .collect()
    });
    for r in results {
        r?;
    }
    Ok(n)
}

fn key(seed: &str) -> SigningKey {
    let mut s = [0u8; 32];
    for (i, b) in s.iter_mut().enumerate() {
        *b = u8::from_str_radix(&seed[2 * i..2 * i + 2], 16).unwrap_or(0);
    }
    SigningKey::from_seed(&s)
}

/// A cell of the benchmark: its id, its `date` argument (days after 2000-01-01), and the
/// first and last tx of its versions (microseconds since the epoch).
struct CellInfo {
    id: String,
    day: i64,
    first: i64,
    last: i64,
}

struct State {
    cells: Vec<CellInfo>,
    hashes: Vec<String>,
    most_depended: Option<(String, usize)>,
    /// The cell read by the most cells, and how many.
    most_read_cell: Option<(String, usize)>,
}

impl State {
    /// The state of a database loaded by an earlier run (`--skip-load`).
    fn read(url: &str) -> Result<State, Fail> {
        let mut c = connect(url)?;
        let mut cells = Vec::new();
        for r in c.query(
            "SELECT v.cell_id, v.first, v.last, r.bytes FROM (SELECT cell_id, min(tx) AS first, max(tx) AS last, min(receipt_hash) AS h FROM versions GROUP BY cell_id) v JOIN receipts r ON r.hash = v.h",
            &[],
        )? {
            let (id, first, last, bytes): (String, String, String, Vec<u8>) = (r.get(0), r.get(1), r.get(2), r.get(3));
            let receipt = Receipt::from_json(&bytes)?;
            // The slice cells (quotes SLA, SLB) are not the benchmark's fx_rate cells.
            if receipt.cell().args.get("quote").and_then(|a| a.scalar()).map(|s| s.lexical()) != Some("USD") {
                continue;
            }
            let date = receipt.cell().args.get("date").and_then(|a| a.scalar()).map(|s| s.lexical().to_string());
            let day = date.as_deref().map(day_of).unwrap_or(0);
            cells.push(CellInfo { id, day, first: micros(&first), last: micros(&last) });
        }
        cells.sort_by(|a, b| a.id.cmp(&b.id));
        let hashes = c
            .query(
                "SELECT receipt_hash FROM versions WHERE cell_id = ANY($1)",
                &[&cells.iter().map(|c| c.id.clone()).collect::<Vec<String>>()],
            )?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let most_depended = c
            .query_opt("SELECT input_hash, count(*) FROM edges GROUP BY input_hash ORDER BY 2 DESC LIMIT 1", &[])?
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as usize));
        let most_read_cell = c
            .query_opt(
                "SELECT i.cell_id, count(DISTINCT d.cell_id) FROM edges e
                   JOIN receipts i ON i.hash = e.input_hash JOIN receipts d ON d.hash = e.receipt_hash
                  GROUP BY i.cell_id ORDER BY 2 DESC LIMIT 1",
                &[],
            )?
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as usize));
        Ok(State { cells, hashes, most_depended, most_read_cell })
    }
}

/// What one loader thread stored: its cells and its receipts' hashes.
type Loaded = (Vec<CellInfo>, Vec<String>);

#[allow(clippy::too_many_arguments)]
fn load(
    store: &PgStore,
    o: &Options,
    per_cell: usize,
    fx_rate: &str,
    convert: &str,
    rates: &SigningKey,
    calc: &SigningKey,
) -> Result<State, Fail> {
    let start = Instant::now();
    let results: Vec<Result<Loaded, Fail>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..o.threads)
            .map(|t| {
                scope.spawn(move || -> Result<Loaded, Fail> {
                    let mut cells = Vec::new();
                    let mut hashes = Vec::new();
                    let mut i = t;
                    while i < o.cells {
                        let day = i as i64;
                        let mut id = String::new();
                        let mut computed = Vec::with_capacity(per_cell);
                        for k in 0..per_cell {
                            let tx = instant(T0 + k as i64 * HOUR + i as i64 * 1_000_000);
                            let bytes = fx_receipt(fx_rate, day, &tx, &tx, &format!("{}", 1000 + k), rates)?;
                            let h = store.put_receipt(&bytes)?;
                            if k == 0 {
                                id = Receipt::from_json(&bytes)?.cell().id();
                            }
                            store.put_version(&id, &tx, &h)?;
                            computed.push((tx, h.clone()));
                            hashes.push(h);
                        }
                        // The same receipts as computed versions, newest first.
                        for (tx, h) in computed.iter().rev() {
                            store.put_computed(&id, tx, h)?;
                        }
                        let first = T0 + i as i64 * 1_000_000;
                        cells.push(CellInfo { id, day, first, last: first + (per_cell as i64 - 1) * HOUR });
                        i += o.threads;
                    }
                    Ok((cells, hashes))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(Fail(1, "a loader thread panicked".into()))))
            .collect()
    });
    let mut cells = Vec::new();
    let mut hashes = Vec::new();
    for r in results {
        let (c, h) = r?;
        cells.extend(c);
        hashes.extend(h);
    }
    cells.sort_by_key(|c| c.day);
    let loaded = start.elapsed();
    println!(
        "load: {} versions ({} cells x {per_cell}) in {:.1} s ({:.0} versions/s: sign + put_receipt (verifying) + put_version + put_computed, {} threads, synchronous_commit=off)",
        hashes.len(),
        cells.len(),
        loaded.as_secs_f64(),
        hashes.len() as f64 / loaded.as_secs_f64(),
        o.threads
    );

    // Dependents: convert receipts whose one input is the first version of cell 0.
    let mut most_depended = None;
    let mut most_read_cell = None;
    if o.dependents > 0 {
        let start = Instant::now();
        let c0 = &cells[0];
        let r0 = hashes.first().cloned().unwrap_or_default();
        let r0 = store
            .receipt(&r0)
            .map(|b| Receipt::from_json(&b))
            .transpose()?
            .ok_or(Fail(1, "no first receipt".into()))?;
        for j in 0..o.dependents {
            let cell = Cell::from_json(
                format!(
                    r#"{{"args":{{"amount":{{"type":"number","value":"{}"}},"base":{{"type":"string","value":"EUR"}},"date":{{"type":"date","value":"{}"}},"quote":{{"type":"string","value":"USD"}}}},"fn":"convert","fn_hash":"{convert}","ns":"calc.example.net"}}"#,
                    j + 1,
                    date(c0.day)
                )
                .as_bytes(),
            )?;
            let r = ReceiptBuilder::new()
                .cell(cell)
                .value(Scalar::from_lexical(Type::Number, &format!("{}", 1000 * (j + 1)))?)
                .valid(r0.valid().clone())
                .asof(r0.asof())
                .tx(r0.tx())
                .issued(r0.issued())
                .publisher("calc.example.net")
                .signer_host("calc.example.net")
                .inputs([r0.hash().to_string()])
                .licence("CC-BY-4.0")
                .summary("An amount converted at the reference rate.")
                .sign(calc)?;
            let h = store.put_receipt(&receipt_json(&r))?;
            store.put_computed(&r.cell().id(), r.tx(), &h)?;
        }
        println!("load: {} dependents of {} in {:.1} s", o.dependents, r0.hash(), start.elapsed().as_secs_f64());
        most_depended = Some((r0.hash().to_string(), o.dependents));
        most_read_cell = Some((c0.id.clone(), o.dependents));
    }
    Ok(State { cells, hashes, most_depended, most_read_cell })
}

/// A receipt of fx_rate(EUR, USD, date) with the given value at tx, as of asof.
fn fx_receipt(fx_rate: &str, day: i64, tx: &str, asof: &str, value: &str, key: &SigningKey) -> Result<Vec<u8>, Fail> {
    fx_receipt_quote(fx_rate, "USD", day, tx, asof, value, key)
}

/// A receipt of fx_rate(EUR, quote, date) with the given value at tx, as of asof.
fn fx_receipt_quote(
    fx_rate: &str,
    quote: &str,
    day: i64,
    tx: &str,
    asof: &str,
    value: &str,
    key: &SigningKey,
) -> Result<Vec<u8>, Fail> {
    let d = date(day);
    let cell = Cell::from_json(
        format!(
            r#"{{"args":{{"base":{{"type":"string","value":"EUR"}},"date":{{"type":"date","value":"{d}"}},"quote":{{"type":"string","value":"{quote}"}}}},"fn":"fx_rate","fn_hash":"{fx_rate}","ns":"rates.example.org"}}"#
        )
        .as_bytes(),
    )?;
    let valid = cell.args.iter().filter(|(k, _)| k.as_str() == "date").map(|(k, v)| (k.clone(), v.clone())).collect();
    let r = ReceiptBuilder::new()
        .cell(cell)
        .value(Scalar::from_lexical(Type::Number, value)?)
        .valid(valid)
        .asof(asof)
        .tx(tx)
        .issued(asof)
        .publisher("rates.example.org")
        .signer_host("rates.example.org")
        .licence("CC-BY-4.0")
        .summary(format!("1 EUR was worth {value} {quote} on {d}."))
        .sign(key)?;
    Ok(receipt_json(&r))
}

/// The canonical JSON text of a whole receipt (payload plus `sig`).
fn receipt_json(r: &Receipt) -> Vec<u8> {
    let mut members = vec![("sig".to_string(), Value::String(r.sig().to_string()))];
    if let Ok(Value::Object(m)) = json::parse(r.payload()) {
        members.extend(m);
    }
    Value::Object(members).to_jcs()
}

fn row(op: &str, t: &mut [Duration], note: &str) {
    t.sort();
    let n = t.len().max(1);
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let p50 = t.get(n * 50 / 100).copied().unwrap_or_default();
    let p99 = t.get((n * 99 / 100).min(n - 1)).copied().unwrap_or_default();
    let mean = t.iter().map(|d| us(*d)).sum::<f64>() / n as f64;
    println!("{op:<26} {:>8} {:>10.0} {:>10.0} {:>10.0}  {note}", t.len(), us(p50), us(p99), mean);
}

fn disk(url: &str) -> Result<(), Fail> {
    let mut c = connect(url)?;
    println!("disk: {:<26} {:>12} {:>12} {:>12}", "table", "table MB", "indexes MB", "total MB");
    let mb = |b: i64| b as f64 / (1024.0 * 1024.0);
    let mut sum = (0, 0, 0);
    for r in c.query(
        "SELECT c.relname, pg_table_size(c.oid), pg_indexes_size(c.oid), pg_total_relation_size(c.oid)
           FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace AND c.relkind = 'r' ORDER BY 4 DESC",
        &[],
    )? {
        let (name, t, i, all): (String, i64, i64, i64) = (r.get(0), r.get(1), r.get(2), r.get(3));
        sum = (sum.0 + t, sum.1 + i, sum.2 + all);
        println!("disk: {name:<26} {:>12.1} {:>12.1} {:>12.1}", mb(t), mb(i), mb(all));
    }
    println!("disk: {:<26} {:>12.1} {:>12.1} {:>12.1}", "(all tables)", mb(sum.0), mb(sum.1), mb(sum.2));
    let db: i64 = c.query_one("SELECT pg_database_size(current_database())", &[])?.get(0);
    println!("disk: database {:.1} MB", mb(db));
    Ok(())
}

fn blobs(store: &PgStore, max: usize, rng: &mut Rng) -> Result<(), Fail> {
    println!("{:<14} {:>12} {:>12} {:>12} {:>12} {:>12}", "blob", "bytes", "put ms", "get ms", "put MB/s", "get MB/s");
    let mut size = 1024;
    while size <= max {
        let reps = if size <= 1 << 20 { 20 } else { 3 };
        let (mut put, mut get) = (Vec::new(), Vec::new());
        for _ in 0..reps {
            let content: Vec<u8> = (0..size).map(|_| rng.next() as u8).collect();
            let s = Instant::now();
            let h = store.put_blob(&content)?;
            put.push(s.elapsed());
            let s = Instant::now();
            let back = store.blob(&h);
            get.push(s.elapsed());
            let back = back?;
            if back.as_deref() != Some(content.as_slice()) {
                return Err(Fail(1, format!("blob of {size} bytes did not come back")));
            }
        }
        put.sort();
        get.sort();
        let (p, g) = (put[put.len() / 2], get[get.len() / 2]);
        let mbs = |d: Duration| size as f64 / (1024.0 * 1024.0) / d.as_secs_f64().max(1e-9);
        println!(
            "{:<14} {size:>12} {:>12.2} {:>12.2} {:>12.1} {:>12.1}",
            "blob",
            p.as_secs_f64() * 1e3,
            g.as_secs_f64() * 1e3,
            mbs(p),
            mbs(g)
        );
        size *= 4;
    }
    Ok(())
}

fn wait(url: &str) -> Result<u8, Fail> {
    let start = Instant::now();
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let answered = (|| -> Option<()> {
            let mut c = connect(url).ok()?;
            let row = c.query_opt("SELECT cell_id, tx FROM versions ORDER BY cell_id LIMIT 1", &[]).ok()??;
            let (cell, tx): (String, String) = (row.get(0), row.get(1));
            let store = crate::store(url).ok()?;
            store.version_asof(&cell, &tx).ok()?.map(|_| ())
        })();
        if answered.is_some() {
            println!(
                "first version_asof answered after {:.0} ms ({attempts} attempts)",
                start.elapsed().as_secs_f64() * 1e3
            );
            return Ok(0);
        }
        if start.elapsed() > Duration::from_secs(300) {
            return Err(Fail(1, "no answer within 300 s".into()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn below_i64(&mut self, n: i64) -> i64 {
        (self.next() % n.max(1) as u64) as i64
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 2000-01-01 is day 10957 after the epoch; the benchmark's day n is 2000-01-01 + n.
fn date(day: i64) -> String {
    let (y, m, d) = civil(10_957 + day);
    format!("{y:04}-{m:02}-{d:02}")
}

fn day_of(date: &str) -> i64 {
    let p = |a: usize, b: usize| date.get(a..b).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    days_from_civil(p(0, 4), p(5, 7), p(8, 10)) - 10_957
}

/// Microseconds since the epoch as an instant.
pub(crate) fn instant(us: i64) -> String {
    let (days, rem) = (us.div_euclid(86_400_000_000), us.rem_euclid(86_400_000_000));
    let (y, m, d) = civil(days);
    let s = rem / 1_000_000;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z", s / 3600, (s / 60) % 60, s % 60, rem % 1_000_000)
}

/// An instant as microseconds since the epoch.
fn micros(t: &str) -> i64 {
    let p = |a: usize, b: usize| t.get(a..b).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    let days = days_from_civil(p(0, 4), p(5, 7), p(8, 10));
    ((days * 24 + p(11, 13)) * 60 + p(14, 16)) * 60 * 1_000_000 + p(17, 19) * 1_000_000 + p(20, 26)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants_and_dates_convert() {
        assert_eq!(instant(T0), "2021-01-01T00:00:00.000000Z");
        assert_eq!(micros("2021-01-01T00:00:00.000000Z"), T0);
        let t = "2024-02-29T23:59:59.999999Z";
        assert_eq!(instant(micros(t)), t);
        assert_eq!(date(0), "2000-01-01");
        assert_eq!(day_of("2000-03-01"), 60);
        assert_eq!(date(day_of("2027-05-18")), "2027-05-18");
    }
}
