use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // deeply nested programs recurse deeply in every pass: run on a large stack
    let code = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || sijoc::driver::main(&args))
        .expect("cannot spawn compiler thread")
        .join()
        .unwrap_or(101);
    exit(code);
}
