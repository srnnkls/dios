#[cfg(target_os = "linux")]
#[path = "mmap_workloads/mod.rs"]
mod mmap_workloads;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    {
        let arguments: Vec<_> = std::env::args()
            .skip(1)
            .filter(|argument| argument != "--bench")
            .collect();
        match mmap_workloads::run(&arguments) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("mmap workloads: {error}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "mmap fault/cache-pressure comparisons require Linux; see benches/plans/mmap_workloads.md"
        );
        std::process::ExitCode::FAILURE
    }
}
