"""Keep driver bypass evidence tied to exact mechanism and completed bytes."""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches/mmap_workloads"))
from probe_collect import validate_probe


def sample() -> dict:
    return dict(method="read_vectored", groups=6144, group_bytes=131072, pages=196608,
                useful_bytes=768 << 20, buffer_bytes=384 << 10, debug_assertions=False,
                elapsed_ns=1000000, cpu_ns=999000, io_mode="Direct", registration="Unregistered",
                file_registration="Fixed", ring_entries=64, memory_alignment=512,
                offset_alignment=512, outstanding_end=0, expected_checksum=123,
                cache_before=dict(cold_resident=0, cold_present_ptes=0, cold_expected=65536),
                counters=dict(submitted=6144, completed=6144, read_bytes=768 << 20,
                              checksum=123, allocations=0, outstanding_max=1,
                              enter_calls=6144, poll_calls=6144))


class ReadvProbeContract(unittest.TestCase):
    def test_valid_readv_and_scattered_counts(self):
        row = sample()
        validate_probe(row)
        row["method"] = "read_scattered"
        row["counters"].update(submitted=196608, completed=196608, outstanding_max=32)
        validate_probe(row)

    def test_rejects_mechanism_byte_and_drain_mismatches(self):
        for field, value in [("submitted", 196608), ("completed", 6143), ("read_bytes", 131072),
                             ("outstanding_max", 32), ("allocations", 1), ("checksum", 124)]:
            row = sample()
            row["counters"][field] = value
            with self.assertRaises(ValueError, msg=field):
                validate_probe(row)
        row = sample()
        row["outstanding_end"] = 1
        with self.assertRaises(ValueError):
            validate_probe(row)

    def test_pipelined_depth_and_adjacent_iovecs(self):
        for depth in (8, 16):
            for method, requests in [("read_scattered", 32), ("read_vectored", 1),
                                     ("read_contiguous", 1), ("read_vectored_adjacent", 1)]:
                row = pipelined_sample(depth, method, requests)
                validate_probe(row)

    def test_rejects_missing_overlap_and_unconsumed_groups(self):
        for field, value in [("groups_max", 1), ("groups_completed", 6143),
                             ("groups_consumed", 6143), ("refills_while_pending", 0),
                             ("outstanding_max", 9)]:
            row = pipelined_sample(8, "read_vectored", 1)
            row["counters"][field] = value
            with self.assertRaises(ValueError, msg=field):
                validate_probe(row)
        row = pipelined_sample(8, "read_vectored", 1)
        row["depth"] = 17
        with self.assertRaises(ValueError):
            validate_probe(row)

    def test_clock_split_requires_complete_ordered_positive_intervals(self):
        row = sample()
        row["diagnostic"] = dict(groups=[dict(group=index, submit_ns=10, ready_wait_ns=20)
                                          for index in range(6144)])
        validate_probe(row)
        for field, value in [("group", 1), ("submit_ns", -1), ("ready_wait_ns", 1000001)]:
            before = row["diagnostic"]["groups"][0][field]
            row["diagnostic"]["groups"][0][field] = value
            with self.assertRaises(ValueError, msg=field):
                validate_probe(row)
            row["diagnostic"]["groups"][0][field] = before
        row["diagnostic"]["groups"].pop()
        with self.assertRaises(ValueError):
            validate_probe(row)


def pipelined_sample(depth: int, method: str, requests: int) -> dict:
    row = sample()
    row.update(schema=2, method=method, depth=depth, ring_entries=depth * 64,
               buffer_bytes=depth * (384 << 10), group_bytes_max=depth * 131072)
    row["counters"].update(submitted=6144 * requests, completed=6144 * requests,
                           outstanding_max=depth * requests, groups_max=depth,
                           groups_completed=6144, groups_consumed=6144,
                           refills_while_pending=6000)
    return row


if __name__ == "__main__":
    unittest.main()
