"""End-to-end test runner: builds the plugin, deploys it to Zorro, runs the
checks a change needs, and reads their results. Windows, standard library
only.

    python tests/zorro/zorrorun.py --deploy unit probe spot:27 perp:58

Stages, run in the order given:

  unit             cargo test: the offline suite and the FFI harness
  live:<name>      one #[ignore]d live test against testnet (may trade)
  probe            HyperliquidProbe.c in Zorro: logs in, prices, no orders
  spot, perp       the Lite-C stress tests in Zorro (they trade on testnet)
  <Script>         any other script in Zorro's Strategy folder

A Zorro stage may carry the pass count it must reach, as `perp:58`.

Settings come from the repo's .env (gitignored): HL_TESTNET_PRIVKEY, and
ZORRO_DIR, the Zorro folder (default: a "Zorro" folder in the home folder).
The key goes into the environment of Zorro and cargo only: never onto a
command line, into a file, or into the output, which blanks keys and
addresses.

Free Zorro ignores "-trade", so a Zorro stage does what a person would:
opens Zorro on the script, selects broker Hyperliquid and account Demo,
reads both back, and presses [Trade] with the Login and Password fields
blank, which makes the plugin use the key from the environment. The plugin
does that on Demo only; on Real a blank Password fails the login. The run is
also stopped at once if the window shows a mainnet login.

Exit code: 0 all passed, 1 a check failed, 2 a stage did not complete
(setup, compile error, no login, timeout), 3 stopped at a mainnet login.
"""

import argparse
import ctypes
import os
import re
import shutil
import subprocess
import sys
import time
from ctypes import wintypes
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]

ALIASES = {
    "probe": "HyperliquidProbe",
    "spot": "HyperliquidStressTest",
    "perp": "HyperliquidPerpStressTest",
}
# Minutes a Zorro run may take before it is stopped.
TIMEOUTS = {"HyperliquidProbe": 5, "HyperliquidStressTest": 15, "HyperliquidPerpStressTest": 30}
DEFAULT_TIMEOUT = 30
# A session is over when its "Logout.." has been followed by this much quiet.
SETTLE_S = 15
KEY_ENV = "HL_TESTNET_PRIVKEY"

EXIT_PASS, EXIT_FAIL, EXIT_INCOMPLETE, EXIT_MAINNET = 0, 1, 2, 3
STATUS = {EXIT_PASS: "PASS", EXIT_FAIL: "FAIL", EXIT_INCOMPLETE: "INCOMPLETE", EXIT_MAINNET: "MAINNET"}


# --------------------------------------------------------------------------
# Output hygiene: nothing secret or account-identifying is printed.
# --------------------------------------------------------------------------

_KEY = re.compile(r"0x[0-9a-fA-F]{64}")
_ADDRESS = re.compile(r"0x[0-9a-fA-F]{40}\b")


def redact(text: str) -> str:
    return _ADDRESS.sub("0x<address>", _KEY.sub("0x<key>", text))


def say(text: str = "") -> None:
    print(redact(text), flush=True)


# --------------------------------------------------------------------------
# Parsing (pure; covered by test_zorrorun.py)
# --------------------------------------------------------------------------

