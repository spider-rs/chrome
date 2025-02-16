#[macro_use]
extern crate lazy_static;
use bytes::Bytes;
use cached::proc_macro::once;

mod conf;
mod modify;
mod proxy;

use conf::{
    CHROME_ARGS, CHROME_INSTANCES, CLIENT, DEFAULT_PORT, DEFAULT_PORT_SERVER, IS_HEALTHY,
    LIGHTPANDA_ARGS, TARGET_REPLACEMENT,
};
use core::sync::atomic::Ordering;
use hyper::{Body, Method, Request};
use std::process::Command;
use tokio::{signal, sync::oneshot};
use warp::{Filter, Rejection, Reply};

type Result<T> = std::result::Result<T, Rejection>;

lazy_static::lazy_static! {
    /// The hostname of the machine to replace 127.0.0.1 when making request to /json/version on port 6000.
    pub static ref HOST_NAME: String = {
        let mut hostname = String::new();

        if let Ok(name) = std::env::var("HOSTNAME_OVERRIDE") {
            hostname = name;
        }

        if hostname.is_empty() {
            if let Ok(name) = std::env::var("HOSTNAME") {
                hostname = name;
            }
        }

        hostname
    };
    static ref ENDPOINT: String = {
        let default_port = std::env::args()
            .nth(4)
            .unwrap_or("9223".into())
            .parse::<u32>()
            .unwrap_or_default();
        let default_port = if default_port == 0 {
            9223
        } else {
            default_port
        };
        format!("http://127.0.0.1:{}/json/version", default_port)
    };
}

/// shutdown the chrome instance by process id
#[cfg(target_os = "windows")]
fn shutdown(pid: &u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .spawn();
}

/// shutdown the chrome instance by process id
#[cfg(not(target_os = "windows"))]
fn shutdown(pid: &u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).spawn();
}

/// fork a chrome process
fn fork(
    chrome_path: &String,
    chrome_address: &String,
    port: Option<u32>,
    lightpanda_build: bool,
) -> String {
    let id = if !lightpanda_build {
        let mut command = Command::new(chrome_path);
        let mut chrome_args = CHROME_ARGS.map(|e| e.to_string());
        if !chrome_address.is_empty() {
            chrome_args[0] = format!("--remote-debugging-address={}", &chrome_address.to_string());
        }

        if let Some(port) = port {
            chrome_args[1] = format!("--remote-debugging-port={}", &port.to_string());
        }

        let cmd = command.args(&chrome_args);

        let id = if let Ok(child) = cmd.spawn() {
            let cid = child.id();

            tracing::info!("Chrome PID: {}", cid);

            cid
        } else {
            tracing::error!("chrome command didn't start");
            0
        };

        id
    } else {
        let panda_args = LIGHTPANDA_ARGS.map(|e| e.to_string());
        let mut command = Command::new(chrome_path);

        let host = panda_args[0].replace("--host=", "");
        let port = panda_args[1].replace("--port=", "");

        let id = if let Ok(child) = command
            .args(["--port", &port])
            .args(["--host", &host])
            .spawn()
        {
            let cid = child.id();

            tracing::info!("Chrome PID: {}", cid);

            cid
        } else {
            tracing::error!("chrome command didn't start");
            0
        };

        id
    };

    if let Ok(mut mutx) = CHROME_INSTANCES.lock() {
        mutx.insert(id.into());
    }

    id.to_string()
}

/// get json endpoint for chrome instance proxying
#[once(option = true, sync_writes = true)]
async fn version_handler_bytes(endpoint_path: Option<&str>) -> Option<Bytes> {
    use hyper::body::HttpBody;

    let req = Request::builder()
        .method(Method::GET)
        .uri(endpoint_path.unwrap_or(ENDPOINT.as_str()))
        .header("content-type", "application/json")
        .body(Body::default())
        .unwrap_or_default();

    let resp = match CLIENT.request(req).await {
        Ok(mut resp) => {
            IS_HEALTHY.store(true, Ordering::Relaxed);

            if !HOST_NAME.is_empty() {
                if let Ok(body_bytes) = resp.body_mut().collect().await {
                    let body = modify::modify_json_output(body_bytes.to_bytes());
                    return Some(body);
                }
            }

            Some(
                resp.body_mut()
                    .collect()
                    .await
                    .unwrap_or_default()
                    .to_bytes(),
            )
        }
        _ => {
            IS_HEALTHY.store(false, Ordering::Relaxed);
            None
        }
    };

    resp
}

/// get json endpoint for chrome instance proxying
async fn version_handler(endpoint_path: Option<&str>) -> Result<impl Reply> {
    let body = version_handler_bytes(endpoint_path)
        .await
        .unwrap_or_default();

    let modified_response = hyper::Response::builder()
        .body(Body::from(body))
        .unwrap_or_default();

    Ok(modified_response)
}

/// get json endpoint for chrome instance proxying
async fn version_handler_with_path(port: u32) -> Result<impl Reply> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("http://127.0.0.1:{}/json/version", port))
        .header("content-type", "application/json")
        .body(Body::default())
        .unwrap_or_default();

    let resp = match CLIENT.request(req).await {
        Ok(resp) => resp,
        _ => Default::default(),
    };

    Ok(resp)
}

