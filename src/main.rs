fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 && args[1] == "hook" {
        zcode_consultant::hooks::run_hook(&args[2]);
        return;
    }
    zcode_consultant::server::run_server();
}
