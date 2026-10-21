//! `bench`: load signed receipts and versions, then time the store's operations.
//!
//! Load: `--versions` versions spread evenly over `--cells` stored `fx_rate` cells (each
//! version a receipt signed, with feature `sign`, by the specification vectors' test key of
//! rates.example.org), stored through `put_receipt` and `put_version` from `--threads`
//! threads, then `--dependents` computed `convert` receipts that all list the first version's
//! receipt as their input. The loading connections use `synchronous_commit = off` (to load
//! faster; durability of a benchmark load does not matter); the timed operations use the
//! server's setting.
//!
//! Timed, one call at a time, `--samples` each: `version_asof` (random cell, random asof),
//! `receipt` (random stored hash), `put_version` (a new latest version of a random cell; the
//! receipt is stored beforehand, untimed), `dependents` of the receipt with the dependents.
//! Then disk use per table and `put_blob` / `blob` for sizes from 1 KiB to `--blob-max`.
//!
//! `bench --wait` polls until a fresh connection answers a `version_asof`, for timing a
//! cold restart (see tools/bench.sh).

use std::time::{Duration, Instant};

use datumesh_core::json::{self, Value};
use datumesh_core::{Cell, Receipt, ReceiptBuilder, Scalar, SigningKey, Type};
use datumesh_store::postgres::{Config, NoTls};
use datumesh_store::{PgStore, PoolOptions, Resolver, Store};

use crate::{connect, Fail};

/// `receipt/stored-number`'s recompute descriptor (fx_rate, stored), canonical.
const FX_RATE: &str = r#"{"code":{"body":"","lang":"stored"},"inputs":[],"signature":{"args":{"base":{"type":"string","valid":false},"date":{"type":"date","valid":true},"quote":{"type":"string","valid":false}},"returns":{"type":"number","unit":{"of":{"arg":"quote"},"per":{"arg":"base"}}}}}"#;
/// `function/computed-convert` (convert, expr), canonical.
const CONVERT: &str = r#"{"code":{"body":"amount * rates.example.org/fx_rate(base: base, date: date, quote: quote)\n","lang":"expr"},"inputs":["rates.example.org/fx_rate"],"signature":{"args":{"amount":{"type":"number","unit":{"arg":"base"},"valid":false},"base":{"type":"string","valid":false},"date":{"type":"date","valid":true},"quote":{"type":"string","valid":false}},"returns":{"type":"number","unit":{"arg":"quote"}}}}"#;
/// The vectors' test seeds (public test keys, valid from 2020-01-01; rates' until 2024-06-01).
const RATES_SEED: &str = "d939265c141c467e77fb079486a3bb8b932c1d471c8c25e5a2eed06c42db8ad0";
const CALC_SEED: &str = "81589e69e00a82e983f0274d169ffa6c2ea7bc26833833826e57ce690125d5b5";

/// 2021-01-01T00:00:00Z in microseconds since the Unix epoch: the first version's tx.
const T0: i64 = 1_609_459_200_000_000;
const HOUR: i64 = 3_600_000_000;

pub(crate) struct Options {
    cells: usize,
    versions: usize,
    dependents: usize,
    samples: usize,
    threads: usize,
    blob_max: usize,
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
    let measure = PgStore::connect(url).map_err(|e| Fail(2, format!("cannot connect: {e}")))?;
    let fx_rate = measure.put_descriptor(FX_RATE.as_bytes())?;
    let convert = measure.put_descriptor(CONVERT.as_bytes())?;

    let state = if o.skip_load {
        State::read(url)?
    } else {
        let config: Config = url.parse().map_err(|e| Fail(2, format!("DATABASE_URL: {e}")))?;
        let loader = PgStore::with_connector(
            move || {
                let mut c = config.connect(NoTls)?;
                c.batch_execute("SET synchronous_commit = off")?;
                Ok(c)
            },
            PoolOptions::new().max_size(o.threads),
        )?;
        load(&loader, o, per_cell, &fx_rate, &convert, &rates, &calc)?
    };
    if state.cells.is_empty() {
        return Err(Fail(1, "nothing loaded".into()));
    }

    let mut rng = Rng(0x5eed);
    println!("{:<14} {:>8} {:>10} {:>10} {:>10}", "op", "samples", "p50 µs", "p99 µs", "mean µs");

