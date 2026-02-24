use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use tiny_http::{Header, Response, Server};

fn get_available_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("Failed to bind to any port")
        .local_addr()
        .expect("Failed to get local address")
        .port()
}

fn get_available_port_with_offset(offset: u16) -> u16 {
    for _ in 0..20 {
        let base = get_available_port();
        let candidate = base as u32 + offset as u32;
        if candidate > u16::MAX as u32 {
            continue;
        }
        let port = candidate as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    get_available_port()
}

/// Get the dist directory path (parent of the executable's directory)
fn get_dist_dir() -> PathBuf {
    let exe_path = env::current_exe().expect("Failed to get executable path");
    let exe_dir = exe_path.parent().expect("Failed to get executable directory");

    // If running from target/release or target/debug, go up to dist
    // Otherwise assume exe is directly in dist
    if exe_dir.ends_with("release") || exe_dir.ends_with("debug") {
        exe_dir
            .parent() // target
            .and_then(|p| p.parent()) // perfetto_launcher
            .and_then(|p| p.parent()) // dist
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| exe_dir.to_path_buf())
    } else {
        exe_dir.to_path_buf()
    }
}

/// Get MIME type based on file extension
fn get_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

fn main() {
    println!("=== Perfetto Launcher ===\n");

    // Get the dist directory
    let dist_dir = get_dist_dir();
    println!("Dist directory: {}\n", dist_dir.display());

    // Verify trace_processor_shell.exe exists
    let trace_processor_path = dist_dir.join("trace_processor_shell.exe");
    if !trace_processor_path.exists() {
        eprintln!("Error: trace_processor_shell.exe not found at {}", trace_processor_path.display());
        eprintln!("Make sure to place the launcher in the correct location.");
        return;
    }

    // Verify index.html exists
    let index_path = dist_dir.join("index.html");
    if !index_path.exists() {
        eprintln!("Error: index.html not found at {}", index_path.display());
        return;
    }

    // Pre-allocate ports so we can configure CORS on trace_processor_shell
    // before the UI HTTP server starts.
    let rpc_port = get_available_port_with_offset(10000);
    let mut http_port = get_available_port_with_offset(10000);
    while http_port == rpc_port {
        http_port = get_available_port_with_offset(10000);
    }
    println!("Starting trace_processor_shell...");
    println!("  Path: {}", trace_processor_path.display());
    println!("  HTTP port: {}", rpc_port);

    let cors_origins = format!(
        "http://localhost:{},http://127.0.0.1:{}",
        http_port, http_port
    );
    let mut args = vec![
        "-D".to_string(),
        "--http-ip-address".to_string(),
        "127.0.0.1".to_string(),
        "--http-port".to_string(),
        rpc_port.to_string(),
        "--http-additional-cors-origins".to_string(),
        cors_origins,
    ];
    
    // Check if a trace file was provided as a command line argument
    if let Some(arg) = env::args().nth(1) {
        let path = Path::new(&arg);
        if path.exists() {
            println!("  Loading trace file: {}", arg);
            args.push(arg);
        } else {
            eprintln!("Warning: Provided trace file does not exist: {}", arg);
        }
    }

    let mut trace_processor = Command::new(&trace_processor_path)
        .args(&args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start trace_processor_shell");

    // Wait for trace_processor to start
    println!("\nWaiting for trace_processor to start...");
    thread::sleep(std::time::Duration::from_millis(500));

    // Start HTTP server
    println!("\nStarting HTTP server on port {}...", http_port);
    let server = Server::http(format!("0.0.0.0:{}", http_port)).expect("Failed to start HTTP server");

    let ui_url = format!("http://localhost:{}/?rpc_port={}", http_port, rpc_port);
    println!("\n=== Perfetto is ready! ===");
    println!("  UI Server:            http://localhost:{}/", http_port);
    println!("  Trace Processor RPC:  http://localhost:{}/", rpc_port);
    println!("\nPress Ctrl+C to stop.\n");

    // Open browser
    if let Err(e) = open::that(&ui_url) {
        eprintln!("Warning: Failed to open browser: {}", e);
        println!("Please open {} manually.", ui_url);
    }

    // Handle requests
    let dist_dir_clone = dist_dir.clone();
    for request in server.incoming_requests() {
        let url_path = request.url().trim_start_matches('/');
        let url_path = url_path.split('?').next().unwrap_or(url_path); // Remove query string

        let file_path = if url_path.is_empty() {
            dist_dir_clone.join("index.html")
        } else {
            dist_dir_clone.join(url_path)
        };

        // Security: ensure path is within dist_dir
        let canonical = match file_path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                let response = Response::from_string("Not Found").with_status_code(404);
                let _ = request.respond(response);
                continue;
            }
        };

        let dist_canonical = match dist_dir_clone.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                let response = Response::from_string("Internal Error").with_status_code(500);
                let _ = request.respond(response);
                continue;
            }
        };

        if !canonical.starts_with(&dist_canonical) {
            let response = Response::from_string("Forbidden").with_status_code(403);
            let _ = request.respond(response);
            continue;
        }

        // Read and serve file
        match fs::read(&canonical) {
            Ok(content) => {
                let mime_type = get_mime_type(&canonical);
                let content_type = Header::from_bytes("Content-Type", mime_type).unwrap();
                let cors_origin = Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();

                let response = Response::from_data(content)
                    .with_header(content_type)
                    .with_header(cors_origin);
                let _ = request.respond(response);
            }
            Err(_) => {
                let response = Response::from_string("Not Found").with_status_code(404);
                let _ = request.respond(response);
            }
        }
    }

    // Cleanup (this won't be reached normally, but just in case)
    let _ = trace_processor.kill();
    let _ = trace_processor.wait();
    println!("Goodbye!");
}
