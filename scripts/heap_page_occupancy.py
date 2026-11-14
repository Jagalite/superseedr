#!/usr/bin/env python3
"""Summarize macOS `heap --noContent --addresses=all PID` snapshots.

This measures virtual pages intersecting live malloc blocks, not resident pages
or reclaimable memory. Holes can contain allocator metadata. Entirely empty
allocator pages and internal waste inside rounded malloc blocks are not counted.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import re


BLOCK = re.compile(r"^0x([0-9a-fA-F]+): .*\((\d+) bytes\)$")


def summarize(text, page_size):
    if page_size <= 0 or page_size & (page_size - 1):
        raise ValueError("page size must be a positive power of two")
    blocks = sorted(
        (int(m[1], 16), int(m[2]))
        for line in text.splitlines()
        if (m := BLOCK.match(line))
    )
    if not blocks:
        raise ValueError("no allocation addresses found; use heap --addresses=all")
    pages = Counter()
    sizes = Counter()
    previous_end = 0
    for address, size in blocks:
        if size <= 0 or address < previous_end:
            raise ValueError("zero-sized, duplicate, or overlapping allocation records")
        end = address + size
        previous_end = end
        sizes[size] += 1
        for page in range(address // page_size, (end - 1) // page_size + 1):
            pages[page] += min(end, (page + 1) * page_size) - max(address, page * page_size)
    bands = {label: dict(pages=0, allocated_bytes=0, outside_block_bytes=0)
             for label in ("<=25%", "25-50%", "50-75%", ">75%")}
    for used in pages.values():
        label = ("<=25%" if used * 4 <= page_size else
                 "25-50%" if used * 2 <= page_size else
                 "50-75%" if used * 4 <= page_size * 3 else ">75%")
        band = bands[label]
        band["pages"] += 1
        band["allocated_bytes"] += used
        band["outside_block_bytes"] += page_size - used
    allocated = sum(size * count for size, count in sizes.items())
    assert allocated == sum(pages.values())
    sparse_contributors = Counter()
    for address, size in blocks:
        end = address + size
        for page in range(address // page_size, (end - 1) // page_size + 1):
            if pages[page] * 4 <= page_size:
                sparse_contributors[size] += min(end, (page + 1) * page_size) - max(address, page * page_size)
    return dict(
        page_size=page_size, blocks=len(blocks), allocated_bytes=allocated,
        pages_with_live_blocks=len(pages), page_span_bytes=len(pages) * page_size,
        outside_block_bytes=len(pages) * page_size - allocated,
        occupancy=bands,
        sparse_page_contributors=[dict(block_bytes=size, bytes_on_sparse_pages=count)
                                  for size, count in sparse_contributors.most_common(15)],
        largest_size_classes=[dict(block_bytes=size, count=count, total_bytes=size * count)
                              for size, count in sorted(sizes.items(), key=lambda x: -x[0] * x[1])[:15]],
        limitation="Virtual address occupancy; not residency or reclaimable bytes. Excludes entirely empty pages and internal allocation waste.",
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("--page-size", type=int, required=True,
                        help="Target page size in bytes (macOS: sysctl -n hw.pagesize)")
    args = parser.parse_args()
    print(json.dumps(summarize(args.snapshot.read_text(), args.page_size), indent=2))


if __name__ == "__main__":
    main()
