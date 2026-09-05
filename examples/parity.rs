// Parity tool: prints the rollout-lookup result (SESSION/PREAMBLE/DIALOG) for
// a given rollout directory, for byte-exact diffing against the retired Go
// version's (~/.zcode/zcode-advisor) comparison shim:
//   cargo run --release --example parity -- <rollout-dir> <question>
use zcode_advisor::rollout::find_calling_session_in;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: parity <rollout-dir> <question>");
        std::process::exit(2);
    }
    match find_calling_session_in(std::path::Path::new(&args[1]), &args[2]) {
        None => println!("NO MATCH"),
        Some(m) => println!("SESSION:{}\nPREAMBLE:{}\nDIALOG:{}", m.session_id, m.preamble, m.dialog),
    }
}
