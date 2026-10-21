//! `datumesh-store`: migrate, import, export and benchmark a datumesh-store database.
//!
//! The database is named by `DATABASE_URL` (a `postgres://` URL, no TLS). Exit status: 0
//! success, 1 failure (or drift found by `migrate --check`), 2 usage or connection error.

#![forbid(unsafe_code)]

mod bench;
mod export;
mod import;

use std::process::ExitCode;

use datumesh_store::migrate;
use datumesh_store::postgres::{Client, Config, NoTls};

const USAGE: &str = "usage: datumesh-store [-v] <command>

commands:
  migrate            apply the migrations not yet applied
  migrate --check    compare the live schema with the migrations' (needs CREATEDB for a
                     scratch database) and prepare every statement; exit 1 on drift
  import [PATH...]   import spec vector files or directories, or JSON lines files
                     (\"-\" or nothing: JSON lines on stdin)
  export             write everything as JSON lines to stdout
  bench [--cells N] [--versions N] [--dependents N] [--samples N] [--threads N]
        [--blob-max BYTES] [--skip-load]
                     load and time the store (see README)
  bench --wait       wait until the database answers a version_asof; print the time

environment: DATABASE_URL (postgres://user:password@host:port/database)";

/// A failure and the exit status it gives.
pub(crate) struct Fail(pub u8, pub String);

impl<E: std::fmt::Display> From<E> for Fail {
    fn from(e: E) -> Fail {
        Fail(1, e.to_string())
    }
}

fn usage(msg: &str) -> Fail {
    Fail(2, format!("{msg}\n{USAGE}"))
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.first().is_some_and(|a| a == "-v");
    if verbose {
        args.remove(0);
    }
    let level = if verbose { tracing::Level::DEBUG } else { tracing::Level::WARN };
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_max_level(level).with_target(false).init();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(Fail(code, msg)) => {
            eprintln!("datumesh-store: {msg}");
            ExitCode::from(code)
        }
    }
}

fn run(args: &[String]) -> Result<u8, Fail> {
    let Some(command) = args.first() else { return Err(usage("no command")) };
    let rest = &args[1..];
    // Arguments are checked before the database is.
    match command.as_str() {
        "migrate" => {
            let check = match rest {
                [] => false,
                [f] if f == "--check" => true,
                _ => return Err(usage("migrate takes only --check")),
            };
            let url = database_url()?;
            if check {
                migrate_check(&url)
            } else {
                migrate_apply(&url)
            }
        }
        "import" => {
            let url = database_url()?;
            import::run(&url, rest)
        }
        "export" => {
            if !rest.is_empty() {
                return Err(usage("export takes no arguments"));
            }
            let url = database_url()?;
            export::run(&url)
        }
        "bench" => {
            let opts = bench::Options::parse(rest).map_err(|m| usage(&m))?;
            let url = database_url()?;
            bench::run(&url, &opts)
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(0)
        }
        other => Err(usage(&format!("unknown command {other:?}"))),
    }
}

fn database_url() -> Result<String, Fail> {
    std::env::var("DATABASE_URL").map_err(|_| Fail(2, "DATABASE_URL is not set".into()))
}

pub(crate) fn connect(url: &str) -> Result<Client, Fail> {
    let config: Config = url.parse().map_err(|e| Fail(2, format!("DATABASE_URL: {e}")))?;
    config.connect(NoTls).map_err(|e| Fail(2, format!("cannot connect: {e}")))
}

fn migrate_apply(url: &str) -> Result<u8, Fail> {
    let mut c = connect(url)?;
    let applied = migrate::apply(&mut c)?;
    if applied.is_empty() {
        println!("schema up to date ({} migrations)", migrate::MIGRATIONS.len());
    }
    for v in applied {
        let m = migrate::MIGRATIONS.iter().find(|m| m.version == v).map_or("", |m| m.name);
        println!("applied migration {v} ({m})");
    }
    Ok(0)
}

fn migrate_check(url: &str) -> Result<u8, Fail> {
    let config: Config = url.parse().map_err(|e| Fail(2, format!("DATABASE_URL: {e}")))?;
    let mut live = connect(url)?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let scratch = format!("datumesh_check_{}_{nanos}", std::process::id());
    live.batch_execute(&format!("CREATE DATABASE {scratch}"))
        .map_err(|e| Fail(1, format!("cannot create the scratch database (CREATEDB needed): {e}")))?;
    let result = (|| -> Result<Vec<String>, Fail> {
        let mut sc = config.clone();
        sc.dbname(&scratch);
        let mut scratch_client = sc.connect(NoTls)?;
        Ok(migrate::check(&mut live, &mut scratch_client)?)
    })();
    let dropped = live.batch_execute(&format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"));
    let diff = result?;
    dropped?;
    if !diff.is_empty() {
        println!("schema drift ({} lines; - expected but missing, + present but not expected):", diff.len());
        for l in &diff {
            println!("{l}");
        }
        return Ok(1);
    }
    let store = datumesh_store::PgStore::connect(url)?;
    let n = store.check_statements()?;
    println!("no drift: the schema is what the migrations produce; {n} statements prepare");
    Ok(0)
}