/// health check server
async fn hc() -> Result<impl Reply> {
    use hyper::Response;
    use hyper::StatusCode;

    #[derive(Debug)]
    struct HealthCheckError;
    impl warp::reject::Reject for HealthCheckError {}

    if IS_HEALTHY.load(Ordering::Relaxed) {
        Response::builder()
            .status(StatusCode::OK)
            .body("healthy!")
            .map_err(|_e| warp::reject::custom(HealthCheckError))
    } else {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body("unhealthy!")
            .map_err(|_e| warp::reject::custom(HealthCheckError))
    }
}

/// Get the default chrome bin location per OS.
fn get_default_chrome_bin() -> &'static str {
    if cfg!(target_os = "windows") {
        "chrome.exe"
    } else if cfg!(target_os = "macos") {
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    } else if cfg!(target_os = "linux") {
        "chromium"
    } else {
        "chrome"
    }
}

/// Main entry for the proxy.
async fn run_main() {
    let chrome_path = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::var("CHROME_PATH").unwrap_or_else(|_| get_default_chrome_bin().to_string())
    });

    // use an env variable extend.
    let lightpanda_build = chrome_path.ends_with("lightpanda-aarch64-macos")
        || chrome_path.ends_with("lightpanda-x86_64-linux");

    let chrome_address = std::env::args().nth(2).unwrap_or("127.0.0.1".to_string());
    let auto_start = std::env::args().nth(3).unwrap_or_else(|| {
        let auto = std::env::var("CHROME_INIT").unwrap_or("false".into());
        if auto == "true" {
            "init".into()
        } else {
            "ignore".into()
        }
    });
    let chrome_path_1 = chrome_path.clone();
    let chrome_address_1 = chrome_address.clone();
    let chrome_address_2 = chrome_address.clone();

    // init chrome process
    if auto_start == "init" {
        fork(
            &chrome_path,
            &chrome_address_1,
            Some(*DEFAULT_PORT),
            lightpanda_build,
        );
    }

    let health_check = warp::path::end()
        .and_then(hc)
        .with(warp::cors().allow_any_origin());

    let chrome_init = move || fork(&chrome_path, &chrome_address_1, None, lightpanda_build);
    let chrome_init_args = move |port: u32| {
        fork(
            &chrome_path_1,
            &chrome_address_2,
            Some(port),
            lightpanda_build,
        )
    };
    let json_args = move || version_handler(None);
    let json_args_with_port = move |port| version_handler_with_path(port);

    let fork = warp::path!("fork").map(chrome_init);
    let fork_with_port = warp::path!("fork" / u32).map(chrome_init_args);

    let version = warp::path!("json" / "version").and_then(json_args);
    let version_with_port = warp::path!("json" / "version" / u32).and_then(json_args_with_port);

    let shutdown_fn = warp::path!("shutdown" / u32).map(|cid: u32| {
        let shutdown_id = match CHROME_INSTANCES.lock() {
            Ok(mutx) => {
                let pid = mutx.get(&cid);

                match pid {
                    Some(pid) => {
                        shutdown(pid);
                        pid.to_string()
                    }
                    _ => "0".into(),
                }
            }
            _ => "0".into(),
        };

        shutdown_id
    });

    let shutdown_base_fn = || {
        if let Ok(mutx) = CHROME_INSTANCES.lock() {
            for pid in mutx.iter() {
                shutdown(pid);
            }
        }

        "0"
    };

    let shutdown_base_fn = warp::path!("shutdown").map(shutdown_base_fn);

    let ctrls = warp::post().and(fork.with(warp::cors().allow_any_origin()));
    let ctrls_fork = warp::post().and(fork_with_port.with(warp::cors().allow_any_origin()));
    let shutdown = warp::post().and(shutdown_fn.with(warp::cors().allow_any_origin()));
    let shutdown_base = warp::post().and(shutdown_base_fn.with(warp::cors().allow_any_origin()));
    let version_port = warp::post().and(version_with_port.with(warp::cors().allow_any_origin()));

    let routes = warp::get()
        .and(health_check)
        .or(shutdown)
        .or(shutdown_base)
        .or(version)
        .or(ctrls_fork)
        .or(version_port)
        .or(ctrls);

    println!(
        "Chrome server at {}:{}",
        if chrome_address.is_empty() {
            "localhost"
        } else {
            &chrome_address
        },
        DEFAULT_PORT_SERVER.to_string()
    );

    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();

    let signal_handle = tokio::spawn(async move {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to setup signal handler")
            .recv()
            .await;

        tracing::info!("Received termination signal");

        if tx_shutdown.send(()).is_err() {
            tracing::error!("Failed to send shutdown signal through channel");
        }
    });

    let srv = async {
        tokio::select! {
            _ = warp::serve(routes)
            .run(([0, 0, 0, 0], DEFAULT_PORT_SERVER.to_owned())) => tracing::error!("Server finished without external shutdown."),
            _ = proxy::proxy::run_proxy() => tracing::error!("Received shutdown signal."),
            _ = rx_shutdown =>  tracing::error!("Received shutdown signal."),
        }
    };

    tokio::select! {
        _ = srv => (),
        _ = signal_handle => (),
    }
}

#[tokio::main]
async fn main() {
    run_main().await
}
