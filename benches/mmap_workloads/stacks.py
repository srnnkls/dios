"""Measured-loop boundaries verified against engine::timed_workload's callees."""

TIMED_BOUNDARIES = (
    "engine::timed_workload",
    "engine::read_dios",
    "engine::read_mmap",
    "scan::read_dios",
    "scan::read_mmap",
    "probe::Probe::run",
)


def workload_boundary(frames: list[str], boundaries=TIMED_BOUNDARIES) -> int | None:
    # The dispatcher can tail-call its read loop even with inline(never).
    # These callees only execute inside the timed region; setup uses other paths.
    return next(
        (
            index
            for index, frame in enumerate(frames)
            if any(boundary in frame for boundary in boundaries)
        ),
        None,
    )
