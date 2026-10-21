#!/usr/bin/env python3
"""Benchmark every deemo command; the artifact the optimization work is
measured against.

    scripts/bench.py --label before            # build, verify, measure, bench/before.json
    scripts/bench.py --label after --diff before
    scripts/bench.py --only pipe,kill --reps 20
    scripts/bench.py --list

Rules (see .pi/bench-brief.md §2): every scenario is a black-box run of the
release binary against a throwaway $DEEMO_HOME; a repetition is scaled to
~0.3 s where the command is scale-bound, and floor-bound commands run N
invocations per repetition and divide. Reported numbers are whole ms per
invocation (min / median / p90) plus a unit rate, so a win is visible as
either seconds shaved or throughput gained.

`--verify` (on by default) runs the end-to-end checks that pin the behaviour
the optimizations must not break: byte-exact passthrough and log, a
never-flushed tail, `| head` exit 0, log realtime-ness, and `kill --dry-run`
agreeing with lsof on the binding side of a port.
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO / "target" / "release" / "deemo"
BENCH_DIR = REPO / "bench"

DEFAULT_REPS = 10
WARMUP = 1

# One repetition of a stream scenario: ~15 MB / 200k lines, which the
# unoptimized pump takes ~0.4 s to move.
BIG_LINES = 200_000
# ps fixtures: the live half reuses a pool of real `sleep` processes; the
# per-entry liveness probe cost is identical to a distinct pid per row and
# 2000 live sleeps would only measure the fixture's memory.
LIVE_POOL = 128

DEVNULL = subprocess.DEVNULL
PIPE = subprocess.PIPE


class BenchError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# shared fixture: the big stream file
# --------------------------------------------------------------------------


def build_big_file(path: Path) -> None:
    with open(path, "wb") as f:
        for i in range(BIG_LINES):
            f.write(
                f"{i:07d} INFO worker req={i:08x} status=200 bytes=1024 "
                f"latency=12ms ok\n".encode()
            )


SHARED = Path(tempfile.mkdtemp(prefix="deemo-bench-shared-"))
BIG = SHARED / "big.log"
TAIL = SHARED / "tail.log"  # a stream whose last line has no newline


def build_shared_fixture() -> None:
    build_big_file(BIG)
    (TAIL).write_bytes(b"first\nsecond\nlast line without newline")


# --------------------------------------------------------------------------
# per-scenario fixture context
# --------------------------------------------------------------------------


class Ctx:
    """Throwaway $DEEMO_HOME plus the child processes of one scenario."""

    def __init__(self, name: str, bin_path: Path):
        self.name = name
        self.root = Path(tempfile.mkdtemp(prefix=f"deemo-bench-{name}-"))
        self.home = self.root / "home"
        self.home.mkdir()
        self.bin = bin_path
        self.procs = []

    def env(self, **extra) -> dict:
        env = {
            k: v
            for k, v in os.environ.items()
            if k not in ("DEEMO_HOME", "COLUMNS", "DEEMO_BENCH")
        }
        env["DEEMO_HOME"] = str(self.home)
        env["DEEMO_BENCH"] = "1"
        env.update(extra)
        return env

    def argv(self, *args) -> list:
        return [str(self.bin), *map(str, args)]

    def spawn(self, argv, **kw):
        p = subprocess.Popen(argv, **kw)
        self.procs.append(p)
        return p

    def logs_dir(self) -> Path:
        d = self.home / "logs"
        d.mkdir(exist_ok=True)
        return d

    def run_dir(self) -> Path:
        d = self.home / "run"
        d.mkdir(exist_ok=True)
        return d

    def clear_logs(self) -> None:
        for p in self.logs_dir().glob("*"):
            p.unlink()

    def teardown(self) -> None:
        for p in self.procs:
            try:
                if p.poll() is None:
                    p.kill()
            except OSError:
                pass
        for p in self.procs:
            try:
                p.wait(timeout=5)
            except Exception:
                pass
        shutil.rmtree(self.root, ignore_errors=True)


def timed_deemo(ctx: Ctx, args, *, expect=(0,), stdin=None, env_extra=None,
                stdout=PIPE, timeout=300):
    """One timed run of `deemo <args>`; asserts the exit code."""
    t0 = time.perf_counter()
    p = subprocess.run(
        ctx.argv(*args),
        stdin=stdin,
        stdout=stdout,
        stderr=PIPE,
        env=ctx.env(**(env_extra or {})),
        timeout=timeout,
    )
    dt = time.perf_counter() - t0
    if p.returncode not in expect:
        raise BenchError(
            f"deemo {' '.join(map(str, args))} -> exit {p.returncode} "
            f"(expected {expect}); stderr tail: {p.stderr[-400:]!r}"
        )
    return dt, p


def timed_batch(ctx: Ctx, args, n, **kw) -> float:
    """N invocations, total elapsed. Floor-bound commands divide by N."""
    t0 = time.perf_counter()
    for _ in range(n):
        timed_deemo(ctx, args, **kw)
    return time.perf_counter() - t0


# --------------------------------------------------------------------------
# fixture helpers
# --------------------------------------------------------------------------


def write_reg(ctx: Ctx, label: str, pid: int, started: str | None = None,
              cmd: str = "sleep 3600") -> Path:
    started = started or time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime())
    path = ctx.run_dir() / f"{label}.{pid}.pid"
    path.write_text(
        f"pid={pid}\nlabel={label}\nstarted={started}\ncmd={cmd}\n"
        f"log={ctx.home}/logs/{label}.log\n"
    )
    return path


def dead_pids(n: int) -> list[int]:
    """Pids that no process owns right now (high, unallocated range)."""
    out = []
    for pid in range(60_000, 65_536):
        if len(out) == n:
            break
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            out.append(pid)
        except PermissionError:
            continue
    if len(out) < n:
        raise BenchError("not enough unused pids for the dead half of the fixture")
    return out


def reap_in_background(p) -> None:
    """Reap like a normal supervisor would: an exited fixture child must not
    sit around as a zombie, or `deemo kill`/`stop` would measure the parent's
    reaping policy instead of deemo's signal-to-exit detection."""
    threading.Thread(target=p.wait, daemon=True).start()


