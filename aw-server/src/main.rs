#[macro_use]
extern crate log;

use std::env;
use std::path::PathBuf;

use clap::crate_version;
use clap::Parser;

use aw_server::*;

#[cfg(target_os = "linux")]
use sd_notify::NotifyState;
#[cfg(all(target_os = "linux", target_arch = "x86"))]
extern crate jemallocator;
#[cfg(all(target_os = "linux", target_arch = "x86"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// Use mimalloc everywhere else (Windows/macOS/Linux desktop). Android builds
// the JNI cdylib via lib.rs, not this binary, so it is unaffected and keeps the
// system allocator. The cfg matches the dependency gate in Cargo.toml and is
// mutually exclusive with the jemalloc allocator above.
#[cfg(all(
    not(target_os = "android"),
    not(all(target_os = "linux", target_arch = "x86"))
))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Rust server for ActivityWatch
#[derive(Parser)]
#[clap(version = crate_version!(), author = "Johan Bjäreholt, Erik Bjäreholt, et al.")]
struct Opts {
    /// Run in testing mode
    #[clap(long)]
    testing: bool,

    /// Verbose output
    #[clap(long)]
    verbose: bool,

    /// Address to listen to
    #[clap(long)]
    host: Option<String>,

    /// Port to listen on
    #[clap(long)]
    port: Option<String>,

    /// Path to database override
    /// Also implies --no-legacy-import if no db found
    #[clap(long)]
    dbpath: Option<String>,

    /// Path to config file override
    #[clap(short = 'c', long = "config")]
    config: Option<PathBuf>,

    /// Path to webui override
    #[clap(long)]
    webpath: Option<String>,

    /// Mapping of custom static paths to serve, in the format: watcher1=/path,watcher2=/path2
    #[clap(long)]
    custom_static: Option<String>,

    /// Device ID override
    #[clap(long)]
    device_id: Option<String>,

    /// Don't import from aw-server-python if no aw-server-rust db found
    #[clap(long)]
    no_legacy_import: bool,
}

#[rocket::main]
#[allow(clippy::result_large_err)]
async fn main() -> Result<(), rocket::Error> {
    let opts: Opts = Opts::parse();

    let mut testing = opts.testing;

    // Always override environment if --testing is specified
    if !testing && cfg!(debug_assertions) {
        testing = true;
    }

    logging::setup_logger("aw-server-rust", testing, opts.verbose)
        .expect("Failed to setup logging");

    if testing {
        info!("Running server in Testing mode");
    }

    let mut config = config::create_config(testing, opts.config.as_deref());

    // set host if overridden
    if let Some(host) = opts.host {
        config.address = host;
    }

    // set port if overridden
    if let Some(port) = opts.port {
        config.port = port.parse().unwrap();
    }

    // set custom_static if overridden, transform into map
    if let Some(custom_static_str) = opts.custom_static {
        let custom_static_map: std::collections::HashMap<String, String> = custom_static_str
            .split(',')
            .map(|s| {
                let mut split = s.split('=');
                let key = split.next().unwrap().to_string();
                let value = split.next().unwrap().to_string();
                (key, value)
            })
            .collect();
        config.custom_static.extend(custom_static_map);

        // validate paths, log error if invalid
        // remove invalid paths
        for (name, path) in config.custom_static.clone().iter() {
            if !std::path::Path::new(path).exists() {
                error!("custom_static path for {} does not exist ({})", name, path);
                config.custom_static.remove(name);
            }
        }
    }

    // Set db path if overridden
    let db_path: String = if let Some(dbpath) = opts.dbpath.clone() {
        dbpath
    } else {
        dirs::db_path(testing)
            .expect("Failed to get db path")
            .to_str()
            .unwrap()
            .to_string()
    };
    info!("Using DB at path {:?}", db_path);

    // The DuckDB backend uses a new database file and does not read the old
    // SQLite database. Warn (rather than silently start empty) if a legacy
    // sqlite.db is still sitting next to it, so users notice their old history
    // is not being migrated.
    if opts.dbpath.is_none() {
        if let Ok(data_dir) = dirs::get_data_dir() {
            let legacy_db = data_dir.join(if testing {
                "sqlite-testing.db"
            } else {
                "sqlite.db"
            });
            if legacy_db.exists() {
                warn!(
                    "A legacy SQLite database exists at {:?} but the DuckDB backend does not read it; \
                     its history will not appear. The file is left untouched.",
                    legacy_db
                );
            }
        }
    }

    let asset_path = opts.webpath.map(PathBuf::from);
    info!("Using aw-webui assets at path {:?}", asset_path);

    // Only use legacy import if opts.dbpath is not set
    let legacy_import = !opts.no_legacy_import && opts.dbpath.is_none();
    if opts.dbpath.is_some() {
        info!("Since custom dbpath is set, --no-legacy-import is implied");
    }

    let device_id: String = if let Some(id) = opts.device_id {
        id
    } else {
        device_id::get_device_id()
    };

    // Encryption is not supported on the DuckDB backend. Refuse to start if a
    // password was requested rather than silently writing a plaintext database.
    if std::env::var_os("AW_DB_PASSWORD").is_some() {
        panic!(
            "AW_DB_PASSWORD is set but database encryption is not supported on this build. \
             Unset AW_DB_PASSWORD to use an unencrypted database."
        );
    }
    let datastore = aw_datastore::Datastore::new(db_path, legacy_import);

    let server_state = endpoints::ServerState {
        // Even if legacy_import is set to true it is disabled on Android so
        // it will not happen there
        datastore,
        asset_resolver: endpoints::AssetResolver::new(asset_path),
        device_id,
    };

    let _rocket = endpoints::build_rocket(server_state, config)
        .ignite()
        .await?;
    #[cfg(target_os = "linux")]
    let _ = sd_notify::notify(true, &[NotifyState::Ready]);
    _rocket.launch().await?;

    Ok(())
}
