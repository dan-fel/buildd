#!/usr/bin/env python3
"""Load test: many sessions editing and building one Cargo workspace at once.

Each session has its own git worktree of REPOSITORY at BASE and one crate to
work on. It repeats ITERATIONS times: think for a while, edit the crate's
library root, then ask for `check -p CRATE` and `test -p CRATE --no-run`.

  --mode buildd   requests go to a buildd daemon this script starts with its
                  own home (slots and limits from the options)
  --mode cargo    each session runs Cargo in its worktree with its own target
                  directory, as sessions do without buildd

The report gives the time to finish, request latency, buildd's queue and build
times, peak and final build disk, and failures, as text and as JSON.
"""

import argparse
import hashlib
import json
import math
import os
import random
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path


def git(directory, *args):
    return subprocess.run(
        ["git", "-C", str(directory), *args], check=True, capture_output=True, text=True
    ).stdout


def disk_kib(paths):
    existing = [str(path) for path in paths if Path(path).exists()]
    if not existing:
        return 0
    # One du over every path counts each hard-linked file once.
    output = subprocess.run(["du", "-skc", *existing], capture_output=True, text=True).stdout
    return int(output.strip().splitlines()[-1].split()[0])


def library_roots(worktree):
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
            cwd=worktree, check=True, capture_output=True, text=True,
        ).stdout
    )
    roots = {}
    for package in metadata["packages"]:
        for target in package["targets"]:
            if "lib" in target["kind"]:
                roots[package["name"]] = Path(target["src_path"]).relative_to(worktree)
    return roots


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def latency_summary(values):
    if not values:
        return None
    return {
        "median": round(statistics.median(values), 3),
        "p95": round(percentile(values, 0.95), 3),
        "max": round(max(values), 3),
    }


