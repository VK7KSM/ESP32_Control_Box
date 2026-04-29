from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import subprocess
import sys
import time
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

try:
    import serial
    from serial import SerialException
    from serial.tools import list_ports
except ImportError:  # pragma: no cover - handled at runtime
    serial = None
    SerialException = Exception
    list_ports = None


RE_ANSI = re.compile(r"\x1b\[[0-9;]*m")
RE_ACCEPT = re.compile(r"\[Rigctld\] 接受连接：(?P<peer>\S+) clients_before=(?P<clients_before>\d+) session_before=(?P<session_before>\d+) RX采样=(?P<rx_sample>\S+)")
RE_CLOSE = re.compile(r"\[Rigctld\] 连接 (?P<peer>\S+) 已关闭，handler_session=(?P<handler_session>\S+) current_session=(?P<current_session>\d+) 剩余 (?P<remaining>\d+) 客户端")
RE_REASON_FIN = re.compile(r"\[Rigctld\] 对端 TCP 正常关闭 .*handler_session=(?P<handler_session>\S+)")
RE_REASON_IDLE = re.compile(r"\[Rigctld\] 10s 无命令，关闭空闲 client .*handler_session=(?P<handler_session>\S+)")
RE_REASON_ERR = re.compile(r"\[Rigctld\] 对端异常断开 kind=(?P<kind>\S+) err=(?P<err>.*?) handler_session=(?P<handler_session>\S+)")
RE_SESSION_START = re.compile(r"\[RigctldGate\].*启动 SatSession #(?P<session>\d+)")
RE_SESSION_BIND = re.compile(r"\[SatSession #(?P<session>\d+)\] 绑定本次 DTrac 会话: RX=(?P<rx>\S+) TX=(?P<tx>[^（\s]+)")
RE_SETUP_EXHAUSTED = re.compile(r"\[SatGate #(?P<session>\d+)\] setup attempts exhausted")
RE_GUARD2 = re.compile(r"\[MenuNav\] Guard2 fail #(?P<menu>\d+) alive=(?P<alive>\S+) tx=(?P<tx>\S+) busy=(?P<busy>\S+)")
RE_U64_SESSION = re.compile(r"#(?P<session>\d+)")

CRASH_MARKERS = ("Backtrace", "stack overflow", "panic", "abort", "CORRUPTED")
BOOT_MARKERS = ("ElfRadio HwNode 启动中", "cpu_start", "Rebooting")
INTENTIONAL_RESTART_MARKER = "[PC通信] WiFi 凭据已写入 NVS，1 秒后重启"
RIGCTLD_KEYWORDS = ("[Rigctld]", "[RigctldGate]", "[SatSession", "[SatGate", "[MenuNav]")
LOG_KEYWORDS = RIGCTLD_KEYWORDS + ("ElfRadio HwNode", "cpu_start", "I (", "W (", "E (", "[WiFi]", "[PC通信]")


def json_default(value: Any) -> str:
    return str(value)


@dataclass
class Event:
    ts: str
    event: str
    severity: str
    raw: str
    fields: dict[str, Any] = field(default_factory=dict)


@dataclass
class ProbeResult:
    device: str
    description: str
    hwid: str
    score: int
    bytes_read: int
    reason: str
    sample: str
    error: str | None = None


