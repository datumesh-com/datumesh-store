//! `datumesh-store`: migrate, import, export and benchmark a datumesh-store database.
//!
//! The database is named by `DATABASE_URL` (a `postgres://` URL; TLS as its `sslmode` says,
//! verified against the Mozilla roots plus the PEM bundle in `DATABASE_CA_FILE`, if set).
//! Exit status: 0 success, 1 failure (or drift found by `migrate --check`), 2 usage or
//! connection error.

#![forbid(unsafe_code)]

mod bench;
mod export;
mod import;

use std::process::ExitCode;

use datumesh_store::pg_unstable::{self, Connector};
use datumesh_store::{migrate, PgStore, PoolOptions};
use postgres::{Client, Config};

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

environment:
  DATABASE_URL       postgres://user:password@host:port/database[?sslmode=disable|prefer|require]
                     (TLS when the server offers it, by default; the server's certificate
                     and host name are always verified)
  DATABASE_CA_FILE   PEM file of root certificates to trust besides the Mozilla roots
                     (a private CA, a self-signed server)";

/// A failure and the exit status it gives.
pub(crate) struct Fail(pub u8, pub String);

impl<E: std::error::Error> From<E> for Fail {
    /// The error and its sources (a `postgres::Error` shows only "db error" by itself).
    fn from(e: E) -> Fail {
        Fail(1, chain(&e))
    }
}

/// An error's text followed by those of its sources, each once.
pub(crate) fn chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        // A store error's text may already hold its causes.
        let text = s.to_string();
        if !msg.contains(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = s.source();
    }
    msg
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

/// The pool options: the defaults, plus the roots of `DATABASE_CA_FILE`.
pub(crate) fn options() -> Result<PoolOptions, Fail> {
    let options = PoolOptions::new();
    let Some(path) = std::env::var_os("DATABASE_CA_FILE") else { return Ok(options) };
    let shown = path.to_string_lossy().into_owned();
    let pem = std::fs::read(&path).map_err(|e| Fail(2, format!("DATABASE_CA_FILE {shown}: {e}")))?;
    options.tls_root_certificates_pem(&pem).map_err(|e| Fail(2, format!("DATABASE_CA_FILE {shown}: {e}")))
}

pub(crate) fn config(url: &str) -> Result<Config, Fail> {
    url.parse().map_err(|e: postgres::Error| Fail(2, format!("DATABASE_URL: {}", chain(&e))))
}

/// Opens clients of `config` as the store connects (TLS, `DATABASE_CA_FILE`).
pub(crate) fn connector(config: Config) -> Result<Connector, Fail> {
    Connector::new(config, &options()?).map_err(|e| Fail(2, e.to_string()))
}

/// A plain client, connected as the store connects.
pub(crate) fn connect(url: &str) -> Result<Client, Fail> {
    connector(config(url)?)?.connect().map_err(|e| Fail(2, format!("cannot connect: {}", chain(&e))))
}

/// A store (`PgStore::connect_with`, with `DATABASE_CA_FILE`).
pub(crate) fn store(url: &str) -> Result<PgStore, Fail> {
    PgStore::connect_with(url, options()?).map_err(|e| match e {
        datumesh_store::Error::Config(_) => Fail(2, e.to_string()),
        e => Fail(2, format!("cannot connect: {e}")),
    })
}

fn migrate_apply(url: &str) -> Result<u8, Fail> {
    let store = store(url)?;
    let applied = migrate::apply(&store)?;
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
    let config = config(url)?;
    let live = store(url)?;
    let mut admin = connect(url)?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let scratch = format!("datumesh_check_{}_{nanos}", std::process::id());
    admin
        .batch_execute(&format!("CREATE DATABASE {scratch}"))
        .map_err(|e| Fail(1, format!("cannot create the scratch database (CREATEDB needed): {}", chain(&e))))?;
    let result = (|| -> Result<Vec<String>, Fail> {
        let mut sc = config.clone();
        sc.dbname(&scratch);
        let connector = connector(sc)?;
        let scratch_store = pg_unstable::with_connector(move || connector.connect(), PoolOptions::new().max_size(1))?;
        Ok(migrate::check(&live, &scratch_store)?)
    })();
    let dropped = admin.batch_execute(&format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"));
    let diff = result?;
    dropped?;
    if !diff.is_empty() {
        println!("schema drift ({} lines; - expected but missing, + present but not expected):", diff.len());
        for l in &diff {
            println!("{l}");
        }
        return Ok(1);
    }
    let n = live.check_statements()?;
    println!("no drift: the schema is what the migrations produce; {n} statements prepare");
    Ok(0)
}
