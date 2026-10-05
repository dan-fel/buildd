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
import json
import os
import random
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

BUILDD_TIMES = re.compile(r"after (\d+\.\d) s in the build and (\d+\.\d) s in the queue")


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
    return ordered[min(len(ordered) - 1, int(fraction * len(ordered)))]


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
    parser.add_argument("--slot-limit-gib", type=float, default=20)
    parser.add_argument("--report", type=Path, required=True)
    options = parser.parse_args()
    if options.mode == "buildd" and options.buildd_home is None:
        parser.error("--mode buildd needs --buildd-home")
    think_min, think_max = (float(part) for part in options.think.split("-"))
    crates = options.crates.split(",")

    # Worktrees at BASE, with no edits from an earlier run.
    options.workdir.mkdir(parents=True, exist_ok=True)
    worktrees = []
    for index in range(options.sessions):
        worktree = (options.workdir / f"s{index + 1:02}").resolve()
        if not worktree.exists():
            git(options.repository, "worktree", "add", "-q", "--detach", str(worktree), options.base)
        git(worktree, "checkout", "-q", "--detach", options.base)
        git(worktree, "reset", "-q", "--hard", options.base)
        git(worktree, "clean", "-q", "-fd")
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
        (home / "config.toml").write_text(
            f"slots = {options.slots}\njobs = {options.jobs}\nslot_limit_gib = {options.slot_limit_gib}\n"
        )
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
                started = time.time()
                completed = subprocess.run(command, cwd=worktree, env=session_environment, capture_output=True, text=True)
                wall = time.time() - started
                times = BUILDD_TIMES.search(completed.stderr)
                with lock:
                    results.append({
                        "session": index + 1,
                        "crate": crate,
                        "iteration": iteration,
                        "operation": operation[0],
                        "started": started,
                        "wall": wall,
                        "exit": completed.returncode,
                        "build": float(times.group(1)) if times else None,
                        "queue": float(times.group(2)) if times else None,
                        "error": completed.stderr[-2000:] if completed.returncode else None,
                    })

    sampler = threading.Thread(target=sample_disk)
    sampler.start()
    began = time.time()
    threads = [threading.Thread(target=session, args=(index, worktree)) for index, worktree in enumerate(worktrees)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    elapsed = time.time() - began
    done.set()
    sampler.join()
    if daemon is not None:
        status = subprocess.run([options.buildd, "status"], env=environment, capture_output=True, text=True).stdout
        daemon.terminate()
        daemon.wait()
    else:
        status = ""
    final_disk = disk_kib(disk_paths)

    walls = [result["wall"] for result in results]
    summary = {
        "mode": options.mode,
        "sessions": options.sessions,
        "iterations": options.iterations,
        "slots": options.slots if options.mode == "buildd" else None,
        "requests": len(results),
        "failures": sum(1 for result in results if result["exit"] != 0),
        "elapsed_s": round(elapsed, 1),
        "latency_s": {
            "median": round(statistics.median(walls), 1),
            "p90": round(percentile(walls, 0.9), 1),
            "max": round(max(walls), 1),
        },
        "latency_by_operation_s": {
            operation: round(statistics.median([r["wall"] for r in results if r["operation"] == operation]), 1)
            for operation in ("check", "test")
        },
        "queue_total_s": round(sum(r["queue"] or 0 for r in results), 1) if options.mode == "buildd" else None,
        "peak_disk_gib": round(max(kib for _, kib in disk_samples) / 2**20, 1) if disk_samples else None,
        "final_disk_gib": round(final_disk / 2**20, 1),
        "status": status,
    }
    options.report.write_text(json.dumps({"summary": summary, "results": results, "disk": disk_samples}, indent=1))
    for key, value in summary.items():
        if key != "status":
            print(f"{key:24} {value}")
    if status:
        print(status, end="")
    for result in results:
        if result["exit"] != 0:
            print(f"FAILED session {result['session']} {result['operation']} -p {result['crate']}:\n{result['error']}")


if __name__ == "__main__":
    main()