class EventDetector:
    def __init__(self) -> None:
        self.last_disconnect_reason: Event | None = None
        self.last_peer: str | None = None
        self.last_session: str | None = None
        self.last_binding: dict[str, str] = {}
        self.counts: Counter[str] = Counter()

    def process(self, line: str, ts: str) -> list[Event]:
        parse_line = RE_ANSI.sub("", line)
        events: list[Event] = []

        def add(event: str, severity: str, fields: dict[str, Any] | None = None) -> Event:
            ev = Event(ts=ts, event=event, severity=severity, raw=line, fields=fields or {})
            events.append(ev)
            self.counts[event] += 1
            return ev

        if INTENTIONAL_RESTART_MARKER in parse_line:
            add("intentional_restart", "warn")

        if any(marker in parse_line for marker in BOOT_MARKERS):
            add("esp32_boot_marker", "info")

        if any(marker.lower() in parse_line.lower() for marker in CRASH_MARKERS):
            add("esp32_crash_marker", "critical")

        if m := RE_ACCEPT.search(parse_line):
            fields = m.groupdict()
            self.last_peer = fields.get("peer")
            add("rigctld_accept", "info", fields)

        if m := RE_SESSION_START.search(parse_line):
            fields = m.groupdict()
            self.last_session = fields.get("session")
            add("sat_session_started", "info", fields)

        if m := RE_SESSION_BIND.search(parse_line):
            fields = m.groupdict()
            self.last_session = fields.get("session")
            self.last_binding = {"rx": fields.get("rx", ""), "tx": fields.get("tx", "")}
            add("sat_session_bound", "info", fields)

        if m := RE_REASON_FIN.search(parse_line):
            ev = add("rigctld_disconnect_reason", "warn", {"reason": "tcp_fin", **m.groupdict()})
            self.last_disconnect_reason = ev

        if m := RE_REASON_IDLE.search(parse_line):
            ev = add("rigctld_disconnect_reason", "warn", {"reason": "idle_timeout", **m.groupdict()})
            self.last_disconnect_reason = ev

        if m := RE_REASON_ERR.search(parse_line):
            fields = m.groupdict()
            ev = add("rigctld_disconnect_reason", "warn", {"reason": "tcp_error", **fields})
            self.last_disconnect_reason = ev

        if m := RE_CLOSE.search(parse_line):
            fields = m.groupdict()
            self.last_peer = fields.get("peer")
            close_event = add("rigctld_client_closed", "warn", fields)
            if fields.get("remaining") == "0":
                dtrac_fields: dict[str, Any] = {
                    **fields,
                    "last_peer": self.last_peer,
                    "last_session": self.last_session,
                    "last_binding": self.last_binding,
                    "close_raw": close_event.raw,
                }
                if self.last_disconnect_reason:
                    dtrac_fields["reason_event"] = self.last_disconnect_reason.event
                    dtrac_fields["reason_fields"] = self.last_disconnect_reason.fields
                    dtrac_fields["reason_raw"] = self.last_disconnect_reason.raw
                add("dtrac_disconnected", "critical", dtrac_fields)

        if "会话结束，排队注入 RX/TX SQL=60% 安全关闭" in parse_line:
            add("sat_cleanup_sql_queued", "info", self._session_from_parse_line(parse_line))

        if "会话结束，无条件排队 RX/TX 亚音 OFF 检查" in parse_line:
            add("sat_cleanup_tone_queued", "info", self._session_from_parse_line(parse_line))

        if "断开后 RX/TX 亚音清理完成" in parse_line:
            add("sat_cleanup_tone_done", "info")

        if "断开后 RX/TX SQL 关闭注入完成" in parse_line:
            add("sat_cleanup_sql_done", "info")

        if m := RE_SETUP_EXHAUSTED.search(parse_line):
            add("sat_setup_attempts_exhausted", "critical", m.groupdict())

        if m := RE_GUARD2.search(parse_line):
            add("menu_guard2_fail", "warn", m.groupdict())

        return events

    @staticmethod
    def _session_from_parse_line(parse_line: str) -> dict[str, str]:
        if m := RE_U64_SESSION.search(parse_line):
            return m.groupdict()
        return {}


class RunOutputs:
    def __init__(self, out_dir: Path, prefix: str, terminal_events: list[str] | None = None) -> None:
        self.out_dir = out_dir
        self.out_dir.mkdir(parents=True, exist_ok=True)
        self.raw_path = out_dir / f"{prefix}-esp32-console.log"
        self.events_path = out_dir / f"{prefix}-events.jsonl"
        self.report_path = out_dir / f"{prefix}-report.md"
        self.latest_path = out_dir / "latest-run.txt"
        self.raw_file = self.raw_path.open("a", encoding="utf-8", newline="")
        self.events_file = self.events_path.open("a", encoding="utf-8", newline="")
        self.report_file = self.report_path.open("a", encoding="utf-8", newline="")
        self.terminal_events = set(terminal_events or [])
        self.latest_path.write_text(
            f"raw={self.raw_path}\nevents={self.events_path}\nreport={self.report_path}\n",
            encoding="utf-8",
        )
        self.report_file.write(f"# ESP32 / DTrac stability monitor run {prefix}\n\n")
        self.report_file.flush()

    def close(self) -> None:
        self.raw_file.close()
        self.events_file.close()
        self.report_file.close()

    def write_raw(self, ts: str, parse_line: str, source: str = "live") -> None:
        self.raw_file.write(f"{ts} [{source}] {parse_line}\n")

    def write_event(self, event: Event) -> None:
        data = {"ts": event.ts, "event": event.event, "severity": event.severity, "raw": event.raw, "fields": event.fields}
        self.events_file.write(json.dumps(data, ensure_ascii=False, sort_keys=True, default=json_default) + "\n")
        if self.terminal_events and event.event in self.terminal_events:
            print(json.dumps(data, ensure_ascii=False, sort_keys=True, default=json_default))

    def append_report(self, event: Event) -> None:
        if event.event not in REPORT_EVENTS:
            return
        self.report_file.write(f"## {event.ts} - {event.event}\n\n")
        self.report_file.write(f"- severity: `{event.severity}`\n")
        if event.fields:
            self.report_file.write("- fields:\n")
            for key, value in event.fields.items():
                rendered = json.dumps(value, ensure_ascii=False, default=json_default) if isinstance(value, (dict, list)) else str(value)
                self.report_file.write(f"  - `{key}`: `{rendered}`\n")
        self.report_file.write("\nRaw:\n\n")
        self.report_file.write(f"> {event.raw}\n\n")
        reason_raw = event.fields.get("reason_raw") if event.fields else None
        if reason_raw:
            self.report_file.write("Reason raw:\n\n")
            self.report_file.write(f"> {reason_raw}\n\n")
        self.report_file.flush()

    def flush(self) -> None:
        self.raw_file.flush()
        self.events_file.flush()
        self.report_file.flush()


