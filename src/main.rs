// Moonruster - Moonraker (Klipper API server) ported to Rust

mod assets;
mod async_serial;
mod common;
mod configparser;
mod confighelper;
mod components;
mod loghelper;
#[cfg(feature = "plugins")]
mod plugins;
mod pyfmt;
mod server;
mod sysfs_devs;
mod utils;

use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// The Moonraker release this port tracks.  Reported as the software
/// version so clients apply the correct feature set.
pub const MOONRAKER_COMPAT_VERSION: &str = "v0.11.0-3-g9e676eb";

struct Args {
    datapath: Option<String>,
    configfile: Option<String>,
    logfile: Option<String>,
    unixsocket: Option<String>,
    nologfile: bool,
    structured: bool,
    verbose: bool,
    debug: bool,
    asyncio_debug: bool,
}

fn env_bool(key: &str) -> bool {
    matches!(std::env::var(key).unwrap_or_default().to_lowercase().as_str(), "y" | "yes" | "true")
}

fn usage() -> ! {
    println!(
        "usage: moonruster [-h] [-d <data path>] [-c <configfile>] [-l <logfile>] [-u <unixsocket>] [-n] [-s] [-v] [-g] [-o]\n\n\
         Moonraker - Klipper API Server\n\n\
         options:\n  -h, --help            show this help message and exit\n  \
         -d <data path>, --datapath <data path>\n                        Location of Moonraker Data File Path\n  \
         -c <configfile>, --configfile <configfile>\n                        Path to Moonraker's configuration file\n  \
         -l <logfile>, --logfile <logfile>\n                        Path to Moonraker's log file\n  \
         -u <unixsocket>, --unixsocket <unixsocket>\n                        Path to Moonraker's unix domain socket\n  \
         -n, --nologfile       disable logging to a file\n  \
         -s, --structured-logging\n                        Enable structured file logging\n  \
         -v, --verbose         Enable verbose logging\n  \
         -g, --debug           Enable Moonraker debug features\n  \
         -o, --asyncio-debug   Enable asyncio debug flag"
    );
    std::process::exit(0);
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args {
        datapath: std::env::var("MOONRAKER_DATA_PATH").ok(),
        configfile: std::env::var("MOONRAKER_CONFIG_PATH").ok(),
        logfile: std::env::var("MOONRAKER_LOG_PATH").ok(),
        unixsocket: std::env::var("MOONRAKER_UDS_PATH").ok(),
        nologfile: env_bool("MOONRAKER_DISABLE_FILE_LOG"),
        structured: env_bool("MOONRAKER_STRUCTURED_LOGGING"),
        verbose: env_bool("MOONRAKER_VERBOSE_LOGGING"),
        debug: env_bool("MOONRAKER_ENABLE_DEBUG"),
        asyncio_debug: env_bool("MOONRAKER_ASYNCIO_DEBUG"),
    };
    // argparse also accepts attached short values ("-d/path") and clustered
    // short flags ("-nv"); expand those first
    let mut expanded: Vec<String> = vec![argv.first().cloned().unwrap_or_default()];
    for arg in argv.iter().skip(1) {
        let b = arg.as_bytes();
        if b.len() > 2 && b[0] == b'-' && b[1] != b'-' {
            if "dclu".contains(b[1] as char) {
                expanded.push(arg[..2].to_string());
                expanded.push(arg[2..].to_string());
                continue;
            }
            if arg[1..].chars().all(|c| "nsvgoh".contains(c)) {
                expanded.extend(arg[1..].chars().map(|c| format!("-{}", c)));
                continue;
            }
        }
        expanded.push(arg.clone());
    }
    let argv = &expanded;
    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        let take = |i: &mut usize| -> String {
            if let Some(v) = &inline {
                return v.clone();
            }
            *i += 1;
            match argv.get(*i) {
                Some(v) => v.clone(),
                None => {
                    eprintln!("moonruster: error: argument {}: expected one argument", flag);
                    std::process::exit(2);
                }
            }
        };
        match flag {
            "-h" | "--help" => usage(),
            "-d" | "--datapath" => a.datapath = Some(take(&mut i)),
            "-c" | "--configfile" => a.configfile = Some(take(&mut i)),
            "-l" | "--logfile" => a.logfile = Some(take(&mut i)),
            "-u" | "--unixsocket" => a.unixsocket = Some(take(&mut i)),
            "-n" | "--nologfile" => a.nologfile = true,
            "-s" | "--structured-logging" => a.structured = true,
            "-v" | "--verbose" => a.verbose = true,
            "-g" | "--debug" => a.debug = true,
            "-o" | "--asyncio-debug" => a.asyncio_debug = true,
            // MOONRAKER_ARGS from existing service env files: "-m moonraker"
            // or, on older installs, the path to moonraker.py
            "-m" | "moonraker" => {}
            p if p.ends_with("moonraker.py") => {}
            other => {
                eprintln!("moonruster: error: unrecognized arguments: {}", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }
    a
}

fn main() {
    // glibc's malloc keeps 128 kB padding on top of the heap and trims only
    // past 128 kB free: about 0.8 MB more own memory than musl for the same
    // allocations (docs/PLUGINS.md "Build"); return freed memory promptly
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_TOP_PAD, 0);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 0);
    }
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|a| a == "--metadata").unwrap_or(false) {
        std::process::exit(components::file_manager::metadata::main(&argv[2..]));
    }
    let args = parse_args(&argv);
    let mut startup_warnings: Vec<Value> = Vec::new();
    let dp = args.datapath.clone().unwrap_or_else(|| "~/printer_data".into());
    let data_path = utils::resolve_path(&utils::expand_user(&dp));
    if !data_path.exists() && std::fs::create_dir(&data_path).is_err() {
        startup_warnings.push(json!(format!("Unable to create data path folder at {}", data_path.display())));
    }
    let uuid_path = data_path.join(".moonraker.uuid");
    let instance_uuid = if !uuid_path.is_file() {
        let u = uuid::Uuid::new_v4().simple().to_string();
        let _ = std::fs::write(&uuid_path, &u);
        u
    } else {
        std::fs::read_to_string(&uuid_path).unwrap_or_default().trim().to_string()
    };
    let cfg_file = args
        .configfile
        .clone()
        .unwrap_or_else(|| data_path.join("config/moonraker.conf").to_string_lossy().to_string());
    let unix_sock = match &args.unixsocket {
        Some(u) => u.clone(),
        None => {
            let comms = data_path.join("comms");
            if !comms.exists() {
                let _ = std::fs::create_dir(&comms);
            }
            comms.join("moonraker.sock").to_string_lossy().to_string()
        }
    };
    let misc = data_path.join("misc");
    if !misc.exists() {
        let _ = std::fs::create_dir(&misc);
    }
    let log_file = if args.nologfile {
        String::new()
    } else if let Some(l) = &args.logfile {
        utils::normpath(&utils::absolute(&utils::expand_user(l))).to_string_lossy().to_string()
    } else {
        data_path.join("logs/moonraker.log").to_string_lossy().to_string()
    };
    let mut app_args = Map::new();
    app_args.insert("data_path".into(), json!(data_path.to_string_lossy()));
    app_args.insert("is_default_data_path".into(), json!(args.datapath.is_none()));
    app_args.insert("config_file".into(), json!(cfg_file));
    app_args.insert("backup_config".into(), json!(confighelper::find_config_backup(&cfg_file)));
    app_args.insert("startup_warnings".into(), Value::Array(Vec::new()));
    app_args.insert("verbose".into(), json!(args.verbose));
    app_args.insert("debug".into(), json!(args.debug));
    app_args.insert("asyncio_debug".into(), json!(args.asyncio_debug));
    app_args.insert("is_backup_config".into(), json!(false));
    app_args.insert("is_python_package".into(), json!(false));
    app_args.insert("instance_uuid".into(), json!(instance_uuid));
    app_args.insert("unix_socket_path".into(), json!(unix_sock));
    app_args.insert("structured_logging".into(), json!(args.structured));
    app_args.insert("software_version".into(), json!(MOONRAKER_COMPAT_VERSION));
    app_args.insert("log_file".into(), json!(log_file));
    app_args.insert("python_version".into(), json!(format!("moonruster {} (rust)", env!("CARGO_PKG_VERSION"))));
    app_args.insert("launch_args".into(), json!(argv.join(" ")));
    app_args.insert("msgspec_enabled".into(), json!(true));
    app_args.insert("uvloop_enabled".into(), json!(false));
    app_args.insert("moonruster_version".into(), json!(env!("CARGO_PKG_VERSION")));
    if let Some(w) = loghelper::init(&app_args) {
        startup_warnings.push(json!(w));
    }
    app_args.insert("startup_warnings".into(), Value::Array(startup_warnings));
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    // Python components: the GIL is released while the loop thread waits
    #[cfg(feature = "plugins")]
    builder.on_thread_park(plugins::park_hook).on_thread_unpark(plugins::unpark_hook);
    let rt = builder.build().expect("tokio runtime");
    let local = tokio::task::LocalSet::new();
    let status = local.block_on(&rt, launch_server(app_args));
    match status {
        Some(code) => {
            log::info!("Server Shutdown");
            log::logger().flush();
            std::process::exit(code);
        }
        None => {
            log::info!("Attempting Server Restart...");
            log::logger().flush();
            restart_process(&argv);
        }
    }
}

/// Returns Some(exit code) to exit or None to restart
async fn launch_server(app_args: Map<String, Value>) -> Option<i32> {
    let srv = server::Server::new(app_args);
    if let Err(e) = srv.init().and_then(|_| srv.load_components()) {
        log::error!("Server Config Error\n{}", e);
        // moonraker returns True here, which results in exit(True)
        return Some(1);
    }
    if let Err(e) = srv.server_init(true).await {
        log::error!("Server Running Error\n{}", e);
        return Some(1);
    }
    srv.run_until_exit().await;
    if *srv.exit_reason.borrow() == "terminate" {
        return Some(0);
    }
    None
}

fn restart_process(argv: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    std::thread::sleep(std::time::Duration::from_millis(500));
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(&argv[0]));
    let err = std::process::Command::new(exe).args(&argv[1..]).exec();
    eprintln!("Failed to restart: {}", err);
    std::process::exit(1);
}
