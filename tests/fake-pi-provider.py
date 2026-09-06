#!/usr/bin/env python3
"""Stdlib OpenAI fixture plus an isolated local Pi RPC smoke test."""
from __future__ import annotations

import argparse
import fcntl
import http.client
import ipaddress
import json
import os
from pathlib import Path
import pty
import select
import shutil
import socket
import subprocess
import struct
import sys
import tempfile
import threading
import time
from urllib.parse import urlparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class State:
    def __init__(self, scenario: str, marker: str) -> None:
        self.scenario, self.marker = scenario, marker
        self.lock = threading.Lock()
        self.requests = 0
        self.connection_id: int | None = None
        self.streamed = threading.Event()
        self.release_stream = threading.Event()
        self.held_post_eof = threading.Event()
        self.stall_abort: threading.Event | None = None
        self.failure: str | None = None
        self.routes: dict[str, str] = {}


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address: tuple[str, int], state: State) -> None:
        self.state = state
        self.next_connection_id = 0
        self.connection_ids: dict[int, int] = {}
        self.active_connections: set[socket.socket] = set()
        super().__init__(address, Handler)

    def get_request(self):
        request, address = super().get_request()
        with self.state.lock:
            self.next_connection_id += 1
            self.connection_ids[request.fileno()] = self.next_connection_id
            self.active_connections.add(request)
        return request, address

    def close_active_connections(self) -> None:
        with self.state.lock:
            connections = list(self.active_connections)
            self.active_connections.clear()
        for connection in connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                connection.close()
            except OSError:
                pass


class ServerV6(Server):
    address_family = socket.AF_INET6

    def server_bind(self) -> None:
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        super().server_bind()

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: Server

    def log_message(self, _format: str, *_args: Any) -> None:
        pass

    def handle(self) -> None:
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            self.note_held_disconnect()

    def note_held_disconnect(self) -> None:
        if getattr(self, "stall_abort", None) is not None:
            self.stall_abort.set()
            self.server.state.held_post_eof.set()

    def chunk(self, body: bytes) -> bool:
        try:
            self.wfile.write(f"{len(body):X}\r\n".encode() + body + b"\r\n")
            self.wfile.flush()
            return True
        except (BrokenPipeError, ConnectionResetError):
            self.note_held_disconnect()
            return False

    def event(self, payload: dict[str, Any] | str) -> bool:
        data = payload if isinstance(payload, str) else json.dumps(payload, separators=(",", ":"))
        return self.chunk(f"data: {data}\n\n".encode())

    def finish(self) -> None:
        if self.event("[DONE]"):
            try:
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                self.note_held_disconnect()

    @staticmethod
    def completion(delta: dict[str, Any], reason: str | None = None) -> dict[str, Any]:
        return {"id": "vhrn-test", "object": "chat.completion.chunk", "choices": [
            {"index": 0, "delta": delta, "finish_reason": reason}
        ]}

    def do_POST(self) -> None:
        if self.path not in ("/v1/chat/completions", "/v1/completions"):
            self.send_error(404)
            return
        try:
            request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        except (ValueError, json.JSONDecodeError):
            self.send_error(400)
            return
        state = self.server.state
        model = request.get("model") if isinstance(request, dict) else None
        scenario = state.routes.get(model, state.scenario)
        with state.lock:
            state.requests += 1
            request_number = state.requests
            accepted_id = self.server.connection_ids.get(self.connection.fileno())
            if accepted_id is None:
                state.failure = "accepted socket has no connection id"
                self.send_error(500, state.failure)
                return
            if state.connection_id is None:
                state.connection_id = accepted_id
            elif scenario in ("tool", "plain") and state.connection_id != accepted_id:
                state.failure = "follow-up used another TCP connection"
                self.send_error(409, state.failure)
                return
            invalid_count = scenario == "tool" and request_number != 2 and request_number != 1
            history = request.get("messages") if isinstance(request, dict) else None
            valid_history = isinstance(history, list) and len(history) >= 2
            if valid_history:
                assistant, tool = history[-2:]
                calls = assistant.get("tool_calls") if isinstance(assistant, dict) else None
                valid_history = (assistant.get("role") == "assistant" and isinstance(calls, list)
                    and any(call.get("id") == "read-marker" and call.get("function", {}).get("name") == "read" for call in calls)
                    and isinstance(tool, dict) and tool.get("role") == "tool" and tool.get("tool_call_id") == "read-marker"
                    and state.marker in str(tool.get("content", "")))
            missing_marker = scenario == "tool" and request_number == 2 and not valid_history
        if invalid_count:
            self.send_error(409, "unexpected tool request count")
            return
        if missing_marker:
            self.send_error(422, "tool result omitted marker")
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        if scenario == "tool" and request_number == 1:
            call = {"tool_calls": [{"index": 0, "id": "read-marker", "type": "function",
                "function": {"name": "read", "arguments": json.dumps({"path": "marker.txt"})}}]}
            self.event(self.completion(call))
            self.event(self.completion({}, "tool_calls"))
            self.finish()
            return
        if scenario in ("policy", "final"):
            self.event(self.completion({"content": "COMPLETE"}))
            self.event(self.completion({}, "stop"))
            self.finish()
            return
        if scenario == "stall":
            self.stall_abort = threading.Event()
            with state.lock:
                state.stall_abort = self.stall_abort
            if not self.event(self.completion({"content": "STREAM-"})):
                return
            state.streamed.set()
            until = time.monotonic() + 20
            while time.monotonic() < until and not self.stall_abort.is_set():
                if not self.event(self.completion({"content": "."})):
                    return
                time.sleep(.1)
            if not self.stall_abort.is_set():
                state.failure = "held POST was not disconnected within 20 seconds"
                self.close_connection = True
            return
        if not self.event(self.completion({"content": "STREAM-"})):
            return
        state.streamed.set()
        # Only the RPC driver releases this barrier after it receives text_delta.
        if not state.release_stream.wait(20):
            state.failure = "RPC driver did not observe STREAM- before provider timeout"
            self.close_connection = True
            return
        self.event(self.completion({"content": "COMPLETE"}))
        self.event(self.completion({}, "stop"))
        self.finish()