_CHECK = re.compile(r"^\s*(PASS|FAIL)\s+(.+?)\s*$")
_RESULT = re.compile(r"=== RESULT: (\d+) passed, (\d+) failed ===")
_LOGIN = re.compile(r"(?:login OK|logging in) \((testnet|mainnet)\)")
_LOGIN_FAILED = re.compile(r"login FAILED: (.*)")
_PLUGIN_REFUSAL = re.compile(r"FAILED|refused")
_ERROR = re.compile(r"\bError \d+\b|\berror in\b|crash", re.IGNORECASE)
_CARGO_RESULT = re.compile(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed")


@dataclass
class Outcome:
    passed: list = field(default_factory=list)
    failed: list = field(default_factory=list)
    result: tuple | None = None  # (passed, failed) as the script counted
    network: str | None = None
    login_error: str | None = None
    refusals: list = field(default_factory=list)
    errors: list = field(default_factory=list)


def parse(lines) -> Outcome:
    out = Outcome()
    for line in lines:
        if m := _CHECK.match(line):
            (out.passed if m.group(1) == "PASS" else out.failed).append(m.group(2))
        elif m := _RESULT.search(line):
            out.result = (int(m.group(1)), int(m.group(2)))
        elif m := _LOGIN.search(line):
            if out.network != "mainnet":
                out.network = m.group(1)
        elif m := _LOGIN_FAILED.search(line):
            out.login_error = m.group(1).strip()
        elif _PLUGIN_REFUSAL.search(line):
            out.refusals.append(line.strip())
        elif _ERROR.search(line):
            out.errors.append(line.strip())
    return out


def combine(window: Outcome, log: Outcome | None) -> Outcome:
    """Checks from the log, which holds the whole run; the login from the
    window too, since the plugin logs in before Zorro opens the log."""
    if log is None:
        return window
    out = log
    if "mainnet" in (window.network, log.network):
        out.network = "mainnet"
    else:
        out.network = log.network or window.network
    out.login_error = log.login_error or window.login_error
    out.errors = log.errors + [e for e in window.errors if e not in log.errors]
    out.refusals = log.refusals + [r for r in window.refusals if r not in log.refusals]
    return out


def verdict(out: Outcome, expect: int | None) -> tuple[int, list]:
    """Exit code and the reasons for it."""
    reasons = []
    if out.network == "mainnet":
        return EXIT_MAINNET, ["the plugin logged in to MAINNET"]
    if out.network != "testnet":
        reasons.append(f"no testnet login ({out.login_error or 'no login line'})")
    if out.result is None:
        reasons.append("the script printed no RESULT line")
        return EXIT_INCOMPLETE, reasons + out.errors[:5]
    passed, failed = out.result
    if failed or out.failed:
        reasons.append(f"{failed} check(s) failed")
    if expect is not None and passed != expect:
        reasons.append(f"{passed} passed, expected {expect}")
    if (passed, failed) != (len(out.passed), len(out.failed)):
        reasons.append(f"RESULT says {passed}/{failed}, the log lists {len(out.passed)}/{len(out.failed)}")
    return (EXIT_FAIL if reasons else EXIT_PASS), reasons


def cargo_verdict(returncode: int, output: str) -> tuple[int, list]:
    results = _CARGO_RESULT.findall(output)
    if not results:
        return EXIT_INCOMPLETE, ["cargo printed no test result (build failed?)"]
    passed = sum(int(p) for _, p, _ in results)
    failed = sum(int(f) for _, _, f in results)
    if returncode != 0 or failed:
        return EXIT_FAIL, [f"{failed} test(s) failed, {passed} passed"]
    if passed == 0:
        return EXIT_FAIL, ["no test ran: is the name right?"]
    return EXIT_PASS, [f"{passed} passed"]


def read_env(env_file: Path) -> dict:
    """KEY=VALUE lines of a .env file; missing file, empty dict."""
    values = {}
    if not env_file.exists():
        return values
    for line in env_file.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line.startswith("export "):
            line = line[7:].lstrip()
        if not line or line.startswith("#"):
            continue
        key, sep, value = line.partition("=")
        if sep:
            value = value.strip().strip('"').strip("'")
            if value:
                values[key.strip()] = value
    return values


def parse_stage(arg: str) -> tuple[str, str, int | None]:
    """`(kind, name, expected passes)`; kind is unit, live or zorro."""
    if arg == "unit":
        return "unit", "cargo test", None
    if arg.startswith("live:"):
        return "live", arg[5:], None
    name, _, expect = arg.partition(":")
    return "zorro", ALIASES.get(name, name), (int(expect) if expect else None)


# --------------------------------------------------------------------------
# Windows: Zorro's process, window and controls
# --------------------------------------------------------------------------

user32 = ctypes.WinDLL("user32", use_last_error=True)
_ENUM = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
user32.EnumWindows.argtypes = [_ENUM, wintypes.LPARAM]
user32.EnumChildWindows.argtypes = [wintypes.HWND, _ENUM, wintypes.LPARAM]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.GetClassNameA.argtypes = [wintypes.HWND, ctypes.c_char_p, ctypes.c_int]
user32.GetWindowTextA.argtypes = [wintypes.HWND, ctypes.c_char_p, ctypes.c_int]
user32.GetDlgItem.argtypes = [wintypes.HWND, ctypes.c_int]
user32.GetDlgItem.restype = wintypes.HWND
user32.SendMessageA.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.SendMessageA.restype = ctypes.c_ssize_t
user32.PostMessageA.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]