REPORT_EVENTS = {
    "monitor_start",
    "monitor_stop",
    "port_probe",
    "port_selected",
    "serial_error",
    "serial_disconnected",
    "esp32_boot_marker",
    "esp32_crash_marker",
    "intentional_restart",
    "rigctld_accept",
    "sat_session_started",
    "sat_session_bound",
    "rigctld_disconnect_reason",
    "dtrac_disconnected",
    "sat_cleanup_sql_queued",
    "sat_cleanup_tone_queued",
    "sat_cleanup_sql_done",
    "sat_cleanup_tone_done",
    "sat_setup_attempts_exhausted",
}


def now_iso() -> str:
    return dt.datetime.now().astimezone().isoformat(timespec="seconds")


def file_stamp() -> str:
    return dt.datetime.now().strftime("%Y%m%d-%H%M%S")


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def ensure_pyserial() -> None:
    if serial is None or list_ports is None:
        raise SystemExit("缺少 pyserial：请先运行 `python -m pip install pyserial`")


def list_serial_port_infos() -> list[Any]:
    ensure_pyserial()
    return sorted(list_ports.comports(), key=lambda p: natural_port_key(p.device))


def natural_port_key(name: str) -> tuple[str, int]:
    m = re.search(r"(\D+)(\d+)$", name)
    if not m:
        return (name, -1)
    return (m.group(1), int(m.group(2)))


def describe_port(info: Any) -> str:
    vid = f"0x{info.vid:04X}" if getattr(info, "vid", None) is not None else "-"
    pid = f"0x{info.pid:04X}" if getattr(info, "pid", None) is not None else "-"
    return f"{info.device} desc={info.description!r} hwid={info.hwid!r} vid={vid} pid={pid} manufacturer={getattr(info, 'manufacturer', None)!r}"


def print_ports() -> None:
    ports = list_serial_port_infos()
    if not ports:
        print("未发现串口")
        return
    for info in ports:
        print(describe_port(info))


def metadata_score(info: Any) -> tuple[int, list[str]]:
    text = " ".join(str(x or "") for x in [info.device, info.description, info.hwid, getattr(info, "manufacturer", "")]).lower()
    score = 0
    reasons: list[str] = []
    if "espressif" in text or "303a" in text:
        score += 20
        reasons.append("metadata:espressif")
    if "usb serial" in text or "usb-serial" in text or "usb jtag" in text or "jtag" in text:
        score += 25
        reasons.append("metadata:usb-serial")
    if "ch340" in text or "ch343" in text or "ch341" in text or "1a86" in text or "wch" in text or "ftdi" in text or "0403" in text:
        score += 55
        reasons.append("metadata:uart-bridge")
    if "bluetooth" in text:
        score -= 50
        reasons.append("metadata:bluetooth-penalty")
    return score, reasons


def open_serial_safely(port: str, baud: int, timeout: float) -> Any:
    ensure_pyserial()
    ser = serial.Serial()
    ser.port = port
    ser.baudrate = baud
    ser.timeout = timeout
    ser.write_timeout = timeout
    ser.dtr = False
    ser.rts = False
    ser.open()
    try:
        ser.dtr = False
        ser.rts = False
    except Exception:
        pass
    return ser