def start_provider(scenario: str, marker: str, host: str = "127.0.0.1") -> Server:
    server_type = ServerV6 if ":" in host else Server
    server = server_type((host, 0), State(scenario, marker))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def self_test() -> int:
    server = start_provider("tool", "self-test-marker")
    conn: http.client.HTTPConnection | None = None
    try:
        conn = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
        conn.request("POST", "/v1/chat/completions", json.dumps({"stream": True}), {"Content-Type": "application/json"})
        assert conn.getresponse().read().find(b"tool_calls") >= 0
        followup = {"stream": True, "messages": [{"role": "assistant", "tool_calls": [{"id": "read-marker", "function": {"name": "read"}}]}, {"role": "tool", "tool_call_id": "read-marker", "content": "self-test-marker"}]}
        conn.request("POST", "/v1/chat/completions", json.dumps(followup), {"Content-Type": "application/json"})
        response = conn.getresponse()
        first = response.readline()
        assert b"STREAM-" in first and server.state.streamed.is_set(), first
        server.state.release_stream.set()
        body = first + response.read()
        assert b"COMPLETE" in body and b"[DONE]" in body, body
        assert server.state.requests == 2 and server.state.connection_id is not None
        print("PASS fake provider: chunked SSE, streamed gate, same TCP connection")
        return 0
    finally:
        if conn is not None:
            conn.close()
        server.close_active_connections(); server.shutdown()
        server.server_close()


class RpcSession:
    def __init__(self, command: list[str], cwd: Path, env: dict[str, str]) -> None:
        self.master, slave = pty.openpty()
        import termios
        attrs = termios.tcgetattr(slave)
        attrs[3] &= ~(termios.ECHO | termios.ICANON)
        termios.tcsetattr(slave, termios.TCSANOW, attrs)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        self.proc = subprocess.Popen(command, cwd=cwd, env=env, stdin=slave, stdout=slave,
                                     stderr=slave, start_new_session=True)
        os.close(slave)
        self.buffer, self.output = bytearray(), bytearray()

    def send(self, request_id: str, command_type: str, **fields: Any) -> None:
        request: dict[str, Any] = {"id": request_id, "type": command_type, **fields}
        os.write(self.master, (json.dumps(request) + "\n").encode())

    def events(self, deadline: float):
        while time.monotonic() < deadline:
            # A response can arrive alongside the preceding request.  Consume that
            # already-buffered line before waiting for another PTY readiness edge.
            while b"\n" in self.buffer:
                raw, _, tail = self.buffer.partition(b"\n")
                self.buffer = bytearray(tail)
                try:
                    yield json.loads(raw.rstrip(b"\r"))
                except json.JSONDecodeError:
                    pass  # Engine progress is allowed on the PTY before RPC starts.
            readable, _, _ = select.select([self.master], [], [], min(.2, deadline - time.monotonic()))
            if not readable:
                if self.proc.poll() is not None:
                    break
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError:
                break
            if not data:
                break
            self.output.extend(data); self.buffer.extend(data)
        while b"\n" in self.buffer:
            raw, _, tail = self.buffer.partition(b"\n")
            self.buffer = bytearray(tail)
            try:
                yield json.loads(raw.rstrip(b"\r"))
            except json.JSONDecodeError:
                pass

    def close(self) -> bool:
        forced_kill = False
        if self.proc.poll() is None:
            self.proc.terminate()
            deadline = time.monotonic() + 15
            while self.proc.poll() is None and time.monotonic() < deadline:
                readable, _, _ = select.select([self.master], [], [], .1)
                if readable:
                    try:
                        data = os.read(self.master, 65536)
                    except OSError:
                        data = b""
                    self.output.extend(data)
            if self.proc.poll() is None:
                forced_kill = True
                self.proc.kill(); self.proc.wait()
        os.close(self.master)
        return forced_kill

    def wait(self, timeout: float) -> int:
        for _event in self.events(time.monotonic() + timeout):
            pass
        return self.proc.wait(timeout=max(.1, timeout))

    def wait_text(self, text: bytes, deadline: float) -> bool:
        while time.monotonic() < deadline:
            if text in self.output:
                return True
            readable, _, _ = select.select([self.master], [], [], min(.2, deadline - time.monotonic()))
            if not readable:
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return text in self.output
            if not data:
                return text in self.output
            self.output.extend(data)
        return text in self.output


def nested(event: dict[str, Any]) -> dict[str, Any]:
    return event.get("event") if isinstance(event.get("event"), dict) else event


def process_line(process: subprocess.Popen[bytes], deadline: float) -> str:
    """Read one unbuffered driver line within its deadline."""
    assert process.stdout is not None
    line = bytearray()
    while time.monotonic() < deadline:
        readable, _, _ = select.select([process.stdout], [], [], min(.2, deadline - time.monotonic()))
        if readable:
            byte = os.read(process.stdout.fileno(), 1)
            if not byte:
                return line.decode(errors="replace").strip()
            if byte == b"\n":
                result = line.decode(errors="replace").strip()
                if result:
                    return result
                line.clear()
            else:
                line.extend(byte)
        if process.poll() is not None:
            break
    return ""


