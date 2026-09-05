// 對拍工具：對同一個 rollout 目錄輸出反查結果（SESSION/PREAMBLE/DIALOG），
// 與 Go 版（~/.zcode/zcode-advisor）的對照 shim 逐位元組 diff：
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