def probe_port(info: Any, baud: int, probe_seconds: float) -> ProbeResult:
    base_score, reasons = metadata_score(info)
    sample_text = ""
    data = b""
    try:
        with open_serial_safely(info.device, baud, timeout=0.1) as ser:
            deadline = time.monotonic() + max(0.0, probe_seconds)
            while time.monotonic() < deadline:
                chunk = ser.read(4096)
                if chunk:
                    data += chunk
                    if len(data) >= 16384:
                        break
                else:
                    time.sleep(0.02)
    except Exception as exc:
        return ProbeResult(info.device, info.description, info.hwid, -1000, 0, "open-failed", "", str(exc))

    sample_text = data.decode("utf-8", errors="replace")
    score = base_score
    if data:
        reasons.append(f"bytes:{len(data)}")
        printable = sum(1 for ch in sample_text if ch.isprintable() or ch in "\r\n\t")
        ratio = printable / max(1, len(sample_text))
        if ratio > 0.75:
            score += 20
            reasons.append("mostly-text")
        for keyword in LOG_KEYWORDS:
            if keyword in sample_text:
                score += 80 if keyword in RIGCTLD_KEYWORDS else 45
                reasons.append(f"log:{keyword}")
        if b"\xaa\x55" in data:
            score -= 15
            reasons.append("binary-aa55-penalty")
    else:
        reasons.append("silent-probe")
    return ProbeResult(info.device, info.description, info.hwid, score, len(data), ",".join(reasons), sample_text[:1000])


def auto_select_port(baud: int, probe_seconds: float, require_log_evidence: bool, outputs: RunOutputs | None = None) -> str:
    ports = list_serial_port_infos()
    if not ports:
        raise RuntimeError("未发现任何串口")

    print("[monitor] 串口列表：", file=sys.stderr)
    for info in ports:
        print(f"  {describe_port(info)}", file=sys.stderr)

    results = [probe_port(info, baud, probe_seconds) for info in ports]
    for result in results:
        parse_line = f"probe {result.device}: score={result.score} bytes={result.bytes_read} reason={result.reason} error={result.error or ''}"
        print(f"[monitor] {parse_line}", file=sys.stderr)
        if outputs:
            ev = Event(now_iso(), "port_probe", "info" if result.score > -1000 else "warn", parse_line, result.__dict__)
            outputs.write_event(ev)
            outputs.append_report(ev)

    viable = [r for r in results if r.score > -1000]
    if not viable:
        errors = "; ".join(f"{r.device}: {r.error}" for r in results)
        raise RuntimeError(f"所有串口都无法打开：{errors}")
    selected = max(viable, key=lambda r: (r.score, r.bytes_read))
    if require_log_evidence and not any(keyword in selected.sample for keyword in LOG_KEYWORDS):
        raise RuntimeError(f"没有串口在 {probe_seconds:.1f}s 探测窗口内输出 ESP32 日志；最高分 {selected.device}: {selected.reason}")
    print(f"[monitor] 自动选择串口：{selected.device} ({selected.reason})", file=sys.stderr)
    return selected.device


def alert_terminal(event: Event, no_bell: bool, alert_command: str | None) -> None:
    if event.event == "dtrac_disconnected":
        print("\n" + "=" * 72, file=sys.stderr)
        print(f"DTRAC DISCONNECT MARKER  {event.ts}", file=sys.stderr)
        print(json.dumps(event.fields, ensure_ascii=False, sort_keys=True, default=json_default), file=sys.stderr)
        print(f"RAW: {event.raw}", file=sys.stderr)
        if reason_raw := event.fields.get("reason_raw"):
            print(f"REASON_RAW: {reason_raw}", file=sys.stderr)
        print("=" * 72 + "\n", file=sys.stderr)
        if not no_bell:
            print("\a", end="", file=sys.stderr, flush=True)
    elif event.event == "esp32_crash_marker":
        print("\n" + "!" * 72, file=sys.stderr)
        print(f"ESP32 CRASH MARKER  {event.ts}", file=sys.stderr)
        print(f"RAW: {event.raw}", file=sys.stderr)
        print("!" * 72 + "\n", file=sys.stderr)
        if not no_bell:
            print("\a", end="", file=sys.stderr, flush=True)
    else:
        return

    if alert_command:
        try:
            subprocess.Popen(alert_command, shell=True)
        except Exception as exc:
            print(f"[monitor] alert-command failed: {exc}", file=sys.stderr)


