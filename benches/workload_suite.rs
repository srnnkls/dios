#[path = "workload_suite/mod.rs"]
mod workload_suite;

fn main() -> std::process::ExitCode {
    let arguments: Vec<_> = std::env::args()
        .skip(1)
        .filter(|argument| argument != "--bench")
        .collect();
    match workload_suite::run(&arguments) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("workload suite: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