def start_sleep_group(ctx: Ctx) -> int:
    p = ctx.spawn(
        ["sleep", "3600"], stdout=DEVNULL, stderr=DEVNULL, start_new_session=True
    )
    reap_in_background(p)
    return p.pid


def registry_fixture(ctx: Ctx, live: int, dead: int) -> None:
    """`live` + `dead` registrations, plus the pool of real pids behind them."""
    pool = [start_sleep_group(ctx) for _ in range(min(live, LIVE_POOL))]
    now = time.time()
    stamp = lambda i: time.strftime(
        "%Y-%m-%dT%H:%M:%S", time.localtime(now - i * 61)
    )
    for i in range(live):
        write_reg(
            ctx,
            f"svc{i:05d}",
            pool[i % len(pool)],
            stamp(i),
            f"node server{i}.mjs --port {3000 + i % 500}",
        )
    for i, pid in enumerate(dead_pids(dead)):
        write_reg(ctx, f"old{i:05d}", pid, stamp(dead - i), f"./bin/worker{i}")


def free_ports(n: int) -> list[int]:
    """Ports nobody holds on any protocol or address.

    A port can be free for TCP/127.0.0.1 and still be held by a UDP or
    wildcard socket, which `kill` would legitimately report; lsof is the
    independent oracle for "free"."""
    ports: list[int] = []
    while len(ports) < n:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        finally:
            s.close()
        if port < 1024 or port in ports:
            continue
        held = subprocess.run(
            ["lsof", "-nP", "-F", "pn", "-i", f":{port}"],
            stdout=PIPE, stderr=DEVNULL,
        ).stdout
        if not held.strip():
            ports.append(port)
    return ports