    // version_asof: a random cell as of a random time within (and a little past) its history.
    let mut t = Vec::with_capacity(o.samples);
    let mut found = 0;
    for _ in 0..o.samples {
        let c = &state.cells[rng.below(state.cells.len())];
        let asof = instant(c.first + rng.below_i64(c.last - c.first + HOUR));
        let s = Instant::now();
        let v = measure.version_asof(&c.id, &asof);
        t.push(s.elapsed());
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
            if d.len() != *n {
                return Err(Fail(1, format!("dependents: {} of {n}", d.len())));
            }
        }
        row("dependents", &mut t, &format!("{n} dependents each"));
    }

    disk(url)?;
    blobs(&measure, o.blob_max, &mut rng)?;
    Ok(0)
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
}

impl State {
    /// The state of a database loaded by an earlier run (`--skip-load`).
    fn read(url: &str) -> Result<State, Fail> {
        let mut c = connect(url)?;
        let mut cells = Vec::new();
        for r in c.query(
            "SELECT v.cell_id, min(v.tx), max(v.tx), min(r.bytes) FROM versions v JOIN receipts r ON r.hash = v.receipt_hash GROUP BY v.cell_id",
            &[],
        )? {
            let (id, first, last, bytes): (String, String, String, Vec<u8>) = (r.get(0), r.get(1), r.get(2), r.get(3));
            let receipt = Receipt::from_json(&bytes)?;
            let date = receipt.cell().args.get("date").and_then(|a| a.scalar()).map(|s| s.lexical().to_string());
            let day = date.as_deref().map(day_of).unwrap_or(0);
            cells.push(CellInfo { id, day, first: micros(&first), last: micros(&last) });
        }
        cells.sort_by(|a, b| a.id.cmp(&b.id));
        let hashes = c.query("SELECT receipt_hash FROM versions", &[])?.iter().map(|r| r.get(0)).collect();
        let most_depended = c
            .query_opt("SELECT input_hash, count(*) FROM edges GROUP BY input_hash ORDER BY 2 DESC LIMIT 1", &[])?
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as usize));
        Ok(State { cells, hashes, most_depended })
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
                        for k in 0..per_cell {
                            let tx = instant(T0 + k as i64 * HOUR + i as i64 * 1_000_000);
                            let bytes = fx_receipt(fx_rate, day, &tx, &tx, &format!("{}", 1000 + k), rates)?;
                            let h = store.put_receipt(&bytes)?;
                            if k == 0 {
                                id = Receipt::from_json(&bytes)?.cell().id();
                            }
                            store.put_version(&id, &tx, &h)?;
                            hashes.push(h);
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
        "load: {} versions ({} cells x {per_cell}) in {:.1} s ({:.0} versions/s: sign + put_receipt + put_version, {} threads, synchronous_commit=off)",
        hashes.len(),
        cells.len(),
        loaded.as_secs_f64(),
        hashes.len() as f64 / loaded.as_secs_f64(),
        o.threads
    );

    // Dependents: convert receipts whose one input is the first version of cell 0.
    let mut most_depended = None;
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
            store.put_receipt(&receipt_json(&r))?;
        }
        println!("load: {} dependents of {} in {:.1} s", o.dependents, r0.hash(), start.elapsed().as_secs_f64());
        most_depended = Some((r0.hash().to_string(), o.dependents));
    }
    Ok(State { cells, hashes, most_depended })
}

/// A receipt of fx_rate(EUR, USD, date) with the given value at tx, as of asof.
fn fx_receipt(fx_rate: &str, day: i64, tx: &str, asof: &str, value: &str, key: &SigningKey) -> Result<Vec<u8>, Fail> {
    let d = date(day);
    let cell = Cell::from_json(
        format!(
            r#"{{"args":{{"base":{{"type":"string","value":"EUR"}},"date":{{"type":"date","value":"{d}"}},"quote":{{"type":"string","value":"USD"}}}},"fn":"fx_rate","fn_hash":"{fx_rate}","ns":"rates.example.org"}}"#
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
        .summary(format!("1 EUR was worth {value} USD on {d}."))
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
    println!("{op:<14} {:>8} {:>10.0} {:>10.0} {:>10.0}  {note}", t.len(), us(p50), us(p99), mean);
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
            let mut c = url.parse::<Config>().ok()?.connect(NoTls).ok()?;
            let row = c.query_opt("SELECT cell_id, tx FROM versions ORDER BY cell_id LIMIT 1", &[]).ok()??;
            let (cell, tx): (String, String) = (row.get(0), row.get(1));
            let store = PgStore::connect(url).ok()?;
            store.version_asof(&cell, &tx).map(|_| ())
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
fn instant(us: i64) -> String {
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