def handle_line(line: str, source: str, detector: EventDetector, outputs: RunOutputs, args: argparse.Namespace) -> None:
    ts = now_iso()
    outputs.write_raw(ts, line, source=source)
    if not args.no_terminal_log and source != "parse":
        print(line)
    for event in detector.process(line, ts):
        outputs.write_event(event)
        outputs.append_report(event)
        alert_terminal(event, args.no_bell, args.alert_command)


def process_bytes(buffer: bytearray, chunk: bytes, source: str, detector: EventDetector, outputs: RunOutputs, args: argparse.Namespace) -> bytearray:
    buffer.extend(chunk)
    while True:
        newline = buffer.find(b"\n")
        if newline < 0:
            break
        raw_line = bytes(buffer[:newline]).rstrip(b"\r")
        del buffer[: newline + 1]
        line = raw_line.decode(args.encoding, errors="replace")
        handle_line(line, source, detector, outputs, args)
    if len(buffer) > args.max_partial_bytes:
        raw_line = bytes(buffer)
        buffer.clear()
        line = raw_line.decode(args.encoding, errors="replace")
        handle_line(line, source, detector, outputs, args)
    return buffer


def run_live(args: argparse.Namespace, outputs: RunOutputs) -> int:
    detector = EventDetector()
    start = time.monotonic()
    deadline = start + args.duration if args.duration else None
    outputs.write_event(Event(now_iso(), "monitor_start", "info", "monitor live start", {"args": vars(args)}))

    selected_port = args.port
    while True:
        if not selected_port or selected_port.lower() == "auto":
            selected_port = auto_select_port(args.baud, args.probe_seconds, args.require_log_evidence, outputs)
        ev = Event(now_iso(), "port_selected", "info", f"selected serial port {selected_port}", {"port": selected_port, "baud": args.baud})
        outputs.write_event(ev)
        outputs.append_report(ev)
        outputs.flush()

        buffer = bytearray()
        try:
            with open_serial_safely(selected_port, args.baud, timeout=0.2) as ser:
                print(f"[monitor] 已连接 {selected_port}，DTR=0 RTS=0，只读采集日志", file=sys.stderr)
                while True:
                    if deadline and time.monotonic() >= deadline:
                        outputs.write_event(Event(now_iso(), "monitor_stop", "info", "duration reached", {"duration": args.duration}))
                        return 0
                    chunk = ser.read(4096)
                    if chunk:
                        process_bytes(buffer, chunk, "live", detector, outputs, args)
                    else:
                        time.sleep(0.02)
                    if time.monotonic() - start >= args.flush_interval:
                        outputs.flush()
                        start = time.monotonic()
        except KeyboardInterrupt:
            if buffer:
                handle_line(buffer.decode(args.encoding, errors="replace"), "live-partial", detector, outputs, args)
            outputs.write_event(Event(now_iso(), "monitor_stop", "info", "keyboard interrupt", {}))
            return 130
        except Exception as exc:
            ev = Event(now_iso(), "serial_error", "critical", f"serial error on {selected_port}: {exc}", {"port": selected_port, "error": str(exc)})
            outputs.write_event(ev)
            outputs.append_report(ev)
            print(f"[monitor] {ev.raw}", file=sys.stderr)
            if not args.reconnect:
                return 2
            time.sleep(args.reconnect_delay)
            selected_port = args.port if args.port and args.port.lower() != "auto" else "auto"


def iter_text_lines(path: Path, encoding: str) -> Any:
    with path.open("rb") as f:
        for raw in f:
            yield raw.rstrip(b"\r\n").decode(encoding, errors="replace")


def run_grep(args: argparse.Namespace, paths: list[Path]) -> int:
    patterns = [re.compile(p) for p in args.grep]
    matched = 0
    for path in paths:
        for line_no, line in enumerate(iter_text_lines(path, args.encoding), start=1):
            parse_line = RE_ANSI.sub("", line)
            if all(pattern.search(parse_line) for pattern in patterns):
                matched += 1
                print(f"{path}:{line_no}: {line}")
    return 0 if matched else 1