def build_events(home):
    # Reading the structured events avoids parsing rounded, human-facing CLI
    # output and counts one shared build once, however many clients it served.
    events = []
    for name in ("events.jsonl.1", "events.jsonl"):
        path = home / name
        if path.exists():
            events.extend(json.loads(line)["kind"] for line in path.read_text().splitlines())
    return events


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repository", required=True, type=Path)
    parser.add_argument("--base", required=True, help="commit every worktree starts from")
    parser.add_argument("--workdir", required=True, type=Path, help="where the worktrees live")
    parser.add_argument("--mode", required=True, choices=["buildd", "cargo"])
    parser.add_argument("--sessions", type=int, required=True)
    parser.add_argument("--iterations", type=int, default=4)
    parser.add_argument("--crates", required=True, help="comma-separated packages, assigned round robin")
    parser.add_argument("--think", default="5-15", help="seconds between iterations, MIN-MAX")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--buildd", default="buildd", help="the buildd executable")
    parser.add_argument("--buildd-home", type=Path, help="buildd home for --mode buildd")
    parser.add_argument("--slots", type=int, default=2)
    parser.add_argument("--jobs", type=int, default=os.cpu_count())
    parser.add_argument("--disk-gib", type=float, help="build disk all slots share (20 GiB per slot)")
    parser.add_argument("--memory-gib", type=float, help="memory admission budget")
    parser.add_argument("--unmeasured-memory-gib", type=float, help="reservation per unmeasured build; omitted learns alone")
    parser.add_argument("--report", type=Path, required=True)
    options = parser.parse_args()
    if options.mode == "buildd" and options.buildd_home is None:
        parser.error("--mode buildd needs --buildd-home")
    if options.mode == "buildd":
        executable = shutil.which(options.buildd)
        if executable is None:
            parser.error(f"buildd executable not found: {options.buildd}")
        options.buildd = str(Path(executable).resolve())
    options.repository = options.repository.resolve()
    if options.sessions < 1 or options.iterations < 1:
        parser.error("sessions and iterations must be positive")
    think_min, think_max = (float(part) for part in options.think.split("-"))
    if not (0 <= think_min <= think_max and math.isfinite(think_max)):
        parser.error("think times must be finite, non-negative and ordered")
    crates = options.crates.split(",")
    base = git(options.repository, "rev-parse", f"{options.base}^{{commit}}").strip()

    # Every run has fresh worktrees and caches. Refuse existing data rather than
    # resetting somebody's checkout or mixing results with earlier daemon events.
    for index in range(options.sessions):
        if (options.workdir / f"s{index + 1:02}").exists():
            parser.error("workdir contains an existing session; use a fresh directory")
    if options.mode == "buildd" and options.buildd_home.exists() and any(options.buildd_home.iterdir()):
        parser.error("buildd-home must be empty; use a fresh directory")
    options.workdir.mkdir(parents=True, exist_ok=True)
    worktrees = []
    for index in range(options.sessions):
        worktree = (options.workdir / f"s{index + 1:02}").resolve()
        git(options.repository, "worktree", "add", "-q", "--detach", str(worktree), base)
        worktrees.append(worktree)
    roots = library_roots(worktrees[0])
    for crate in crates:
        if crate not in roots:
            sys.exit(f"{crate} has no library target")

    daemon = None
    environment = dict(os.environ)
    environment.pop("CARGO_TARGET_DIR", None)
    if options.mode == "buildd":
        home = options.buildd_home.resolve()
        home.mkdir(parents=True, exist_ok=True)
        disk_gib = options.disk_gib if options.disk_gib is not None else 20 * options.slots
        config = f"slots = {options.slots}\njobs = {options.jobs}\ndisk_gib = {disk_gib}\n"
        for name in ("memory_gib", "unmeasured_memory_gib"):
            if (value := getattr(options, name)) is not None:
                config += f"{name} = {value}\n"
        (home / "config.toml").write_text(config)
        environment["BUILDD_HOME"] = str(home)
        log = open(home / "daemon.log", "a")
        daemon = subprocess.Popen([options.buildd, "daemon"], env=environment, stdout=log, stderr=log)
        deadline = time.time() + 10
        while not (home / "sock").exists():
            if time.time() > deadline or daemon.poll() is not None:
                sys.exit("the buildd daemon did not start")
            time.sleep(0.05)
        disk_paths = [home / "slots"]
    else:
        disk_paths = [worktree / "target" for worktree in worktrees]

    results = []
    lock = threading.Lock()
    done = threading.Event()
    disk_samples = []

    def sample_disk():
        while not done.is_set():
            disk_samples.append((time.time(), disk_kib(disk_paths)))
            done.wait(5)

    def session(index, worktree):
        rng = random.Random(options.seed * 1000 + index)
        session_environment = dict(environment, BUILDD_LABEL=f"session-{index + 1:02}")
        crate = crates[index % len(crates)]
        root = worktree / roots[crate]
        for iteration in range(options.iterations):
            time.sleep(rng.uniform(think_min, think_max))
            probe = (index + 1) * 1000 + iteration
            with open(root, "a") as source:
                source.write(f"\n#[allow(dead_code)]\nfn buildd_load_probe_{probe}() -> u32 {{\n    {probe}\n}}\n")
            for operation in (["check", "-p", crate], ["test", "-p", crate, "--no-run"]):
                command = [options.buildd, *operation] if options.mode == "buildd" else ["cargo", *operation]
                started = time.monotonic()
                completed = subprocess.run(command, cwd=worktree, env=session_environment, capture_output=True, text=True)
                wall = time.monotonic() - started
                with lock:
                    results.append({
                        "session": index + 1,
                        "crate": crate,
                        "iteration": iteration,
                        "operation": operation[0],
                        "started_s": started - began,
                        "wall": wall,
                        "exit": completed.returncode,
                        "error": completed.stderr[-2000:] if completed.returncode else None,
                    })

    sampler = threading.Thread(target=sample_disk)
    sampler.start()
    began = time.monotonic()
    threads = [threading.Thread(target=session, args=(index, worktree)) for index, worktree in enumerate(worktrees)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    elapsed = time.monotonic() - began
    done.set()
    sampler.join()
    if daemon is not None:
        status = subprocess.run([options.buildd, "status"], env=environment, capture_output=True, text=True).stdout
        daemon.terminate()
        daemon.wait()
        log.close()
        events = build_events(home)
    else:
        status = ""
        events = []
    final_disk = disk_kib(disk_paths)

    walls = [result["wall"] for result in results]
    finished = [event for event in events if event["type"] == "finished"]
    completed = [event for event in finished if event["outcome"]["kind"] == "exited"]
    failed = sum(result["exit"] != 0 for result in results)
    expected = options.sessions * options.iterations * 2
    requested = [event for event in events if event["type"] == "requested"]
    shared = sum(event["shared"] for event in requested)
    events_complete = len(requested) == len(results) == len(finished) + shared
    summary = {
        "mode": options.mode,
        "source_commit": base,
        "crates": crates,
        "seed": options.seed,
        "think_s": [think_min, think_max],
        "initial_build_cache": "empty (OS and download caches retained)",
        "jobs": options.jobs if options.mode == "buildd" else None,
        "sessions": options.sessions,
        "iterations": options.iterations,
        "slots": options.slots if options.mode == "buildd" else None,
        "requests": len(results),
        "expected_requests": expected,
        "failures": failed,
        "valid": len(results) == expected and failed == 0 and (options.mode != "buildd" or events_complete),
        "elapsed_s": round(elapsed, 3),
        "successful_results_per_minute": round((len(results) - failed) * 60 / elapsed, 3),
        "latency_s": latency_summary(walls),
        "latency_by_operation_s": {
            operation: latency_summary([r["wall"] for r in results if r["operation"] == operation])
            for operation in ("check", "test")
        },
        "daemon": {
            "binary_sha256": hashlib.sha256(Path(options.buildd).read_bytes()).hexdigest(),
            "config": config,
            "events_complete": events_complete,
            "completed_jobs": len(completed),
            "completed_jobs_per_minute": round(len(completed) * 60 / elapsed, 3),
            "failed_jobs": sum(event["outcome"]["code"] != 0 for event in completed),
            "interrupted_jobs": len(finished) - len(completed),
            "shared_requests": shared,
            "queue_s": latency_summary([event["queued_ms"] / 1000 for event in finished]),
            "largest_job_peak_gib": max(
                (event["usage"]["peak_memory"] / 2**30 for event in finished if event["usage"] is not None),
                default=None,
            ),
        } if options.mode == "buildd" else None,
        "peak_disk_gib": round(max(final_disk, max((kib for _, kib in disk_samples), default=0)) / 2**20, 1),
        "final_disk_gib": round(final_disk / 2**20, 1),
        "status": status,
    }
    options.report.write_text(json.dumps({"summary": summary, "results": results, "disk": disk_samples, "events": events}, indent=1))
    for key, value in summary.items():
        if key != "status":
            print(f"{key:24} {value}")
    if status:
        print(status, end="")
    for result in results:
        if result["exit"] != 0:
            print(f"FAILED session {result['session']} {result['operation']} -p {result['crate']}:\n{result['error']}")
    return 0 if summary["valid"] else 1


if __name__ == "__main__":
    sys.exit(main())