def wait_port(port: int, host: str = "127.0.0.1", deadline: float = 15.0) -> None:
    end = time.time() + deadline
    while time.time() < end:
        try:
            with socket.create_connection((host, port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.01)
    raise BenchError(f"holder on {host}:{port} never came up")


def start_holder(ctx: Ctx, port: int):
    docroot = ctx.root / "www"
    docroot.mkdir(exist_ok=True)
    p = ctx.spawn(
        [
            sys.executable,
            "-m",
            "http.server",
            str(port),
            "--bind",
            "127.0.0.1",
            "--directory",
            str(docroot),
        ],
        stdout=DEVNULL,
        stderr=DEVNULL,
    )
    wait_port(port)
    reap_in_background(p)
    return p


def wait_exit(procs, timeout: float = 15.0) -> None:
    end = time.time() + timeout
    for p in procs:
        left = max(0.05, end - time.time())
        try:
            p.wait(timeout=left)
        except subprocess.TimeoutExpired:
            raise BenchError(f"process {p.args} did not exit")


def log_path_from_stderr(ctx: Ctx, stderr: bytes) -> Path:
    for line in stderr.decode(errors="replace").splitlines():
        marker = "deemo: log: "
        if line.startswith(marker):
            return Path(line[len(marker):])
    raise BenchError(f"no log path announced; stderr={stderr[-400:]!r}")


def wait_settled(path: Path, size: int, timeout: float = 30.0) -> None:
    """Poll until the log holds the whole stream (the pump wrote EOF through)."""
    end = time.time() + timeout
    while time.time() < end:
        if path.exists() and path.stat().st_size >= size:
            return
        time.sleep(0.001)
    raise BenchError(f"{path} never reached {size} bytes")


# --------------------------------------------------------------------------
# scenarios
# --------------------------------------------------------------------------


class Scn:
    def __init__(self, name, input, agg=1, reps=DEFAULT_REPS, setup=None,
                 rep=None, rate=None):
        self.name = name
        self.input = input
        self.agg = agg
        self.reps = reps
        self.setup = setup
        self.rep = rep
        self.rate = rate


def mbps(nbytes: int):
    return lambda ms: f"{nbytes / (ms / 1000) / 1e6:5.0f} MB/s"


def lines_per_s(nlines: int):
    return lambda ms: f"{nlines / (ms / 1000) / 1e3:6.0f}k lines/s"


def us_per(unit: int, label: str):
    return lambda ms: f"{ms * 1000 / unit:6.0f} µs/{label}"


def per_s(label: str = "inv"):
    return lambda ms: f"{1 / (ms / 1000):6.0f} {label}/s"


def scenarios() -> list[Scn]:
    out: list[Scn] = []

    # --- floor ------------------------------------------------------------
    def bare(ctx: Ctx) -> float:
        # Bare `deemo` exits 2 only when stdin is a terminal (otherwise it is
        # pipe mode), so the floor needs a real pty on stdin.
        master, slave = os.openpty()
        try:
            return timed_batch(ctx, [], 100, expect=(2,), stdin=slave,
                               stdout=DEVNULL)
        finally:
            os.close(slave)
            os.close(master)

    out.append(Scn("floor.bare", "1 inv, aggregated x100, stdin=tty", agg=100,
                   rep=bare, rate=per_s()))
    out.append(
        Scn(
            "floor.true",
            "1 inv, aggregated x100",
            agg=100,
            rep=lambda c: timed_batch(c, [], 100, stdin=DEVNULL, stdout=DEVNULL),
            rate=per_s(),
        )
    )
    # `deemo` with no argv is the floor for deemo; /usr/bin/true is the floor
    # for the machine. Both are timed through the same Python runner.

    out.append(
        Scn(
            "cli.help",
            "1 inv, aggregated x75",
            agg=75,
            rep=lambda c: timed_batch(c, ["--help"], 75, stdin=DEVNULL,
                                      stdout=DEVNULL),
        )
    )
    out.append(
        Scn(
            "cli.version",
            "1 inv, aggregated x75",
            agg=75,
            rep=lambda c: timed_batch(c, ["--version"], 75, stdin=DEVNULL,
                                      stdout=DEVNULL),
        )
    )

    # --- ps ---------------------------------------------------------------
    def ps_rep(verbose=False, columns=None):
        def rep(ctx: Ctx) -> float:
            extra = {"COLUMNS": str(columns)} if columns else {}
            dt, _ = timed_deemo(ctx, ["ps"] + (["-v"] if verbose else []),
                                env_extra=extra)
            return dt

        return rep

    def ps_setup(live, dead):
        return lambda c: registry_fixture(c, live, dead)

    entries = lambda n: f"{n} live + {n} dead registrations"
    entries_rate = lambda n: us_per(n * 2, "entry")
    for n in (200, 2000):
        out.append(
            Scn(f"ps.{n}", entries(n), setup=ps_setup(n, n), rep=ps_rep(),
                rate=entries_rate(n))
        )
    out.append(
        Scn("ps.v2000", entries(2000), setup=ps_setup(2000, 2000),
            rep=ps_rep(verbose=True), rate=entries_rate(2000))
    )
    out.append(
        Scn("ps.narrow", entries(2000) + ", COLUMNS=40",
            setup=ps_setup(2000, 2000), rep=ps_rep(columns=40),
            rate=entries_rate(2000))
    )
    out.append(
        Scn(
            "ps.empty",
            "1 inv, aggregated x100",
            agg=100,
            rep=lambda c: timed_batch(c, ["ps"], 100, expect=(1,),
                                      stdin=DEVNULL),
        )
    )

    # --- logs -------------------------------------------------------------
    def logs_setup(ctx: Ctx) -> None:
        d = ctx.logs_dir()
        # Timestamped names, the default scheme: 10 of the label, 190 others.
        now = time.time()
        for i in range(200):
            label = "srv" if i < 10 else f"app{i:03d}"
            name = f"{label}-{time.strftime('%Y%m%d-%H%M%S', time.localtime(now - i * 60))}.{i:03d}.log"
            (d / name).write_bytes(b"log line\n")
        (d / "srv-20260101-000000.000.log").write_bytes(
            b"the newest srv log\nsecond line\n"
        )

    out.append(
        Scn("logs.hit", "201 log files", agg=100, setup=logs_setup,
            rep=lambda c: timed_batch(c, ["logs", "srv"], 100),
            rate=per_s())
    )
    out.append(
        Scn("logs.miss", "201 log files", agg=100, setup=logs_setup,
            rep=lambda c: timed_batch(c, ["logs", "nope"], 100, expect=(1,)),
            rate=per_s())
    )
    out.append(
        Scn(
            "logs.big",
            "10 MB log, 4 KiB tail",
            agg=25,
            setup=lambda c: (c.logs_dir() / "big.log").write_bytes(
                b"0123456789abcdef" * (10 * 1024 * 1024 // 16)
            ),
            rep=lambda c: timed_batch(c, ["logs", "big"], 25),
            rate=per_s(),
        )
    )

    # --- stop -------------------------------------------------------------
    out.append(
        Scn(
            "stop.dead",
            "400 live registrations, absent label, aggregated x20",
            agg=20,
            setup=lambda c: registry_fixture(c, 400, 0),
            rep=lambda c: timed_batch(c, ["stop", "nope"], 20, expect=(1,)),
        )
    )

    def stop_groups(k):
        def rep(ctx: Ctx) -> float:
            for old in ctx.run_dir().glob("sa.*.pid"):
                old.unlink()
            for _ in range(k):
                write_reg(ctx, "sa", start_sleep_group(ctx), cmd="sleep 3600")
            dt, _ = timed_deemo(ctx, ["stop", "sa"])
            return dt

        return Scn("stop.alive", f"{k} live groups under one label", rep=rep,
                   rate=us_per(k, "stop"))

    out.append(stop_groups(6))

    def stop_by_pid(k):
        def rep(ctx: Ctx) -> float:
            for old in ctx.run_dir().glob("sp*.pid"):
                old.unlink()
            pids = []
            for i in range(k):
                pid = start_sleep_group(ctx)
                write_reg(ctx, f"sp{i}", pid, cmd="sleep 3600")
                pids.append(pid)
            dt, _ = timed_deemo(ctx, ["stop", *pids])
            return dt

        return Scn("stop.pid", f"{k} live pids in one invocation", rep=rep,
                   rate=us_per(k, "stop"))

    out.append(stop_by_pid(6))

    # --- kill -------------------------------------------------------------
    KILL_PORTS = 10

    def kill_free_setup(ctx: Ctx) -> None:
        ctx.kill_ports = free_ports(KILL_PORTS)  # type: ignore[attr-defined]

    out.append(
        Scn(
            "kill.free",
            f"{KILL_PORTS} free ports, --dry-run",
            setup=kill_free_setup,
            rep=lambda c: timed_deemo(
                c, ["kill", *c.kill_ports, "--dry-run"], expect=(1,)
            )[0],
            rate=us_per(KILL_PORTS, "port"),
        )
    )

    def kill_held_setup(ctx: Ctx) -> None:
        ctx.kill_port = free_ports(1)[0]  # type: ignore[attr-defined]
        start_holder(ctx, ctx.kill_port)

    out.append(
        Scn(
            "kill.held",
            "1 python http.server holder, --dry-run, aggregated x12",
            agg=12,
            setup=kill_held_setup,
            rep=lambda c: timed_batch(
                c, ["kill", c.kill_port, "--dry-run"], 12
            ),
            rate=per_s(),
        )
    )

    def kill_held_real(ctx: Ctx) -> float:
        port = free_ports(1)[0]
        holder = start_holder(ctx, port)
        dt, _ = timed_deemo(ctx, ["kill", port])
        wait_exit([holder])
        return dt

    out.append(
        Scn("kill.held.real", "1 python http.server holder, real kill",
            rep=kill_held_real, rate=per_s("kill"))
    )

    def kill_many(ctx: Ctx) -> float:
        ports = free_ports(5)
        holders = [start_holder(ctx, p) for p in ports]
        dt, _ = timed_deemo(ctx, ["kill", *ports])
        wait_exit(holders)
        return dt

    out.append(
        Scn("kill.many", "5 held ports, real kill in one invocation",
            rep=kill_many, rate=us_per(5, "kill"))
    )

    # --- detach -----------------------------------------------------------
    out.append(
        Scn(
            "detach.true",
            "1 inv, aggregated x50",
            agg=50,
            rep=lambda c: timed_batch(c, ["--", "true"], 50),
        )
    )

    def detach_keep_setup(ctx: Ctx) -> None:
        ctx.keep_names = [  # type: ignore[attr-defined]
            f"k-{time.strftime('%Y%m%d-%H%M%S', time.localtime(time.time() - i * 3600))}.{i:03d}.log"
            for i in range(3000)
        ]
        recreate_keep(ctx)

    def recreate_keep(ctx: Ctx) -> None:
        d = ctx.logs_dir()
        for name in ctx.keep_names:  # type: ignore[attr-defined]
            (d / name).write_bytes(b"x\n")

    def detach_keep(ctx: Ctx) -> float:
        recreate_keep(ctx)
        dt, _ = timed_deemo(ctx, ["--keep", "5", "true"])
        return dt

    out.append(
        Scn("detach.keep", "3000 log files of the label, --keep 5",
            setup=detach_keep_setup, rep=detach_keep)
    )

    # --- foreground -------------------------------------------------------
    def fg(args):
        def rep(ctx: Ctx) -> float:
            ctx.clear_logs()
            dt, _ = timed_deemo(ctx, ["-F", *args, "cat", str(BIG)])
            return dt

        return rep

    out.append(
        Scn("fg.lines", f"{BIG_LINES} lines / {BIG.stat().st_size >> 20} MB",
            rep=fg([]), rate=lines_per_s(BIG_LINES))
    )
    out.append(
        Scn("fg.quiet", f"{BIG_LINES} lines / {BIG.stat().st_size >> 20} MB",
            rep=fg(["--quiet"]), rate=lines_per_s(BIG_LINES))
    )

    # --- pipe -------------------------------------------------------------
    def pipe_rep(ctx: Ctx, args) -> float:
        ctx.clear_logs()
        t0 = time.perf_counter()
        cat, deemo = cat_pipe(ctx, args)
        deemo.communicate()
        cat.wait()
        dt = time.perf_counter() - t0
        if deemo.returncode != 0:
            raise BenchError(f"deemo {' '.join(args)} -> exit {deemo.returncode}")
        return dt

    out.append(
        Scn("pipe.lines", f"cat {BIG.name} | deemo, {BIG_LINES} lines",
            rep=lambda c: pipe_rep(c, []), rate=lines_per_s(BIG_LINES))
    )
    out.append(
        Scn("pipe.yolo", f"cat {BIG.name} | deemo --yolo, {BIG_LINES} lines",
            rep=lambda c: pipe_rep(c, ["--yolo"]), rate=lines_per_s(BIG_LINES))
    )
    out.append(
        Scn("pipe.quiet", f"cat {BIG.name} | deemo --quiet, {BIG_LINES} lines",
            rep=lambda c: pipe_rep(c, ["--quiet"]), rate=lines_per_s(BIG_LINES))
    )

    def pipe_bg(ctx: Ctx) -> float:
        ctx.clear_logs()
        t0 = time.perf_counter()
        cat, deemo = cat_pipe(ctx, ["-B", "--label", f"pbg{time.time_ns()}"])
        _, err = deemo.communicate()
        cat.wait()
        if deemo.returncode != 0:
            raise BenchError(f"deemo -B -> exit {deemo.returncode}")
        log = log_path_from_stderr(ctx, err)
        wait_settled(log, BIG.stat().st_size)
        return time.perf_counter() - t0

    out.append(
        Scn("pipe.bg", f"cat {BIG.name} | deemo -B, until log settles",
            reps=5, rep=pipe_bg, rate=lines_per_s(BIG_LINES))
    )

    def pipe_head(ctx: Ctx) -> float:
        ctx.clear_logs()
        t0 = time.perf_counter()
        head = ctx.spawn(["head", "-1"], stdin=PIPE, stdout=PIPE, stderr=DEVNULL)
        cat, deemo = cat_pipe(ctx, ["--label", "ph"], stdout=head.stdin)
        head.stdin.close()
        _, derr = deemo.communicate()
        head.communicate()
        cat.wait()
        dt = time.perf_counter() - t0
        if deemo.returncode != 0:
            raise BenchError(f"deemo | head -> exit {deemo.returncode}")
        if b"error" in derr.lower():
            raise BenchError(f"broken pipe must be silent, got {derr[-300:]!r}")
        return dt

    out.append(
        Scn("pipe.head", f"cat {BIG.name} | deemo | head -1", reps=5,
            rep=pipe_head)
    )

    # --- machine references ----------------------------------------------
    out.append(
        Scn(
            "ref.cat",
            f"cat {BIG.name} > /dev/null, aggregated x5",
            agg=5,
            rep=lambda c: timed_batch_bin(
                ["cat", str(BIG)], 5, stdout=DEVNULL
            ),
            rate=mbps(BIG.stat().st_size),
        )
    )

    def lsof_setup(ctx: Ctx) -> None:
        ctx.lsof_port = free_ports(1)[0]  # type: ignore[attr-defined]

    out.append(
        Scn(
            "ref.lsof",
            "lsof -nP -F pn -i :PORT, free port, aggregated x12",
            agg=12,
            setup=lsof_setup,
            rep=lambda c: timed_batch_bin(
                ["lsof", "-nP", "-F", "pn", "-i", f":{c.lsof_port}"],
                12,
                expect=(0, 1),
            ),
            rate=per_s(),
        )
    )

    return out


def cat_pipe(ctx: Ctx, args, *, stdout=PIPE, source=BIG):
    """`cat SOURCE | deemo args` over a real pipe, parent copies closed."""
    cat = ctx.spawn(["cat", str(source)], stdout=PIPE, stderr=DEVNULL)
    deemo = ctx.spawn(ctx.argv(*args), stdin=cat.stdout, stdout=stdout,
                      stderr=PIPE, env=ctx.env())
    cat.stdout.close()
    return cat, deemo


def timed_batch_bin(argv, n, *, expect=(0,), stdout=PIPE) -> float:
    t0 = time.perf_counter()
    for _ in range(n):
        p = subprocess.run(argv, stdout=stdout, stderr=DEVNULL, stdin=DEVNULL)
        if p.returncode not in expect:
            raise BenchError(f"{argv} -> exit {p.returncode}")
    return time.perf_counter() - t0


# --------------------------------------------------------------------------
# end-to-end checks: behaviour the optimization must not break
# --------------------------------------------------------------------------


def verify(bin_path: Path) -> list[dict]:
    checks: list[dict] = []

    def check(name, fn):
        try:
            detail = fn() or ""
            checks.append({"name": name, "ok": True, "detail": detail})
        except Exception as e:  # noqa: BLE001 - a failed check is data
            checks.append({"name": name, "ok": False, "detail": str(e)})

    ctx = Ctx("verify", bin_path)
    try:
        big = BIG.read_bytes()

        def pipe_run(label, args=(), source=BIG):
            """One pipe-mode run: (stdout, log bytes or None)."""
            ctx.clear_logs()
            with open(source, "rb") as stdin:
                _, p = timed_deemo(ctx, ["--label", label, *args], stdin=stdin)
            log = ctx.logs_dir() / f"{label}.log"
            return p.stdout, (log.read_bytes() if log.exists() else None)

        def pipe_byte_exact():
            out, log = pipe_run("chk", ["--timestamp", "off"])
            assert out == big, "passthrough is not byte-exact"
            assert log == big, "log is not byte-exact"
            return f"{len(big)} bytes through and logged"

        def pipe_unterminated_tail():
            out, log = pipe_run("tail", ["--timestamp", "off"], source=TAIL)
            want = TAIL.read_bytes()
            assert out == want, "passthrough dropped or altered the tail"
            assert log == want, "log dropped or altered the tail"
            return "final line without newline kept verbatim"

        def fg_byte_exact():
            ctx.clear_logs()
            _, p = timed_deemo(ctx, ["-F", "--label", "fgc", "--timestamp", "off",
                                     "cat", str(BIG)])
            assert p.stdout == big, "foreground passthrough is not byte-exact"
            assert (ctx.logs_dir() / "fgc.log").read_bytes() == big, \
                "foreground log is not byte-exact"
            return "stdout and stderr merged, 15 MB through and logged"

        def fg_quiet():
            ctx.clear_logs()
            _, p = timed_deemo(ctx, ["-F", "--quiet", "--label", "fq",
                                     "--timestamp", "off", "sh", "-c", "echo hi"])
            assert p.stdout == b"", f"--quiet echoed {p.stdout!r}"
            assert (ctx.logs_dir() / "fq.log").read_bytes() == b"hi\n"
            return "log only, no echo"

        def head_exits_zero():
            ctx.clear_logs()
            head = ctx.spawn(["head", "-1"], stdin=PIPE, stdout=PIPE,
                             stderr=DEVNULL)
            cat, deemo = cat_pipe(ctx, ["--label", "hd"], stdout=head.stdin)
            head.stdin.close()
            _, derr = deemo.communicate()
            out, _ = head.communicate()
            cat.wait()
            assert deemo.returncode == 0, f"exit {deemo.returncode}, not 0"
            assert out.count(b"\n") == 1, f"head got {out!r}"
            assert b"error" not in derr.lower(), f"stderr complained: {derr!r}"
            return "downstream EPIPE: exit 0, one line, silent"

        def visible_while_still_running(label, producer):
            """The log must show line 1 while the stream is still open and
            silent: a full second before line 2 can arrive. That is the
            flush-before-blocking rule, measured causally."""
            ctx.clear_logs()
            log = ctx.logs_dir() / f"{label}.log"
            proc = producer()
            end = time.time() + 1.0  # well inside the 2 s pause
            while time.time() < end:
                if log.exists() and b"first" in log.read_bytes():
                    assert proc.poll() is None, "stream already closed; not causal"
                    proc.wait()
                    return "line visible after one read, before the next"
                time.sleep(0.01)
            proc.kill()
            proc.wait()
            raise BenchError(f"{label}: log lagged >1 s behind a 2 s pause")

        def fg_realtime():
            return visible_while_still_running(
                "rtf",
                lambda: ctx.spawn(
                    ctx.argv("-F", "--label", "rtf", "--timestamp", "off",
                             "sh", "-c", "echo first; sleep 2; echo second"),
                    stdin=DEVNULL, stdout=DEVNULL, stderr=DEVNULL, env=ctx.env(),
                ),
            )

        def pipe_realtime():
            def producer():
                d = ctx.spawn(
                    ctx.argv("--label", "rtp", "--timestamp", "off"),
                    stdin=PIPE, stdout=DEVNULL, stderr=PIPE, env=ctx.env(),
                )
                d.stdin.write(b"first\n")
                d.stdin.flush()
                # the write end stays open: EOF is what ends the pump
                threading.Timer(2.0, lambda: d.stdin.close()).start()
                return d

            return visible_while_still_running("rtp", producer)

        def yolo_passthrough():
            out, log = pipe_run("yolo", ["--yolo"])
            assert out == big, "--yolo passthrough is not byte-exact"
            assert log is None, "--yolo wrote a log"
            return "no log, no registration, byte-exact"

        def bg_settles():
            ctx.clear_logs()
            cat, deemo = cat_pipe(ctx, ["-B", "--label", "bgchk"])
            _, err = deemo.communicate()
            cat.wait()
            log = log_path_from_stderr(ctx, err)
            wait_settled(log, len(big), timeout=30)
            assert log.read_bytes() == big, "--background log is not byte-exact"
            return "detached pump wrote the whole stream"

        def kill_dry_run_matches_lsof():
            # DESIGN Kill: only the *binding* side is a target. Compare with
            # lsof across TCP/UDP and IPv4/IPv6, with a client connected, and
            # agree that a free port has no holder.
            tcp4, udp4, tcp6 = free_ports(3)
            docroot = str(ctx.root)
            ready = lambda name: str(ctx.root / f"ready-{name}")

            def binder(port, host, kind, name):
                # A marker after bind() is the only readiness signal that does
                # not depend on the thing under test.
                code = (
                    "import socket,time\n"
                    f"s=socket.socket({'socket.AF_INET6' if host == '::1' else 'socket.AF_INET'}, "
                    f"{'socket.SOCK_DGRAM' if kind == 'udp' else 'socket.SOCK_STREAM'})\n"
                    f"s.bind(({host!r},{port}))\n"
                    + ("s.listen()\n" if kind == "tcp" else "")
                    + f"open({ready(name)!r},'w').close()\n"
                    "time.sleep(60)\n"
                )
                return ctx.spawn([sys.executable, "-c", code], stdout=DEVNULL,
                                 stderr=DEVNULL)

            holders = {
                tcp4: ctx.spawn(
                    [sys.executable, "-m", "http.server", str(tcp4), "--bind",
                     "127.0.0.1", "--directory", docroot],
                    stdout=DEVNULL, stderr=DEVNULL,
                ),
                udp4: binder(udp4, "127.0.0.1", "udp", "udp4"),
                tcp6: binder(tcp6, "::1", "tcp", "tcp6"),
            }
            wait_port(tcp4)
            for name in ("udp4", "tcp6"):
                marker = ctx.root / f"ready-{name}"
                end = time.time() + 15
                while not marker.exists():
                    assert time.time() < end, f"{name} holder never bound"
                    time.sleep(0.01)
            client = socket.create_connection(("127.0.0.1", tcp4), timeout=2)
            try:
                want = {}
                for port in holders:
                    _, p = timed_deemo(ctx, ["kill", port, "--dry-run"])
                    want[port] = sorted(int(x) for x in p.stdout.split())
                # lsof as the oracle, filtered to the local side exactly as
                # deemo documents it.
                for port, holder in holders.items():
                    text = subprocess.run(
                        ["lsof", "-nP", "-F", "pn", "-i", f":{port}"],
                        stdout=PIPE, stderr=DEVNULL,
                    ).stdout.decode()
                    oracle, pid = set(), None
                    for line in text.splitlines():
                        if line.startswith("p"):
                            pid = int(line[1:])
                        elif line.startswith("n") and pid:
                            local = line[1:].split("->")[0]
                            if local.rsplit(":", 1)[-1] == str(port):
                                oracle.add(pid)
                    assert want[port] == sorted(oracle), \
                        f"port {port}: deemo {want[port]}, lsof {sorted(oracle)}"
                    assert holder.pid in want[port], f"port {port}: holder missing"
                # A port nobody holds: both say so.
                free = free_ports(1)[0]
                _, p = timed_deemo(ctx, ["kill", free, "--dry-run"], expect=(0, 1))
                assert p.stdout.split() == [], p.stdout
                assert b"nothing is using port" in p.stderr, p.stderr
                return (f"tcp4 {want[tcp4]} + client, udp4 {want[udp4]}, "
                        f"tcp6 {want[tcp6]}, free port empty")
            finally:
                client.close()
                for h in holders.values():
                    h.kill()
                wait_exit(list(holders.values()))

        def kill_dedupe():
            ports = free_ports(2)
            holder = start_holder(ctx, ports[0])
            try:
                s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                s.bind(("127.0.0.1", ports[1]))
                s.listen()
                second = ctx.spawn(["sleep", "3600"], stdout=DEVNULL,
                                   stderr=DEVNULL)
                try:
                    _, p = timed_deemo(ctx, ["kill", *ports, "--dry-run"])
                    pids = [int(x) for x in p.stdout.split()]
                    assert pids.count(holder.pid) == 1, f"holder listed twice: {pids}"
                finally:
                    second.kill()
                    s.close()
            finally:
                holder.kill()
                wait_exit([holder])
            return "each pid listed once across several ports"

        def ps_and_stop():
            ctx.clear_logs()
            timed_deemo(ctx, ["--label", "psrow", "sleep", "3600"])
            try:
                _, p = timed_deemo(ctx, ["ps"])
                text = p.stdout.decode()
                assert "psrow" in text and "running" in text, text
                timed_deemo(ctx, ["stop", "psrow"])
                _, p = timed_deemo(ctx, ["ps"], expect=(1,))
                assert b"no processes" in p.stdout, p.stdout
            finally:
                subprocess.run(ctx.argv("stop", "psrow"), stdout=DEVNULL,
                               stderr=DEVNULL, env=ctx.env())
            return "registration listed, stopped, swept"

        def zombie_counts_as_dead():
            # DESIGN Stop: "A zombie counts as dead." A target whose parent is
            # alive stays a zombie after SIGTERM; deemo must see it die instead
            # of burning the 5 s + 1 s grace and reporting a survivor. This
            # holder is deliberately not reaped by the harness.
            port = free_ports(1)[0]
            zombie = ctx.spawn(
                [sys.executable, "-m", "http.server", str(port), "--bind",
                 "127.0.0.1", "--directory", str(ctx.root)],
                stdout=DEVNULL, stderr=DEVNULL,
            )
            wait_port(port)
            t0 = time.perf_counter()
            _, p = timed_deemo(ctx, ["kill", port], expect=(0, 1))
            dt = time.perf_counter() - t0
            died = zombie.poll() is not None
            if not died:
                zombie.kill()
            zombie.wait()
            assert died, "the holder was never signalled"
            assert dt < 2.0, f"kill took {dt * 1000:.0f} ms on a zombie target"
            assert p.returncode == 0, f"kill reported a survivor: {p.stderr!r}"
            return f"zombie target declared dead in {dt * 1000:.0f} ms"

        check("pipe.byte_exact", pipe_byte_exact)
        check("pipe.unterminated_tail", pipe_unterminated_tail)
        check("pipe.yolo", yolo_passthrough)
        check("fg.byte_exact", fg_byte_exact)
        check("fg.quiet", fg_quiet)
        check("pipe.head_exit0", head_exits_zero)
        check("fg.realtime", fg_realtime)
        check("pipe.realtime", pipe_realtime)
        check("pipe.background", bg_settles)
        check("kill.dry_run_vs_lsof", kill_dry_run_matches_lsof)
        check("kill.dedupe", kill_dedupe)
        check("liveness.zombie", zombie_counts_as_dead)
        check("ps.stop", ps_and_stop)
    finally:
        ctx.teardown()
    return checks


# --------------------------------------------------------------------------
# runner
# --------------------------------------------------------------------------


def run_scenario(scn: Scn, bin_path: Path) -> dict:
    ctx = Ctx(scn.name, bin_path)
    try:
        if scn.setup:
            scn.setup(ctx)
        for _ in range(WARMUP):
            scn.rep(ctx)
        samples = [scn.rep(ctx) / scn.agg * 1000 for _ in range(scn.reps)]
    finally:
        ctx.teardown()

    samples.sort()
    n = len(samples)
    med = samples[n // 2] if n % 2 else (samples[n // 2 - 1] + samples[n // 2]) / 2
    p90 = samples[min(n - 1, int(round(0.9 * (n - 1))))]
    return {
        "name": scn.name,
        "input": scn.input,
        "agg": scn.agg,
        "reps": scn.reps,
        "min": round(samples[0], 2),
        "median": round(med, 2),
        "p90": round(p90, 2),
        "max": round(samples[-1], 2),
        "rate": scn.rate(med) if scn.rate else "",
    }


def git_commit() -> str:
    try:
        return subprocess.run(
            ["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"],
            stdout=PIPE, stderr=DEVNULL, check=True,
        ).stdout.decode().strip()
    except Exception:
        return "unknown"


def host_info() -> str:
    try:
        cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"],
                             stdout=PIPE, stderr=DEVNULL).stdout.decode().strip()
    except Exception:
        cpu = "unknown cpu"
    try:
        os_name = subprocess.run(["sw_vers", "-productVersion"], stdout=PIPE,
                                 stderr=DEVNULL).stdout.decode().strip()
    except Exception:
        os_name = "unknown os"
    return f"{cpu}; {os_name}"


def print_table(rows: list[dict]) -> None:
    head = f"{'scenario':<16} {'input':<44} {'reps':>4} {'agg':>4} " \
           f"{'min':>6} {'med':>6} {'p90':>6}  rate"
    print(head)
    print("-" * len(head))
    for r in rows:
        print(
            f"{r['name']:<16} {r['input'][:44]:<44} {r['reps']:>4} {r['agg']:>4} "
            f"{r['min']:>6.0f} {r['median']:>6.0f} {r['p90']:>6.0f}  {r['rate']}"
        )


def print_variance(rows: list[dict]) -> None:
    worst = max(rows, key=lambda r: (r["p90"] / r["min"]) if r["min"] else 0)
    ratio = worst["p90"] / worst["min"] if worst["min"] else 0
    print(
        f"\nvariance: worst min..p90 spread {worst['min']:.0f}..{worst['p90']:.0f} ms "
        f"({ratio:.2f}x, {worst['name']}); "
        f"{sum(1 for r in rows if r['min'] and r['p90'] / r['min'] > 1.25)} "
        f"scenario(s) above 1.25x"
    )


def diff(before: dict, after: dict) -> None:
    b = {r["name"]: r for r in before["scenarios"]}
    a = {r["name"]: r for r in after["scenarios"]}
    print(
        f"\nbefore={before['label']} ({before['commit']} {before['date'][:10]})  "
        f"after={after['label']} ({after['commit']} {after['date'][:10]})"
    )
    head = f"{'scenario':<16} {'input':<34} {'before':>7} {'after':>7} " \
           f"{'win':>7}  rate after"
    print(head)
    print("-" * len(head))
    for name, rb in sorted(b.items(), key=lambda kv: -kv[1]["median"]):
        ra = a.get(name)
        if not ra:
            continue
        win = rb["median"] / ra["median"] if ra["median"] else 0
        delta = rb["median"] - ra["median"]
        # §2: less than ~10% or less than 0.5 ms is noise.
        noise = delta < max(0.1 * rb["median"], 0.5)
        tag = "noise" if noise and delta > 0 else ""
        print(
            f"{name:<16} {rb['input'][:34]:<34} {rb['median']:>7.0f} "
            f"{ra['median']:>7.0f} {win:>6.2f}x  {ra['rate']} {tag}"
        )
    missing = sorted(set(b) - set(a))
    if missing:
        print(f"\nnot measured after: {', '.join(missing)}")


def build(bin_path: Path) -> None:
    print("building release binary …", flush=True)
    subprocess.run(["cargo", "build", "--release"], cwd=REPO, check=True)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--label", default="run", help="artifact name: bench/<label>.json")
    ap.add_argument("--only", default="", help="comma-separated scenario substrings")
    ap.add_argument("--reps", type=int, default=None)
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--no-verify", action="store_true")
    ap.add_argument("--diff", default=None, help="compare against bench/<label>.json")
    ap.add_argument("--list", action="store_true", help="list scenario names")
    ap.add_argument("--bin", default=str(DEFAULT_BIN))
    args = ap.parse_args()

    bin_path = Path(args.bin).resolve()
    build_shared_fixture()
    try:
        if args.list:
            for scn in scenarios():
                print(f"{scn.name:<16} {scn.input}")
            return 0

        if not args.no_build:
            build(bin_path)
        if not bin_path.exists():
            print(f"missing {bin_path}; run without --no-build", file=sys.stderr)
            return 2

        picks = [s for s in scenarios()
                 if not args.only or any(k in s.name for k in args.only.split(","))]
        if not picks:
            print(f"no scenario matches {args.only!r}", file=sys.stderr)
            return 2
        if args.reps:
            for s in picks:
                s.reps = args.reps

        failed: list[str] = []
        checks: list[dict] = []
        if not args.no_verify:
            print("verifying behaviour (E2E checks) …", flush=True)
            checks = verify(bin_path)
            for c in checks:
                print(f"  {'ok  ' if c['ok'] else 'FAIL'} {c['name']:<20} {c['detail']}")
            failed += [c["name"] for c in checks if not c["ok"]]

        rows = []
        for scn in picks:
            print(f".. {scn.name} ({scn.input})", flush=True)
            try:
                rows.append(run_scenario(scn, bin_path))
            except Exception as e:  # noqa: BLE001 - report, keep going, fail at end
                traceback.print_exc()
                failed.append(scn.name)
                rows.append({"name": scn.name, "input": scn.input, "agg": scn.agg,
                             "reps": scn.reps, "min": 0, "median": 0, "p90": 0,
                             "max": 0, "rate": f"ERROR: {e}"})
        rows.sort(key=lambda r: -r["median"])

        print()
        print_table(rows)
        print_variance(rows)

        artifact = {
            "label": args.label,
            "commit": git_commit(),
            "host": host_info(),
            "date": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "binary": str(bin_path),
            "checks": checks,
            "scenarios": rows,
        }
        BENCH_DIR.mkdir(exist_ok=True)
        out = BENCH_DIR / f"{args.label}.json"
        out.write_text(json.dumps(artifact, indent=2) + "\n")
        print(f"\nwrote {out.relative_to(REPO)}")

        if args.diff:
            other = BENCH_DIR / f"{args.diff}.json"
            if not other.exists():
                print(f"no artifact {other}", file=sys.stderr)
                return 2
            diff(json.loads(other.read_text()), artifact)

        if failed:
            print(f"\nFAILED: {', '.join(failed)}", file=sys.stderr)
            return 1
        return 0
    finally:
        shutil.rmtree(SHARED, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
