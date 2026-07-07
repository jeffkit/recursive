/// Minimal test helper for `src/hooks/external.rs` unit tests.
///
/// Usage:
///   hook_echo '<json>'          — print json to stdout, exit 0
///   hook_echo '<json>' <code>   — print json to stdout, exit <code>
///   hook_echo --exit <code>     — exit with <code> (no output)
///
/// Compiles to `target/debug/hook_echo` (or release). Unit tests locate it
/// at runtime via the CARGO_MANIFEST_DIR compile-time env var.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, code_str] if flag == "--exit" => {
            let code: i32 = code_str.parse().unwrap_or(1);
            std::process::exit(code);
        }
        [json] => {
            println!("{}", json);
        }
        [json, code_str] => {
            println!("{}", json);
            let code: i32 = code_str.parse().unwrap_or(0);
            if code != 0 {
                std::process::exit(code);
            }
        }
        _ => {
            // No args: output a default continue decision.
            println!(r#"{{"action":"continue"}}"#);
        }
    }
}
