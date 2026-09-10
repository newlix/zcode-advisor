fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--version" {
        println!("zcode-consultant {}", env!("CARGO_PKG_VERSION"));
        print_config_report();
        return;
    }
    if args.len() > 2 && args[1] == "hook" {
        zcode_consultant::hooks::run_hook(&args[2]);
        return;
    }
    zcode_consultant::server::run_server();
}

// print_config_report: what THIS environment would actually do — which
// config file was found (or why it wasn't; a broken file silently falls
// back to defaults, so "not what you configured" must be visible here), the
// effective backend/reviewer knobs, whether the claude binary resolves the
// way a consult would resolve it, and where runtime data lives. Pure reads:
// no API calls, no state writes, instant.
fn print_config_report() {
    let cfg = zcode_consultant::config::global();
    let path = zcode_consultant::config::config_path();
    let status = match &cfg.warning {
        Some(w) => format!("BROKEN, defaults in use ({w})"),
        None if path.exists() => "found".to_string(),
        None => "not found, built-in defaults".to_string(),
    };
    println!("config:   {} ({status})", path.display());
    println!("backend:  {} timeout={:?}", cfg.backend.kind_and_summary(), cfg.timeout);
    println!("reviewer: {} timeout={:?}", cfg.reviewer.summary(), cfg.reviewer.timeout);
    // claude-backed roles resolve their bin per call; here, once, so a
    // missing CLI is visible before the first consult fails (login state is
    // only exercised by a real call)
    let report_bin = |role: &str, bin: &str| match zcode_consultant::claude::resolve_bin(bin) {
        Ok(p) => println!("{role:<9} {} (found)", p.display()),
        Err(e) => println!("{role:<9} NOT FOUND: {e}"),
    };
    if let zcode_consultant::config::Backend::Claude { bin, .. } = &cfg.backend {
        report_bin("advisor", bin);
    }
    report_bin("reviewer", &cfg.reviewer.bin);
    println!(
        "data:     {} (consultant.log, state/)",
        zcode_consultant::util::data_dir().display()
    );
}
