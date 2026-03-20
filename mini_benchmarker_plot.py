#!/usr/bin/env python3
"""Render comparison charts from tagged Mini Benchmarker logs."""

from __future__ import annotations

import argparse
import csv
import re
import statistics
from pathlib import Path

import matplotlib.pyplot as plt

TEST_ORDER = [
    "stress-ng cpu-cache-mem",
    "y-cruncher pi 1b",
    "perf sched msg fork thread",
    "perf memcpy",
    "namd 92K atoms",
    "calculating prime numbers",
    "argon2 hashing",
    "ffmpeg compilation",
    "xz compression",
    "kernel defconfig",
    "blender render",
    "x265 encoding",
    "Total time (s)",
    "Total score",
]

TEST_PATTERN = re.compile(
    r"^(stress-ng cpu-cache-mem|y-cruncher pi 1b|perf sched msg fork thread|"
    r"perf memcpy|namd 92K atoms|calculating prime numbers|argon2 hashing|"
    r"ffmpeg compilation|xz compression|kernel defconfig|blender render|"
    r"x265 encoding|Total time \(s\)|Total score):\s+([0-9]+\.[0-9]+)$",
    re.MULTILINE,
)
KERNEL_PATTERN = re.compile(r"Kernel:\s+(\S+)")
LABEL_PATTERN = re.compile(r"Benchmark label:\s+(.+)")


def parse_log(path: Path) -> tuple[str, dict[str, float]]:
    text = path.read_text(encoding="utf-8", errors="replace")
    label_match = LABEL_PATTERN.search(text)
    if label_match:
        label = label_match.group(1).strip()
    else:
        kernel_match = KERNEL_PATTERN.search(text)
        if not kernel_match:
            raise ValueError(f"Could not find Kernel: line in {path}")
        label = kernel_match.group(1)

    values: dict[str, float] = {}
    for test_name, value in TEST_PATTERN.findall(text):
        values[test_name] = float(value)

    missing = [name for name in TEST_ORDER if name not in values]
    if missing:
        raise ValueError(f"{path} is missing benchmark values: {', '.join(missing)}")

    return label, values


def aggregate_logs(log_dir: Path) -> tuple[list[str], dict[str, dict[str, float]], dict[str, int]]:
    grouped: dict[str, dict[str, list[float]]] = {}
    run_counts: dict[str, int] = {}

    for path in sorted(log_dir.glob("*.log")):
        label, values = parse_log(path)
        if label not in grouped:
            grouped[label] = {name: [] for name in TEST_ORDER}
            run_counts[label] = 0
        run_counts[label] += 1
        for test_name in TEST_ORDER:
            grouped[label][test_name].append(values[test_name])

    if not grouped:
        raise ValueError(f"No .log files found in {log_dir}")

    labels = list(grouped.keys())
    averages = {
        label: {
            test_name: statistics.fmean(samples)
            for test_name, samples in grouped[label].items()
        }
        for label in labels
    }
    return labels, averages, run_counts


def write_csv(out_dir: Path, labels: list[str], averages: dict[str, dict[str, float]], run_counts: dict[str, int]) -> None:
    csv_path = out_dir / "mini_benchmarker_summary.csv"
    with csv_path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(["label", "runs", "benchmark", "mean_seconds"])
        for label in labels:
            for test_name in TEST_ORDER:
                writer.writerow([label, run_counts[label], test_name, f"{averages[label][test_name]:.2f}"])


def render_chart(out_dir: Path, labels: list[str], averages: dict[str, dict[str, float]], title: str) -> None:
    tests = list(reversed(TEST_ORDER))
    series_count = len(labels)
    figure_height = max(8.0, len(tests) * 0.85 + series_count * 0.6)
    fig, ax = plt.subplots(figsize=(14, figure_height))

    bar_height = 0.8 / max(series_count, 1)
    positions = list(range(len(tests)))

    for index, label in enumerate(labels):
        offset = (index - (series_count - 1) / 2.0) * bar_height
        values = [averages[label][test_name] for test_name in tests]
        ys = [pos + offset for pos in positions]
        bars = ax.barh(ys, values, height=bar_height, label=label)
        for bar, value in zip(bars, values):
            ax.text(
                bar.get_width(),
                bar.get_y() + bar.get_height() / 2.0,
                f"{value:.2f}",
                va="center",
                ha="left",
                fontsize=9,
            )

    ax.set_yticks(positions)
    ax.set_yticklabels(tests)
    ax.set_xlabel("Average Time (s). Less is better")
    ax.set_ylabel("Mini Benchmarker")
    ax.set_title(title)
    ax.grid(axis="x", alpha=0.4)
    ax.legend(loc="lower right")
    fig.tight_layout()

    fig.savefig(out_dir / "mini_benchmarker_comparison.png", dpi=200)
    fig.savefig(out_dir / "mini_benchmarker_comparison.svg")
    plt.close(fig)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log_dir", type=Path, help="Directory containing tagged Mini Benchmarker .log files")
    parser.add_argument(
        "--title",
        default="Mini Benchmarker Comparison",
        help="Chart title",
    )
    args = parser.parse_args()

    log_dir = args.log_dir.resolve()
    labels, averages, run_counts = aggregate_logs(log_dir)
    write_csv(log_dir, labels, averages, run_counts)
    render_chart(log_dir, labels, averages, args.title)
    print(f"Wrote chart and CSV summary to {log_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