LB_GETTEXT, LB_GETTEXTLEN, LB_GETCOUNT, WM_CLOSE = 0x0189, 0x018A, 0x018B, 0x0010
CB_GETCOUNT, CB_GETCURSEL, CB_SETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN = 0x0146, 0x0147, 0x014E, 0x0148, 0x0149
WM_COMMAND, CBN_SELCHANGE, BM_CLICK = 0x0111, 1, 0x00F5
# Control ids of Zorro 3.11's main window, read from a live window.
ID_BROKER, ID_ACCOUNT, ID_USER, ID_PASSWORD, ID_TRADE, ID_MESSAGES = 1017, 1003, 1000, 1001, 1062, 1028


def _windows_of(pid: int) -> list:
    found = []

    def top(h, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(h, ctypes.byref(owner))
        if owner.value == pid:
            found.append(h)
        return True

    user32.EnumWindows(_ENUM(top), 0)
    return found


def window_text(h) -> str:
    buf = ctypes.create_string_buffer(512)
    user32.GetWindowTextA(h, buf, 512)
    return buf.value.decode("cp1252", "replace")


def main_window(pid: int, script: str, wait_s: float = 30):
    """Zorro's main window, once its title names `script`."""
    deadline = time.time() + wait_s
    while time.time() < deadline:
        for w in _windows_of(pid):
            title = window_text(w)
            if title.startswith("Zorro") and title.endswith(f" - {script}"):
                return w
        time.sleep(0.3)
    return None


def message_lines(window) -> list:
    """Lines of Zorro's message window."""
    lb = user32.GetDlgItem(window, ID_MESSAGES)
    lines = []
    for i in range(max(0, user32.SendMessageA(lb, LB_GETCOUNT, 0, 0))):
        n = user32.SendMessageA(lb, LB_GETTEXTLEN, i, 0)
        if n < 0:
            continue
        buf = ctypes.create_string_buffer(n + 1)
        got = user32.SendMessageA(lb, LB_GETTEXT, i, ctypes.addressof(buf))
        if got >= 0:
            lines.append(buf.raw[:got].decode("cp1252", "replace"))
    return lines


def combo_items(combo) -> list:
    items = []
    for i in range(max(0, user32.SendMessageA(combo, CB_GETCOUNT, 0, 0))):
        n = user32.SendMessageA(combo, CB_GETLBTEXTLEN, i, 0)
        buf = ctypes.create_string_buffer(max(n, 0) + 1)
        user32.SendMessageA(combo, CB_GETLBTEXT, i, ctypes.addressof(buf))
        items.append(buf.value.decode("cp1252", "replace"))
    return items


def combo_selected(combo) -> str | None:
    i = user32.SendMessageA(combo, CB_GETCURSEL, 0, 0)
    items = combo_items(combo)
    return items[i] if 0 <= i < len(items) else None


def select(window, control_id: int, wanted: str) -> str | None:
    """Selects the item `wanted` as a click would, telling Zorro about it,
    and returns what the box shows afterwards."""
    combo = user32.GetDlgItem(window, control_id)
    if not combo:
        return None
    items = combo_items(combo)
    if wanted in items and combo_selected(combo) != wanted:
        user32.SendMessageA(combo, CB_SETCURSEL, items.index(wanted), 0)
        user32.SendMessageA(window, WM_COMMAND, (CBN_SELCHANGE << 16) | control_id, combo)
        time.sleep(1.0)  # Zorro loads the broker plugin on a change
    return combo_selected(combo)


def prepare_and_trade(window) -> list:
    """Broker Hyperliquid, account Demo, blank login, then [Trade]. Returns
    why it would not press it; empty when pressed."""
    broker = select(window, ID_BROKER, "Hyperliquid")
    account = select(window, ID_ACCOUNT, "Demo")
    problems = []
    if broker != "Hyperliquid":
        problems.append(f"broker box shows {broker!r}, not 'Hyperliquid'")
    if account != "Demo":
        problems.append(f"account box shows {account!r}, not 'Demo'")
    for cid, name in ((ID_USER, "Login"), (ID_PASSWORD, "Password")):
        edit = user32.GetDlgItem(window, cid)
        if edit and window_text(edit):
            # A typed key would be used instead, on either network.
            problems.append(f"the {name} field is not empty")
    trade = user32.GetDlgItem(window, ID_TRADE)
    if not trade or window_text(trade) != "Trade":
        problems.append("no [Trade] button")
    if not problems:
        # Posted, not sent: Zorro runs the whole session from this click.
        user32.PostMessageA(trade, BM_CLICK, 0, 0)
    return problems


def zorro_pids() -> list:
    out = subprocess.run(
        ["tasklist", "/FI", "IMAGENAME eq Zorro.exe", "/FO", "CSV", "/NH"],
        capture_output=True, text=True,
    ).stdout
    return [int(row.split('","')[1]) for row in out.splitlines() if row.startswith('"Zorro.exe"')]


def close_zorro(pid: int, wait_s: float = 20) -> None:
    """Asks Zorro to close (AutoConfirm = 1 in Zorro.ini stops it asking
    about trades), then ends the process if it is still there."""
    for w in _windows_of(pid):
        user32.PostMessageA(w, WM_CLOSE, 0, 0)
    deadline = time.time() + wait_s
    while time.time() < deadline:
        if pid not in zorro_pids():
            return
        time.sleep(0.5)
    kill_zorro(pid)


def kill_zorro(pid: int) -> None:
    subprocess.run(["taskkill", "/PID", str(pid), "/F"], capture_output=True)


# --------------------------------------------------------------------------
# Stages
# --------------------------------------------------------------------------

def cargo_exe() -> str:
    found = Path.home() / ".cargo" / "bin" / "cargo.exe"
    return str(found) if found.exists() else "cargo"


def deploy(zorro: Path) -> None:
    for target in ([], ["--target", "i686-pc-windows-msvc"]):
        say(f"building the {'32' if target else '64'}-bit DLL...")
        r = subprocess.run([cargo_exe(), "build", "--release", *target], cwd=REPO, capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError("cargo build failed:\n" + r.stderr[-3000:])
    copies = [
        (REPO / "target/i686-pc-windows-msvc/release/liquidzorro.dll", zorro / "Plugin/Hyperliquid.dll"),
        (REPO / "target/release/liquidzorro.dll", zorro / "Plugin64/Hyperliquid.dll"),
        (REPO / "examples/AssetsHyperliquid.csv", zorro / "History/AssetsHyperliquid.csv"),
        (HERE / "HyperliquidProbe.c", zorro / "Strategy/HyperliquidProbe.c"),
    ]
    copies += [(c, zorro / "Strategy" / c.name) for c in (REPO / "examples").glob("Hyperliquid*Test.c")]
    for src, dst in copies:
        shutil.copy2(src, dst)
    say(f"deployed {len(copies)} files")


def run_cargo(args: list, key: str | None, label: str) -> tuple[int, list]:
    env = dict(os.environ)
    if key:
        env[KEY_ENV] = key
    say(f"\n-- {label}: cargo {' '.join(args)}")
    r = subprocess.run([cargo_exe(), *args], cwd=REPO, env=env, capture_output=True, text=True)
    output = r.stdout + r.stderr
    for line in output.splitlines():
        if re.search(r"^test .* FAILED$|panicked|^error|test result:", line):
            say(f"  | {line}")
    return cargo_verdict(r.returncode, output)


def run_zorro(zorro: Path, script: str, key: str, timeout_min: float) -> tuple[Outcome, bool]:
    """Runs `script` in [Trade] mode. Returns what it showed, and whether it
    may have been cut short with trades open."""
    env = dict(os.environ)
    env[KEY_ENV] = key
    started = time.time()
    # A script name alone only selects it, which free Zorro allows.
    proc = subprocess.Popen(
        [str(zorro / "Zorro.exe"), script], cwd=zorro, env=env,
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    window = main_window(proc.pid, script)
    if window is None:
        say(f"!! Zorro showed no window for {script}")
        close_zorro(proc.pid)
        return Outcome(errors=["Zorro showed no window"]), False
    problems = prepare_and_trade(window)
    if problems:
        for p in problems:
            say(f"!! not trading: {p}")
        close_zorro(proc.pid)
        return Outcome(errors=problems), False
    say(f"\n-- {script}: Hyperliquid, Demo, [Trade] (pid {proc.pid}); up to {timeout_min:.0f} min")

    lines, seen, last_change, cut_short = [], 0, time.time(), False
    while proc.poll() is None:
        current = message_lines(window)
        if len(current) != len(lines):
            last_change = time.time()
        lines = current or lines
        for line in lines[seen:]:
            say(f"  | {line}")
            # A run that reached mainnet is stopped at once.
            if "(mainnet)" in line and ("login OK" in line or "logging in" in line):
                kill_zorro(proc.pid)
                say("!! MAINNET login: Zorro killed")
                return parse(lines), True
        seen = len(lines)
        logged_out = any(l.startswith("Logout") for l in lines[-6:])
        if logged_out and time.time() - last_change > SETTLE_S:
            break
        if time.time() - started > timeout_min * 60:
            say(f"!! still running after {timeout_min:.0f} min; closing Zorro")
            cut_short = True
            break
        time.sleep(1)
    close_zorro(proc.pid)

    log = newest_log(zorro, script, started)
    log_out = None
    if log is not None:
        if log.name.endswith("_real.log"):
            say(f"!! the log is {log.name}: the run used the Real account")
        log_out = parse(log.read_text(encoding="cp1252", errors="replace").splitlines())
    return combine(parse(lines), log_out), cut_short


def newest_log(zorro: Path, script: str, since: float) -> Path | None:
    logs = [p for p in (zorro / "Log").glob(f"{script}_*.log") if p.stat().st_mtime >= since - 2]
    return max(logs, key=lambda p: p.stat().st_mtime, default=None)


def report(name: str, code: int, reasons: list, out: Outcome | None = None) -> None:
    detail = ""
    if out is not None:
        counted = f"{out.result[0]} passed, {out.result[1]} failed" if out.result else "no result"
        detail = f" ({counted}, network {out.network or '?'})"
    say(f"== {name}: {STATUS[code]}{detail}")
    for r in reasons:
        say(f"   - {r}")
    if out is None:
        return
    for check in out.failed:
        say(f"   FAIL {check}")
    if out.refusals:
        say(f"   plugin refusals logged ({len(out.refusals)}; the tests provoke most of them):")
        for line in out.refusals:
            say(f"     {line[:200]}")


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("stages", nargs="+", help="unit | live:<test> | probe | spot | perp | <Script>[:<passes>]")
    ap.add_argument("--deploy", action="store_true", help="build both release DLLs and copy them, the scripts and the asset list to Zorro")
    ap.add_argument("--close-zorro", action="store_true", help="close a Zorro that is open instead of stopping")
    ap.add_argument("--timeout", type=float, help="minutes per Zorro run (default per script)")
    ap.add_argument("--keep-going", action="store_true", help="run the remaining stages after one fails")
    a = ap.parse_args(argv)

    stages = [parse_stage(s) for s in a.stages]
    env = read_env(REPO / ".env")
    key = env.get(KEY_ENV)
    zorro = Path(env.get("ZORRO_DIR") or os.environ.get("ZORRO_DIR") or Path.home() / "Zorro")
    needs_zorro = a.deploy or any(kind == "zorro" for kind, _, _ in stages)
    if not key and (needs_zorro or any(kind == "live" for kind, _, _ in stages)):
        say(f"{KEY_ENV} is not set in the repo's .env")
        return EXIT_INCOMPLETE
    if needs_zorro:
        if not (zorro / "Zorro.exe").exists():
            say(f"no Zorro.exe in {zorro}; set ZORRO_DIR in the repo's .env")
            return EXIT_INCOMPLETE
        open_pids = zorro_pids()
        if open_pids and not a.close_zorro:
            say(f"Zorro is open (pid {open_pids}); close it, or pass --close-zorro")
            return EXIT_INCOMPLETE
        for pid in open_pids:
            say(f"closing the open Zorro (pid {pid})")
            close_zorro(pid)
    if a.deploy:
        try:
            deploy(zorro)
        except Exception as e:
            say(f"deploy failed: {e}")
            return EXIT_INCOMPLETE

    worst, summary, maybe_open = EXIT_PASS, [], False
    for kind, name, expect in stages:
        if kind == "unit":
            code, reasons = run_cargo(["test"], None, "unit")
            report("unit", code, reasons)
        elif kind == "live":
            args = ["test", "--lib", "--", "--ignored", "--test-threads=1", "--exact", f"bridge::tests::{name}"]
            code, reasons = run_cargo(args, key, f"live:{name}")
            report(f"live:{name}", code, reasons)
        else:
            out, cut_short = run_zorro(zorro, name, key, a.timeout or TIMEOUTS.get(name, DEFAULT_TIMEOUT))
            code, reasons = verdict(out, expect)
            report(name, code, reasons, out)
            maybe_open |= cut_short or code == EXIT_MAINNET
        summary.append((f"{kind}:{name}" if kind != "zorro" else name, code))
        worst = max(worst, code)
        if code == EXIT_MAINNET or (code != EXIT_PASS and not a.keep_going):
            break

    say("\n== summary")
    for name, code in summary:
        say(f"   {STATUS[code]:<10} {name}")
    skipped = len(stages) - len(summary)
    if skipped:
        say(f"   ({skipped} stage(s) not run)")
    if maybe_open:
        say("   a run was cut short: check the account for open positions and orders")
    return worst


if __name__ == "__main__":
    sys.exit(main())