def run_parse_only(args: argparse.Namespace, outputs: RunOutputs, paths: list[Path], source: str = "parse") -> int:
    detector = EventDetector()
    outputs.write_event(Event(now_iso(), "monitor_start", "info", "parse-only start", {"files": [str(p) for p in paths]}))
    for path in paths:
        for line in iter_text_lines(path, args.encoding):
            handle_line(line, source, detector, outputs, args)
    outputs.write_event(Event(now_iso(), "monitor_stop", "info", "parse-only done", {"counts": dict(detector.counts)}))
    print("[monitor] event counts:", json.dumps(dict(detector.counts), ensure_ascii=False, sort_keys=True), file=sys.stderr)
    return 0


def write_self_test_log(out_dir: Path) -> Path:
    sample = out_dir / "self-test-input.log"
    sample.write_text(
        "\n".join(
            [
                "I (326) cpu_start: Multicore app",
                "I (1234) elfradio_hwnode: ElfRadio HwNode 启动中...",
                "[Rigctld] 接受连接：192.168.2.208:43640 clients_before=0 session_before=0 RX采样=LEFT",
                "[RigctldGate] cmd=\"F 145902500\" name=Some(\"set_freq\") 启动 SatSession #1",
                "[SatSession #1] 绑定本次 DTrac 会话: RX=LEFT TX=RIGHT（连接时 MAIN 作为 RX）",
                "[MenuNav] Guard2 fail #28 alive=false tx=false busy=true",
                "[SatGate #1] setup attempts exhausted",
                "[Rigctld] 10s 无命令，关闭空闲 client (read_timeout/无 DTrac F/I/S)，handler_session=Some(1)",
                "[Rigctld] 连接 192.168.2.208:43640 已关闭，handler_session=Some(1) current_session=1 剩余 0 客户端",
                "***ERROR*** A stack overflow in task pthread has been detected.",
                "[SatSession] 断开后 RX/TX SQL 关闭注入完成",
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    return sample


def build_parser() -> argparse.ArgumentParser:
    default_out = repo_root() / "logs" / "dtrac-stability"
    parser = argparse.ArgumentParser(description="ESP32 / DTrac rigctld stability log monitor")
    parser.add_argument("--port", default="auto", help="串口号；默认 auto 自动枚举和短时探测")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--out-dir", type=Path, default=default_out)
    parser.add_argument("--prefix", default=None)
    parser.add_argument("--list-ports", action="store_true")
    parser.add_argument("--parse-only", nargs="*", type=Path, help="离线解析已有 log 文件，不打开串口")
    parser.add_argument("--grep", action="append", help="只过滤输出匹配所有正则的原始行，可与 --parse-only 一起用")
    parser.add_argument("--self-test", action="store_true", help="生成内置样例并验证解析器")
    parser.add_argument("--event", dest="event_filters", action="append", help="parse-only/self-test 时在终端额外打印指定 event JSON")
    parser.add_argument("--probe-seconds", type=float, default=3.0)
    parser.add_argument("--require-log-evidence", action="store_true")
    parser.add_argument("--reconnect", action="store_true")
    parser.add_argument("--reconnect-delay", type=float, default=2.0)
    parser.add_argument("--duration", type=float, default=0.0, help="测试用运行时长秒数；0 表示一直运行")
    parser.add_argument("--no-terminal-log", action="store_true")
    parser.add_argument("--no-bell", action="store_true")
    parser.add_argument("--alert-command", default=None)
    parser.add_argument("--flush-interval", type=float, default=5.0)
    parser.add_argument("--encoding", default="utf-8")
    parser.add_argument("--max-partial-bytes", type=int, default=8192)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    if args.list_ports:
        print_ports()
        return 0

    if args.grep:
        paths = args.parse_only
        if not paths:
            latest = args.out_dir / "latest-run.txt"
            if latest.exists():
                values = dict(line.split("=", 1) for line in latest.read_text(encoding="utf-8").splitlines() if "=" in line)
                paths = [Path(values["raw"])] if "raw" in values else None
        if not paths:
            raise SystemExit("--grep 需要配合 --parse-only <log>，或先有 latest-run.txt")
        return run_grep(args, paths)

    prefix = args.prefix or file_stamp()
    outputs = RunOutputs(args.out_dir, prefix, terminal_events=args.event_filters)
    try:
        if args.self_test:
            sample = write_self_test_log(args.out_dir)
            return run_parse_only(args, outputs, [sample], source="self-test")
        if args.parse_only is not None:
            if not args.parse_only:
                raise SystemExit("--parse-only 需要至少一个 log 文件路径")
            return run_parse_only(args, outputs, args.parse_only)
        return run_live(args, outputs)
    finally:
        outputs.flush()
        outputs.close()


if __name__ == "__main__":
    raise SystemExit(main())