class Runtime:
    """An isolated host environment shared by one or more Pi project sessions."""
    def __init__(self, engine: str) -> None:
        self.engine = engine; self.repo = Path(__file__).resolve().parents[1]
        self.binary = Path(os.environ.get("VHRN_BIN", self.repo / "target/debug/vhrn"))
        self.actual_engine = shutil.which(engine); self.original = dict(os.environ)
        if not self.binary.is_file() or not self.actual_engine:
            raise RuntimeError("missing vhrn binary or engine")
        self.temp = tempfile.TemporaryDirectory(prefix="pi-local-e2e.", dir=self.repo / "target")
        self.root = Path(self.temp.name); self.wrappers: list[RpcSession] = []; self.providers: list[Server] = []
        for name in ("home", "config", "cache", "state", "gh", "tmp", "bin"):
            (self.root / name).mkdir(mode=0o700)
        agent = self.root / "home/.pi/agent"; agent.mkdir(parents=True)
        (agent / "settings.json").write_text(json.dumps({"theme": "dark"}))
        (agent / "keybindings.json").write_text(json.dumps({}))
        restored = {k: self.original.get(k) for k in ("HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME")}
        shim = self.root / "bin" / engine
        shim.write_text("#!" + sys.executable + "\nimport os,sys\ne=dict(os.environ); r=" + repr(restored) + "\nfor k,v in r.items():\n if v is None:e.pop(k,None)\n else:e[k]=v\nos.execve(" + repr(self.actual_engine) + ",[" + repr(self.actual_engine) + "]+sys.argv[1:],e)\n")
        shim.chmod(0o700)
        self.env = {k: self.original[k] for k in ("PATH", "LANG", "LC_ALL", "USER", "LOGNAME") if k in self.original}
        self.env.update({"PATH": str(self.root / "bin") + os.pathsep + self.original["PATH"], "HOME": str(self.root / "home"), "XDG_CONFIG_HOME": str(self.root / "config"), "XDG_CACHE_HOME": str(self.root / "cache"), "XDG_STATE_HOME": str(self.root / "state"), "GH_CONFIG_DIR": str(self.root / "gh"), "TMPDIR": str(self.root / "tmp"), "TERM": "xterm-256color", "VHRN_ENGINE": engine, "VHRN_PROXY_IMAGE": "vhrn-proxy:local"})
        if engine == "docker": self.env["DOCKER_CONTEXT"] = "colima"

    def project(self, name: str, marker: str | None = None) -> Path:
        project = self.root / name; project.mkdir()
        if marker: (project / "marker.txt").write_text(marker)
        return project

    def provider(self, scenario: str, marker: str, host: str = "127.0.0.1") -> Server:
        server = start_provider(scenario, marker, host); self.providers.append(server); return server

    def models(self, server: Server, authority_override: str = "localhost") -> None:
        model = lambda name: {"id": name, "name": name, "contextWindow": 8192, "maxTokens": 512, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}}
        authority = authority_override if ":" not in authority_override else f"[{authority_override}]"
        data = {"providers": {"vhrn-test": {"baseUrl": f"http://{authority}:{server.server_port}/v1", "api": "openai-completions", "apiKey": "dummy", "models": [model(name) for name in ("tool", "policy", "stall", "final")]}}}
        agent = self.root / "home/.pi/agent"; agent.mkdir(parents=True, exist_ok=True); (agent / "models.json").write_text(json.dumps(data))

    def version(self, harness: str, project: Path) -> None:
        env = dict(self.env); env["VHRN_IMAGE"] = f"vhrn-{harness}:local"
        session = RpcSession([str(self.binary), harness, "--", "--version"], project, env)
        self.wrappers.append(session)
        try:
            if session.wait(100): raise RuntimeError(session.output.decode(errors="replace"))
        finally: self.close_session(session)

    def rpc(self, project: Path, model: str, tools: str | None = None, local_args: tuple[str, ...] = (), session_dir: str | None = None, persist: bool = False, provider: str = "vhrn-test", allow_domains: tuple[str, ...] = ()) -> RpcSession:
        args = [str(self.binary), "pi"]
        for domain in allow_domains:
            args.extend(["--allow", domain])
        for authority in local_args:
            args.extend(["--allow", "--local", authority])
        args.extend(["--", "--mode", "rpc", "--provider", provider, "--model", model])
        if persist:
            if session_dir is not None:
                raise RuntimeError("persistent Pi sessions cannot also set --session-dir")
        elif session_dir is None:
            args.append("--no-session")
        else:
            args.extend(["--session-dir", session_dir])
        if tools: args.extend(["--tools", tools])
        env = dict(self.env); env["VHRN_IMAGE"] = "vhrn-pi:local"
        session = RpcSession(args, project, env); self.wrappers.append(session); return session

    def interactive(self, project: Path, model: str, local_args: tuple[str, ...]) -> RpcSession:
        args = [str(self.binary), "pi"]
        for authority in local_args:
            args.extend(["--allow", "--local", authority])
        args.extend(["--", "--no-session", "--provider", "vhrn-test", "--model", model])
        env = dict(self.env); env["VHRN_IMAGE"] = "vhrn-pi:local"
        session = RpcSession(args, project, env); self.wrappers.append(session); return session

    def net(self, *args: str) -> str:
        result = subprocess.run([str(self.binary), "net", *args], env=self.env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
        output = result.stdout.decode(errors="replace")
        if result.returncode: raise RuntimeError(output)
        return output

    def wait_loopback_visible(self, session: RpcSession, endpoint: str) -> float:
        """Wait for a fresh CONNECT to observe an atomically replaced policy file."""
        started, deadline, last = time.monotonic(), time.monotonic() + 10, "no result"
        script = "const n=require('net'),u=new URL(process.env.HTTP_PROXY),a=" + json.dumps(endpoint) + ";const s=n.connect(+u.port,u.hostname,()=>s.write('CONNECT '+a+' HTTP/1.1\\r\\nHost: '+a+'\\r\\n\\r\\n'));s.once('data',b=>{console.log(b.toString().split('\\r\\n')[0]);s.destroy()});s.once('error',e=>console.log('ERR '+e.code));"
        env = {**self.original, **({"DOCKER_CONTEXT": "colima"} if self.engine == "docker" else {})}
        while time.monotonic() < deadline:
            result = subprocess.run([self.actual_engine, "exec", f"vhrn-agent-{session.proc.pid}", "node", "-e", script], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=5)
            last = result.stdout.decode(errors="replace").strip()
            if last.startswith("HTTP/1.1 200"):
                return time.monotonic() - started
            time.sleep(.1)
        raise RuntimeError(f"loopback grant was not visible within 10s ({last})")

    def proxy_broker_address(self, session: RpcSession) -> str:
        env = {**self.original, **({"DOCKER_CONTEXT": "colima"} if self.engine == "docker" else {})}
        command = [self.actual_engine, "inspect", f"vhrn-proxy-{session.proc.pid}"]
        if self.engine == "docker":
            command[2:2] = ["--format", "{{json .Config.Env}}"]
        result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
        if result.returncode:
            raise RuntimeError("could not inspect proxy environment")
        # Apple prints full JSON while Docker can template the environment.  Inspect only
        # the advertised address and never copy or print the rest of the proxy environment.
        import re
        match = re.search(r"VHRN_BROKER_ADDR=([^\"\\\\\s]+)", result.stdout.decode(errors="replace"))
        if not match:
            raise RuntimeError("proxy did not receive VHRN_BROKER_ADDR")
        return match.group(1)

    def raw_connect(self, session: RpcSession, authority: str) -> tuple[str, float]:
        """Ask the agent-side proxy for CONNECT and bound the proxy connection itself."""
        script = """const n=require('net'),u=new URL(process.env.HTTP_PROXY),a=%s;
const t=Date.now(),s=n.connect(+u.port,u.hostname,()=>s.write('CONNECT '+a+' HTTP/1.1\\r\\nHost: '+a+'\\r\\n\\r\\n'));
const done=x=>{console.log(x+' '+(Date.now()-t));s.destroy()};
s.setTimeout(5000,()=>done('TIMEOUT'));s.once('data',b=>done(b.toString().split('\\r\\n')[0]));s.once('error',e=>done('ERR-'+e.code));""" % json.dumps(authority)
        env = {**self.original, **({"DOCKER_CONTEXT": "colima"} if self.engine == "docker" else {})}
        result = subprocess.run(
            [self.actual_engine, "exec", f"vhrn-agent-{session.proc.pid}", "node", "-e", script],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10,
        )
        if result.returncode:
            raise RuntimeError("raw CONNECT helper failed")
        parts = result.stdout.decode(errors="replace").strip().rsplit(" ", 1)
        if len(parts) != 2 or not parts[1].isdigit():
            raise RuntimeError("raw CONNECT returned malformed result")
        return parts[0], float(parts[1]) / 1000

    def plain_http_reuse(self, session: RpcSession, url: str) -> subprocess.Popen[bytes]:
        """Use the running agent's real proxy for two absolute-form HTTP requests."""
        script = r'''const http=require("http"),p=new URL(process.env.HTTP_PROXY),target=%s;
const body=JSON.stringify({model:"final",stream:true,messages:[{role:"user",content:"test"}]}),agent=new http.Agent({keepAlive:true,maxSockets:1});
function post(first) {
  const q=http.request({hostname:p.hostname,port:+p.port,method:"POST",path:target,agent,headers:{host:new URL(target).host,"content-type":"application/json","content-length":Buffer.byteLength(body)}},r=>{
    if (r.statusCode !== 200) { console.log("STATUS "+r.statusCode); r.resume(); agent.destroy(); return; }
    if (first) {
      r.once("data",chunk=>{ console.log("FIRST "+chunk.toString()); r.pause(); process.stdin.once("data",()=>{process.stdin.destroy();r.resume()}); });
      r.once("end",()=>post(false));
    } else { r.resume(); r.once("end",()=>{ console.log("SECOND"); agent.destroy(); }); }
  });
  q.on("error",()=>{console.log("ERR");process.exitCode=1}); q.end(body);
}
post(true);''' % json.dumps(url)
        env = {**self.original, **({"DOCKER_CONTEXT": "colima"} if self.engine == "docker" else {})}
        return subprocess.Popen([self.actual_engine, "exec", "-i", f"vhrn-agent-{session.proc.pid}", "node", "-e", script], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def plain_http_status(self, session: RpcSession, url: str) -> str:
        script = r'''const http=require("http"),p=new URL(process.env.HTTP_PROXY),target=%s;
const body=JSON.stringify({model:"policy",stream:true,messages:[{role:"user",content:"test"}]}),q=http.request({hostname:p.hostname,port:+p.port,method:"POST",path:target,headers:{host:new URL(target).host,"content-type":"application/json","content-length":Buffer.byteLength(body)}},r=>{console.log(r.statusCode);r.resume()});q.on("error",()=>console.log("ERR"));q.end(body);''' % json.dumps(url)
        env = {**self.original, **({"DOCKER_CONTEXT": "colima"} if self.engine == "docker" else {})}
        result = subprocess.run([self.actual_engine, "exec", f"vhrn-agent-{session.proc.pid}", "node", "-e", script], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=15)
        return result.stdout.decode(errors="replace").strip()

    def broker_check_address(self, session: RpcSession) -> tuple[str, int]:
        value = self.proxy_broker_address(session)
        host, port = value.rsplit(":", 1)
        if not port.isdigit():
            raise RuntimeError("proxy broker address has no numeric port")
        # Apple advertises its host-reachable gateway address. Colima advertises
        # host.docker.internal to the proxy, while the host reaches the listener via loopback.
        return ("127.0.0.1" if self.engine == "docker" else host, int(port))

    @staticmethod
    def broker_refused(address: tuple[str, int]) -> bool:
        try:
            with socket.create_connection(address, timeout=2):
                return False
        except ConnectionRefusedError:
            return True

    @staticmethod
    def ready(session: RpcSession, expose_output: bool = True) -> None:
        session.send("state", "get_state")
        if not any(isinstance(e, dict) and e.get("id") == "state" and (e.get("result") is not None or e.get("success") is True) for e in session.events(time.monotonic() + 45)):
            detail = ": " + session.output.decode(errors="replace") if expose_output else ""
            raise RuntimeError("Pi RPC get_state failed" + detail)
        session.send("retry", "set_auto_retry", enabled=False)
        if not any(isinstance(e, dict) and e.get("id") == "retry" and (e.get("result") is not None or e.get("success") is True) for e in session.events(time.monotonic() + 15)):
            raise RuntimeError("Pi did not accept set_auto_retry" if not expose_output else "Pi did not accept set_auto_retry: " + session.output.decode(errors="replace"))

    @staticmethod
    def state(session: RpcSession, request_id: str = "state-file") -> dict[str, Any]:
        session.send(request_id, "get_state")
        for event in session.events(time.monotonic() + 15):
            if isinstance(event, dict) and event.get("id") == request_id and event.get("success") is True:
                data = event.get("data", event.get("result", {}))
                return data if isinstance(data, dict) else {}
        raise RuntimeError("Pi did not return state: " + session.output.decode(errors="replace"))

    @staticmethod
    def command(session: RpcSession, request_id: str, command_type: str, **fields: Any) -> None:
        session.send(request_id, command_type, **fields)
        if not any(isinstance(event, dict) and event.get("id") == request_id and event.get("success") is True for event in session.events(time.monotonic() + 15)):
            raise RuntimeError(f"Pi did not accept {command_type}: " + session.output.decode(errors="replace"))

    def close_session(self, session: RpcSession) -> None:
        forced_kill = session.close()  # terminate the wrapper first; vhrn should remove both containers itself.
        failures: list[str] = []
        if forced_kill:
            failures.append("wrapper required SIGKILL during cleanup")
        for kind in ("agent", "proxy"):
            name = f"vhrn-{kind}-{session.proc.pid}"
            env = {**self.original, **({"DOCKER_CONTEXT": "colima"} if self.engine == "docker" else {})}
            inspect = subprocess.run([self.actual_engine, "inspect", name], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
            absent = inspect.returncode != 0 and any(text in inspect.stdout.decode(errors="replace").lower() for text in ("not found", "notfound", "no such object", "no such container"))
            if not absent:
                failures.append(f"{name} survived graceful cleanup" if inspect.returncode == 0 else f"could not confirm {name} absent")
                remove = [self.actual_engine, "delete", "--force"] if self.engine == "container" else [self.actual_engine, "rm", "--force"]
                subprocess.run(remove + [name], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20)
                final = subprocess.run([self.actual_engine, "inspect", name], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
                final_absent = final.returncode != 0 and any(text in final.stdout.decode(errors="replace").lower() for text in ("not found", "notfound", "no such object", "no such container"))
                if not final_absent:
                    failures.append(f"{name} survived forced cleanup")
        self.wrappers.remove(session)
        if failures: raise RuntimeError("; ".join(failures))

    def close(self) -> None:
        errors: list[BaseException] = []
        for session in self.wrappers[:]:
            try: self.close_session(session)
            except BaseException as error: errors.append(error)
        for server in self.providers:
            server.state.release_stream.set(); server.close_active_connections(); server.shutdown(); server.server_close()
        for directory in (self.root / "cache/vhrn/broker", self.root / "state/vhrn/net/runs"):
            deadline = time.monotonic() + 5
            while directory.exists() and any(directory.iterdir()) and time.monotonic() < deadline:
                time.sleep(.1)
            if directory.exists() and any(directory.iterdir()): errors.append(RuntimeError(f"residual runtime state: {directory}"))
        self.temp.cleanup()
        if errors: raise errors[0]


def stop_reason(session: RpcSession, timeout: float = 75) -> str | None:
    reason: str | None = None
    for event in session.events(time.monotonic() + timeout):
        payload = nested(event) if isinstance(event, dict) else {}
        if payload.get("type") == "message_end" and payload.get("message", {}).get("role") == "assistant":
            reason = payload["message"].get("stopReason")
        if payload.get("type") == "agent_settled":
            return reason
    raise RuntimeError("Pi did not settle: " + session.output.decode(errors="replace"))


def abort_and_settle(session: RpcSession) -> None:
    session.send("abort", "abort")
    response = settled = False
    for event in session.events(time.monotonic() + 15):
        payload = nested(event) if isinstance(event, dict) else {}
        response = response or (isinstance(event, dict) and event.get("id") == "abort" and event.get("success") is True)
        settled = settled or payload.get("type") == "agent_settled"
        if response and settled: return
    raise RuntimeError("abort did not respond and settle: " + session.output.decode(errors="replace"))


def state_not_streaming(session: RpcSession) -> None:
    session.send("post-abort-state", "get_state")
    for event in session.events(time.monotonic() + 15):
        if isinstance(event, dict) and event.get("id") == "post-abort-state" and event.get("success") is True:
            if event.get("data", {}).get("isStreaming") is False: return
            break
    raise RuntimeError("Pi remained streaming after abort: " + session.output.decode(errors="replace"))


def endpoint_cases(fixture: Runtime) -> None:
    project = fixture.project("project-endpoints")
    rows = [
        ("ipv4-localhost", "127.0.0.1", "localhost", "localhost", True),
        ("ipv4-numeric", "127.0.0.1", "127.0.0.1", "127.0.0.1", True),
        ("ipv6-localhost-fallback", "::1", "localhost", "localhost", True),
        ("ipv6-numeric", "::1", "::1", "[::1]", True),
        ("ipv6-denied-alias", "::1", "::1", "localhost", False),
    ]
    for name, host, url_host, grant_host, allowed in rows:
        server = fixture.provider("final", "unused", host)
        fixture.models(server, url_host)
        grant = f"{grant_host}:{server.server_port}"
        rpc = fixture.rpc(project, "final", local_args=(grant,))
        try:
            fixture.ready(rpc)
            if not allowed:
                status, elapsed = fixture.raw_connect(rpc, f"[::1]:{server.server_port}")
                if status != "HTTP/1.1 403 Forbidden" or elapsed > 5:
                    raise RuntimeError(f"{name} proxy denial was {status!r} after {elapsed:.2f}s")
            rpc.send(name, "prompt", message="Reply with COMPLETE.")
            reason = stop_reason(rpc)
            if allowed:
                if reason != "stop" or server.state.requests != 1:
                    raise RuntimeError(f"{name} expected one POST, got {server.state.requests}: " + rpc.output.decode(errors="replace"))
            elif reason != "error" or server.state.requests != 0:
                raise RuntimeError(f"{name} reached forbidden provider: " + rpc.output.decode(errors="replace"))
        finally:
            fixture.close_session(rpc)

    server = fixture.provider("final", "unused")
    fixture.models(server, "127.0.0.1")
    rpc = fixture.rpc(project, "final", local_args=(f"127.0.0.1:{server.server_port}",))
    try:
        fixture.ready(rpc, expose_output=False)
        status, elapsed = fixture.raw_connect(rpc, f"127.0.0.1:{server.server_port}")
        if status.lower() != "http/1.1 200 connection established" or elapsed > 5:
            raise RuntimeError(f"granted numeric endpoint was {status!r} after {elapsed:.2f}s")
        status, elapsed = fixture.raw_connect(rpc, f"127.0.0.2:{server.server_port}")
        if status != "HTTP/1.1 403 Forbidden" or elapsed > 5:
            raise RuntimeError(f"numeric alias denial was {status!r} after {elapsed:.2f}s")
        if server.state.requests:
            raise RuntimeError("numeric alias raw CONNECT reached the provider")
    finally:
        fixture.close_session(rpc)

    # IPv4 and IPv6 can share a port while remaining distinct authorities.  This
    # proves an IPv4 grant cannot authorize the loopback IPv6 listener.
    v4 = fixture.provider("final", "unused", "127.0.0.1")
    v6 = ServerV6(("::1", v4.server_port), State("final", "unused"))
    fixture.providers.append(v6); threading.Thread(target=v6.serve_forever, daemon=True).start()
    fixture.models(v4, "127.0.0.1")
    rpc = fixture.rpc(project, "final", local_args=(f"127.0.0.1:{v4.server_port}",))
    try:
        fixture.ready(rpc)
        status, elapsed = fixture.raw_connect(rpc, f"[::1]:{v4.server_port}")
        if status != "HTTP/1.1 403 Forbidden" or elapsed > 5 or v6.state.requests:
            raise RuntimeError("IPv4 grant authorized the same-port IPv6 endpoint")
        rpc.send("same-port-ipv4", "prompt", message="Reply with COMPLETE.")
        if stop_reason(rpc) != "stop" or v4.state.requests != 1:
            raise RuntimeError("same-port IPv4 endpoint was not reachable exactly once")
    finally:
        fixture.close_session(rpc)


def signal_cases(fixture: Runtime) -> None:
    project = fixture.project("project-signals")
    for signal_name, terminate in (("SIGTERM", True), ("SIGKILL", False)):
        server = fixture.provider("stall", "unused")
        fixture.models(server)
        rpc = fixture.rpc(project, "stall", local_args=(f"localhost:{server.server_port}",))
        fixture.ready(rpc, expose_output=False)
        rpc.send("stall", "prompt", message="Stream until wrapper signal.")
        if not any(nested(event).get("type") == "message_update" and nested(event).get("assistantMessageEvent", {}).get("delta") == "STREAM-" for event in rpc.events(time.monotonic() + 30)):
            raise RuntimeError(f"{signal_name} stall did not stream")
        address = fixture.broker_check_address(rpc)
        if fixture.broker_refused(address):
            raise RuntimeError(f"{signal_name} broker listener was not open before signal")
        if terminate:
            rpc.proc.terminate()
        else:
            rpc.proc.kill()
        rpc.proc.wait(timeout=30)
        if not server.state.held_post_eof.wait(10):
            raise RuntimeError(f"{signal_name} did not close the held POST")
        if not fixture.broker_refused(address):
            raise RuntimeError(f"{signal_name} left broker listener open")
        if terminate:
            fixture.close_session(rpc)
            for directory in (fixture.root / "cache/vhrn/broker", fixture.root / "state/vhrn/net/runs"):
                if directory.exists() and any(directory.iterdir()):
                    raise RuntimeError(f"SIGTERM retained runtime state: {directory}")
            continue

        # SIGKILL bypasses wrapper teardown. Its deliberately manual cleanup is not evidence
        # that SIGTERM teardown works, so keep it separate and label it in the result.
        env = {**fixture.original, **({"DOCKER_CONTEXT": "colima"} if fixture.engine == "docker" else {})}
        names = [f"vhrn-{kind}-{rpc.proc.pid}" for kind in ("agent", "proxy")]
        present = [name for name in names if subprocess.run([fixture.actual_engine, "inspect", name], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20).returncode == 0]
        if not present:
            raise RuntimeError("SIGKILL unexpectedly left no named containers to manually clean")
        remove = [fixture.actual_engine, "delete", "--force"] if fixture.engine == "container" else [fixture.actual_engine, "rm", "--force"]
        for name in present:
            subprocess.run(remove + [name], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20, check=True)
        rpc.close()
        fixture.wrappers.remove(rpc)
        for directory in (fixture.root / "cache/vhrn/broker", fixture.root / "state/vhrn/net/runs"):
            if directory.exists():
                shutil.rmtree(directory)
        print("SIGKILL sockets closed; manual test cleanup completed", flush=True)


def session_cases(fixture: Runtime) -> None:
    server = fixture.provider("final", "unused")
    fixture.models(server)
    projects = [fixture.project("project-session-a"), fixture.project("project-session-b")]
    expected: list[Path] = []
    for index, project in enumerate(projects):
        rpc = fixture.rpc(project, "final", local_args=(f"localhost:{server.server_port}",), persist=True)
        try:
            fixture.ready(rpc)
            rpc.send(f"session-{index}", "prompt", message="Reply with COMPLETE.")
            if stop_reason(rpc) != "stop":
                raise RuntimeError("session prompt did not settle")
            state = Runtime.state(rpc, f"session-state-{index}")
            session_file = state.get("sessionFile")
            if not isinstance(session_file, str) or not session_file:
                raise RuntimeError("Pi get_state did not report a sessionFile")
            key = "".join(char if char.isascii() and char.isalnum() else "-" for char in str(project))
            container_root = "/home/dev/.pi/agent-sessions/"
            if not session_file.startswith(container_root):
                raise RuntimeError("Pi sessionFile was outside the per-project session mount")
            expected.append(fixture.root / "cache/vhrn/state" / f"pi-sessions/{key}" / session_file.removeprefix(container_root))
        finally:
            fixture.close_session(rpc)
    if expected[0] == expected[1] or any(not path.is_file() or not path.stat().st_size for path in expected):
        raise RuntimeError("Pi session files were not nonempty and partitioned")

    override = projects[0] / ".pi-test-sessions"
    rpc = fixture.rpc(projects[0], "final", local_args=(f"localhost:{server.server_port}",), session_dir=str(override))
    try:
        fixture.ready(rpc, expose_output=False)
        rpc.send("session-override", "prompt", message="Reply with COMPLETE.")
        if stop_reason(rpc) != "stop":
            raise RuntimeError("session-dir override prompt did not settle")
        state = Runtime.state(rpc, "session-override-state")
        actual = Path(str(state.get("sessionFile", "")))
        if override not in actual.parents or not actual.is_file() or not actual.stat().st_size:
            raise RuntimeError("Pi did not honor --session-dir override")
    finally:
        fixture.close_session(rpc)


def persistence_cases(fixture: Runtime) -> None:
    """Exercise Pi's bootstrap migration and disposable sync contract with real startup."""
    project = fixture.project("project-persistence")
    server = fixture.provider("final", "unused")
    agent = fixture.root / "home/.pi/agent"
    forbidden = ("host-api-key-canary", "host-auth-canary", "host-oauth-canary", "host-trust-canary")
    # These are Pi's real pre-migration shapes.  The key binding must be migrated
    # by Pi itself; a made-up key would merely prove that JSON was copied.
    agent.joinpath("settings.json").write_text(json.dumps({
        "theme": "dark", "apiKeys": {"openai": "host-api-key-canary"},
        "defaultProjectTrust": "always",
    }))
    agent.joinpath("keybindings.json").write_text(json.dumps({"cursorLeft": ["ctrl+b"]}))
    agent.joinpath("auth.json").write_text(json.dumps({"token": "host-auth-canary"}))
    agent.joinpath("oauth.json").write_text(json.dumps({"token": "host-oauth-canary"}))
    agent.joinpath("trust.json").write_text(json.dumps({"project": "host-trust-canary"}))
    fixture.models(server)
    rpc = fixture.rpc(project, "final", local_args=(f"localhost:{server.server_port}",))
    try:
        fixture.ready(rpc, expose_output=False)
    finally:
        fixture.close_session(rpc)
    state = fixture.root / "cache/vhrn/state/pi"
    settings = json.loads((state / "settings.json").read_text())
    if settings.get("theme") != "dark" or "apiKeys" in settings or "defaultProjectTrust" in settings:
        raise RuntimeError("Pi bootstrap did not preserve preferences and scrub host security settings")
    bindings = json.loads((state / "keybindings.json").read_text())
    if bindings.get("tui.editor.cursorLeft") != ["ctrl+b"] or "cursorLeft" in bindings:
        raise RuntimeError("Pi did not migrate the legacy cursorLeft binding: " + ",".join(sorted(bindings)))
    for path in state.rglob("*"):
        if path.is_file():
            text = path.read_text(errors="replace")
            if any(canary in text for canary in forbidden):
                raise RuntimeError(f"host-only Pi data leaked into state file category {path.name}")

    # State is container-owned after bootstrap.  Change it, then change the host
    # sources that are deliberately disposable mirrors and prove each side wins.
    settings["theme"] = "light"; (state / "settings.json").write_text(json.dumps(settings))
    bindings["tui.editor.cursorLeft"] = ["alt+left"]; (state / "keybindings.json").write_text(json.dumps(bindings))
    agent.joinpath("settings.json").write_text(json.dumps({"theme": "host-second-run"}))
    agent.joinpath("keybindings.json").write_text(json.dumps({"cursorLeft": ["ctrl+f"]}))
    sentinel = state / "packages" / "vhrn-state-sentinel" / "package.json"
    sentinel.parent.mkdir(parents=True, exist_ok=True); sentinel.write_text(json.dumps({"name": "vhrn-state-sentinel"}))
    models = agent / "models.json"; host_models = models.read_text() + "\n"
    models.write_text(host_models)
    prompts = agent / "prompts"; prompts.mkdir(exist_ok=True); (prompts / "host-prompt.md").write_text("host mirror refresh\n")
    rpc = fixture.rpc(project, "final", local_args=(f"localhost:{server.server_port}",))
    try:
        fixture.ready(rpc, expose_output=False)
    finally:
        fixture.close_session(rpc)
    mirror = fixture.root / "cache/vhrn/sandbox/pi/models.json"
    if mirror.read_text() != host_models:
        raise RuntimeError("disposable Pi models mirror did not refresh")
    if json.loads((state / "settings.json").read_text()).get("theme") != "light":
        raise RuntimeError("container-owned Pi settings were overwritten by host sync")
    if json.loads((state / "keybindings.json").read_text()).get("tui.editor.cursorLeft") != ["alt+left"]:
        raise RuntimeError("container-owned Pi keybindings were overwritten by host sync")
    if not sentinel.is_file() or not sentinel.stat().st_size:
        raise RuntimeError("container-owned Pi package state did not persist")
    if not (fixture.root / "cache/vhrn/sandbox/pi/prompts/host-prompt.md").is_file():
        raise RuntimeError("disposable Pi prompt mirror did not refresh")


def trust_cases(fixture: Runtime) -> None:
    """An interactive Pi decision must override filtered host trust defaults."""
    project = fixture.project("project-trust")
    (project / ".pi").mkdir(); (project / ".pi/settings.json").write_text("{}\n")
    server = fixture.provider("final", "unused")
    agent = fixture.root / "home/.pi/agent"
    agent.joinpath("settings.json").write_text(json.dumps({"theme": "dark", "defaultProjectTrust": "always"}))
    agent.joinpath("trust.json").write_text(json.dumps({str(project): True}))
    fixture.models(server)
    interactive = fixture.interactive(project, "final", (f"localhost:{server.server_port}",))
    try:
        if not interactive.wait_text(b"Trust project folder?", time.monotonic() + 45):
            raise RuntimeError("Pi did not show its interactive project trust panel")
        os.write(interactive.master, b"\x1b[B\x1b[B\x1b[B\r")
        trust = fixture.root / "cache/vhrn/state/pi/trust.json"
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if trust.is_file():
                try:
                    values = json.loads(trust.read_text())
                    if values.get(str(project)) is False:
                        break
                except json.JSONDecodeError:
                    pass
            time.sleep(.1)
        else:
            raise RuntimeError("Pi did not persist the declined project trust decision")
    finally:
        fixture.close_session(interactive)
    rpc = fixture.rpc(project, "final", local_args=(f"localhost:{server.server_port}",))
    try:
        fixture.ready(rpc, expose_output=False)
        if json.loads((fixture.root / "cache/vhrn/state/pi/trust.json").read_text()).get(str(project)) is not False:
            raise RuntimeError("Pi trust decision was overwritten after restart")
    finally:
        fixture.close_session(rpc)


def remote_case(fixture: Runtime, kind: str) -> None:
    prefix = "VHRN_PI_REMOTE_" + kind.upper() + "_"
    url, model, key = (os.environ[prefix + name] for name in ("URL", "MODEL", "KEY"))
    parsed = urlparse(url)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
        raise RuntimeError("remote endpoint must be an HTTPS hostname without URL credentials")
    try:
        address = ipaddress.ip_address(parsed.hostname)
        if address.is_private or address.is_loopback or address.is_link_local or address.is_reserved:
            raise RuntimeError("remote endpoint must be a public DNS hostname")
    except ValueError:
        pass
    api = "openai-completions" if kind == "openai" else "anthropic-messages"
    agent = fixture.root / "home/.pi/agent"
    agent.joinpath("models.json").write_text(json.dumps({"providers": {f"remote-{kind}": {"baseUrl": url, "api": api, "apiKey": key, "models": [{"id": model, "name": model, "contextWindow": 8192, "maxTokens": 512, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}}]}}}))
    project = fixture.project(f"project-remote-{kind}")
    rpc = fixture.rpc(project, model, provider=f"remote-{kind}", allow_domains=(parsed.hostname,))
    try:
        fixture.ready(rpc)
        rpc.send("remote", "prompt", message="Reply with COMPLETE.")
        if stop_reason(rpc) != "stop":
            raise RuntimeError(f"remote {kind} request did not settle")
    finally:
        fixture.close_session(rpc)


def runtime(engine: str, case: str) -> int:
    print(f"RUN pi local e2e: {engine} {case}", flush=True)
    fixture = Runtime(engine)
    visibility: list[float] = []
    try:
        project = fixture.project("project", "vhrn-tool-marker-7e9f")
        server = fixture.provider("tool", "vhrn-tool-marker-7e9f"); fixture.models(server)
        for harness in (("claude", "codex") if case in ("persistence", "trust") else ("pi", "claude", "codex")):
            fixture.version(harness, project)
        if case == "endpoints":
            endpoint_cases(fixture)
        if case == "signals":
            signal_cases(fixture)
        if case in ("sessions", "all"):
            session_cases(fixture)
        if case in ("persistence", "all"):
            persistence_cases(fixture)
        if case in ("trust", "all"):
            trust_cases(fixture)
        if case == "remote-openai":
            remote_case(fixture, "openai")
        if case == "remote-anthropic":
            remote_case(fixture, "anthropic")
        if case in ("tool", "all"):
            rpc = fixture.rpc(project, "tool", "read"); fixture.ready(rpc)
            fixture.net("allow", "--project", str(project), "--local", f"localhost:{server.server_port}")
            rpc.send("prompt", "prompt", message="Read marker.txt and reply.")
            complete = tool_success = final_ok = False
            for event in rpc.events(time.monotonic() + 75):
                payload = nested(event) if isinstance(event, dict) else {}; message = payload.get("assistantMessageEvent", {})
                if payload.get("type") == "message_update" and message.get("type") == "text_delta":
                    if message.get("delta") == "STREAM-": server.state.release_stream.set()
                    if message.get("delta") == "COMPLETE": complete = True
                if payload.get("type") == "tool_execution_end" and payload.get("isError") is False: tool_success = True
                if payload.get("type") == "message_end" and payload.get("message", {}).get("role") == "assistant": final_ok = payload["message"].get("stopReason") == "stop"
                if payload.get("type") == "agent_settled": break
            if not (final_ok and complete and tool_success and server.state.streamed.is_set() and server.state.release_stream.is_set() and server.state.failure is None and server.state.requests == 2): raise RuntimeError("Pi tool flow did not stream and settle: " + rpc.output.decode(errors="replace"))
            fixture.close_session(rpc)
        if case in ("policy", "all"):
            policy = fixture.provider("policy", "unused"); fixture.models(policy)
            policy.state.routes.update({"policy": "policy", "stall": "stall", "final": "final"})
            a_project, b_project = fixture.project("project-a"), fixture.project("project-b")
            a = fixture.rpc(a_project, "policy"); fixture.ready(a)
            b = fixture.rpc(b_project, "policy"); fixture.ready(b)
            endpoint = f"localhost:{policy.server_port}"
            fixture.net("allow", "--project", str(a_project), "--local", endpoint)
            visibility.append(fixture.wait_loopback_visible(a, endpoint))
            a.send("a-allow", "prompt", message="Reply with COMPLETE.")
            if stop_reason(a) != "stop": raise RuntimeError("project A was not allowed: " + a.output.decode(errors="replace"))
            before = policy.state.requests; b.send("b-denied", "prompt", message="Reply with COMPLETE.")
            if stop_reason(b) != "error" or policy.state.requests != before: raise RuntimeError("project B reached provider without a grant: " + b.output.decode(errors="replace"))
            global_allow = fixture.net("allow", "--local", endpoint)
            active_status = fixture.net("status", "--local")
            visibility.append(fixture.wait_loopback_visible(b, endpoint))
            b.send("b-global", "prompt", message="Reply with COMPLETE.")
            if stop_reason(b) != "stop": raise RuntimeError("global grant did not update active B session\n" + global_allow + active_status + b.output.decode(errors="replace"))
            fixture.net("deny", "--local", endpoint)
            status = fixture.net("status", "--local")
            if endpoint not in status or str(a_project) not in status: raise RuntimeError("global deny erased project A provenance: " + status)
            fixture.close_session(b); b = fixture.rpc(b_project, "policy"); fixture.ready(b)
            before = policy.state.requests; b.send("b-restart-denied", "prompt", message="Reply with COMPLETE.")
            if stop_reason(b) != "error" or policy.state.requests != before: raise RuntimeError("restarted B reached provider after global deny: " + b.output.decode(errors="replace"))
            fixture.net("deny", "--project", str(a_project), "--local", endpoint)
            fixture.close_session(a); a = fixture.rpc(a_project, "policy"); fixture.ready(a)
            before = policy.state.requests; a.send("a-restart-denied", "prompt", message="Reply with COMPLETE.")
            if stop_reason(a) != "error" or policy.state.requests != before: raise RuntimeError("restarted A reached provider after project deny: " + a.output.decode(errors="replace"))
            fixture.close_session(a); fixture.close_session(b)

            # Pi uses streaming HTTP through this same proxy.  Exercise the proxy's
            # ordinary absolute-form HTTP route independently: release the first
            # SSE response only after it is observable, require the second request
            # to reuse the upstream TCP socket, then deny a fresh request.
            plain = fixture.provider("plain", "unused"); plain.state.routes["policy"] = "policy"; fixture.models(plain)
            plain_project = fixture.project("project-plain-http")
            plain_rpc = fixture.rpc(plain_project, "final")
            fixture.ready(plain_rpc)
            plain_endpoint = f"localhost:{plain.server_port}"
            fixture.net("allow", "--project", str(plain_project), "--local", plain_endpoint)
            fixture.wait_loopback_visible(plain_rpc, plain_endpoint)
            plain_url = f"http://localhost:{plain.server_port}/v1/chat/completions"
            request = fixture.plain_http_reuse(plain_rpc, plain_url)
            try:
                assert request.stdout is not None and request.stdin is not None
                first = process_line(request, time.monotonic() + 15)
                if not first.startswith("FIRST ") or "STREAM-" not in first:
                    raise RuntimeError("plain HTTP did not expose its first SSE chunk before release: " + first)
                plain.state.release_stream.set(); request.stdin.write(b"release\n"); request.stdin.flush(); request.stdin.close()
                second = process_line(request, time.monotonic() + 15)
                if second != "SECOND" or request.wait(timeout=15):
                    raise RuntimeError(f"plain HTTP did not finish its same-agent second request ({second}; posts={plain.state.requests})")
            finally:
                if request.poll() is None:
                    request.terminate()
                    try:
                        request.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        request.kill(); request.wait()
            if plain.state.requests != 2 or plain.state.connection_id is None:
                raise RuntimeError("plain HTTP did not issue two requests on one upstream TCP connection")
            fixture.net("deny", "--project", str(plain_project), "--local", plain_endpoint)
            deadline, revoked = time.monotonic() + 10, False
            while time.monotonic() < deadline:
                before = plain.state.requests
                if fixture.plain_http_status(plain_rpc, plain_url) == "403":
                    if plain.state.requests != before:
                        raise RuntimeError("the final revoked plain HTTP request reached the origin")
                    revoked = True
                    break
                time.sleep(.1)
            if not revoked:
                raise RuntimeError("plain HTTP revocation was not visible within 10 seconds")
            fixture.close_session(plain_rpc)
        if case in ("cancel", "all"):
            cancel_server = fixture.provider("policy", "unused"); fixture.models(cancel_server)
            cancel_server.state.routes.update({"stall": "stall", "final": "final"})
            cancel_project = fixture.project("project-cancel")
            cancel = fixture.rpc(cancel_project, "stall"); fixture.ready(cancel)
            cancel_endpoint = f"localhost:{cancel_server.server_port}"
            fixture.net("allow", "--project", str(cancel_project), "--local", cancel_endpoint)
            visibility.append(fixture.wait_loopback_visible(cancel, cancel_endpoint))
            cancel.send("stall", "prompt", message="Stream until aborted.")
            if not any(nested(event).get("type") == "message_update" and nested(event).get("assistantMessageEvent", {}).get("delta") == "STREAM-" for event in cancel.events(time.monotonic() + 30)):
                raise RuntimeError("stall did not stream: " + cancel.output.decode(errors="replace"))
            abort_and_settle(cancel)
            state_not_streaming(cancel)
            Runtime.command(cancel, "model", "set_model", provider="vhrn-test", modelId="final")
            cancel.send("final", "prompt", message="Reply with COMPLETE.")
            if stop_reason(cancel) != "stop": raise RuntimeError("final prompt after abort failed: " + cancel.output.decode(errors="replace"))
            if not cancel_server.state.held_post_eof.wait(5): raise RuntimeError("provider did not observe abort")
            fixture.close_session(cancel)
    finally:
        fixture.close()
    latency = "" if not visibility else " visibility=" + ",".join(f"{seconds:.2f}s" for seconds in visibility)
    print(f"PASS pi local e2e: {engine} {case}{latency}", flush=True)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(); parser.add_argument("--self-test", action="store_true"); parser.add_argument("--runtime", choices=("container", "docker")); parser.add_argument("--case", choices=("smoke", "tool", "policy", "cancel", "endpoints", "signals", "sessions", "persistence", "trust", "remote-openai", "remote-anthropic", "all"), default="all"); options = parser.parse_args()
    if options.self_test: return self_test()
    if options.case.startswith("remote-"):
        prefix = "VHRN_PI_REMOTE_" + options.case.removeprefix("remote-").upper() + "_"
        if not all(os.environ.get(prefix + name) for name in ("URL", "MODEL", "KEY")):
            print(f"UNRUN {options.case}: set {prefix}URL, {prefix}MODEL, and {prefix}KEY", flush=True)
            return 77
    if options.runtime:
        if options.case.startswith("remote-"):
            try:
                return runtime(options.runtime, options.case)
            except Exception:
                # Endpoint credentials may appear in Pi's own diagnostics.  Keep
                # this optional real-network path category-only for CI logs.
                print(f"FAIL {options.case}: endpoint or Pi protocol check failed", flush=True)
                return 1
        return runtime(options.runtime, options.case)
    parser.error("select --self-test or --runtime")


if __name__ == "__main__": raise SystemExit(main())
