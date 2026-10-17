#!/usr/bin/env python3
"""Run after make build. Uses fake IPC sockets and never launches applications."""

import asyncio
from contextlib import asynccontextmanager
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import tomllib


def client(address, app_id):
    return {
        "address": address,
        "class": app_id,
        "pid": -1,
        "workspace": {"id": 1, "name": "1"},
        "monitor": 0,
        "at": [100, 200],
        "size": [800, 600],
        "floating": False,
        "fullscreen": 0,
    }


def six_clients():
    return [client(f"0x{i + 1:x}", f"app{i}") for i in range(6)]


class FakeHypr:
    def __init__(self, root, clients):
        self.clients = clients
        self.writers = []
        self.logs = []
        self.condition = asyncio.Condition()
        self.ipc_failure = None
        self.snapshot = root / "state" / "sessions" / "last.toml"

    def seed(self, clients):
        self.snapshot.parent.mkdir(parents=True, exist_ok=True)
        content = '[session]\nname = "last"\ntimestamp = 1\n'
        for window in clients:
            app_id = window["class"]
            content += (
                f'\n[[window]]\napp_id = "{app_id}"\nlaunch_cmd = "{app_id}"\n'
                'workspace = "1"\nmonitor = "TEST"\nfloating = false\nfullscreen = false\n'
            )
        self.snapshot.write_text(content)

    async def request(self, reader, writer):
        command = (await reader.read()).decode()
        if command == "j/clients":
            response = json.dumps(self.clients) if self.ipc_failure != "clients" else "invalid"
        elif command == "j/monitors":
            response = '[{"id":0,"name":"TEST"}]' if self.ipc_failure != "monitors" else "invalid"
        else:
            response = "ok"
            if command.startswith("dispatch exec "):
                app_id = command.split()[-1].strip("'\"")
                window = client(f"0x{len(self.clients) + 1:x}", app_id)
                self.clients.append(window)
                await self.broadcast(f'openwindow>>{window["address"]},1,{app_id},Restored')
        writer.write(response.encode())
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    async def subscribe(self, reader, writer):
        self.writers.append(writer)
        try:
            await reader.read()
        finally:
            self.writers.remove(writer)
            writer.close()
            await writer.wait_closed()

    async def broadcast(self, event):
        for writer in self.writers[:]:
            writer.write(f"{event}\n".encode())
            await writer.drain()

    async def read_logs(self):
        while line := await self.daemon.stderr.readline():
            async with self.condition:
                self.logs.append(line.decode())
                self.condition.notify_all()

    async def logged(self, marker, after=0):
        async with self.condition:
            while True:
                for index in range(after, len(self.logs)):
                    if marker in self.logs[index]:
                        return index + 1
                await self.condition.wait()

    async def emit(self, event):
        start = len(self.logs)
        await self.broadcast(f"{event}\nevent-save-barrier")
        start = await self.logged("unhandled event: event-save-barrier", start)
        await self.logged("unhandled event: event-save-barrier", start)

    async def save(self):
        start = len(self.logs)
        self.daemon.send_signal(signal.SIGUSR1)
        await self.logged("saved session", start)

    async def stop(self):
        self.daemon.send_signal(signal.SIGTERM)
        assert await self.daemon.wait() == 0, "daemon failed on shutdown"

    def windows(self):
        return {w["app_id"]: w for w in tomllib.loads(self.snapshot.read_text())["window"]}

    async def close_to_helper(self):
        for window in self.clients[1:]:
            self.clients.remove(window)
            await self.emit(f'closewindow>>{window["address"]}')


@asynccontextmanager
async def harness(binary, root, clients, *, seed=None, period=60, restore=False,
                  per_window=True, commands=None):
    root.mkdir()
    h = FakeHypr(root, clients)
    if seed is not None:
        h.seed(seed)
    sockets = root / "hypr" / "test"
    sockets.mkdir(parents=True)
    ipc = await asyncio.start_unix_server(h.request, sockets / ".socket.sock")
    events = await asyncio.start_unix_server(h.subscribe, sockets / ".socket2.sock")
    config = root / "config.toml"
    overrides = {w["class"]: w["class"] for w in clients + (seed or [])}
    overrides.update(base="base", telegram="telegram-desktop", x="brave --app=https://x.com/")
    overrides.update(commands or {})
    config.write_text(
        f'[general]\nsession_dir = "{root}/state"\n'
        f'restore_on_start = {str(restore).lower()}\nnotify = false\nsave_interval = {period}\n'
        f'per_window_launch = {str(per_window).lower()}\n'
        'settle_delay = 2\nrestore_layout = false\nrestore_geometry = false\n'
        '[overrides]\n' + "".join(f'{key} = "{value}"\n' for key, value in overrides.items())
    )
    h.daemon = await asyncio.create_subprocess_exec(
        str(binary), "-vvv", "--config", str(config),
        env={**os.environ, "XDG_RUNTIME_DIR": str(root), "HYPRLAND_INSTANCE_SIGNATURE": "test",
             "RUST_LOG": "hypr_persist=trace"},
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE,
    )
    logger = asyncio.create_task(h.read_logs())
    try:
        await asyncio.wait_for(h.logged("daemon running"), timeout=5)
        yield h
    except BaseException:
        print("".join(h.logs), file=sys.stderr)
        raise
    finally:
        if h.daemon.returncode is None:
            h.daemon.kill()
            await h.daemon.wait()
        await logger
        ipc.close()
        events.close()
        await ipc.wait_closed()
        await events.wait_closed()


async def normal_events(binary, root):
    async with harness(binary, root, [client("0xaaa", "base")]) as h:
        await h.save()
        assert set(h.windows()) == {"base"}
        for address, app_id in [("0xbbb", "telegram"), ("0xccc", "x")]:
            h.clients.append(client(address, app_id))
            await h.emit(f"openwindow>>{address},1,{app_id},Test")
            assert app_id in h.windows(), f"{app_id} was not saved before the periodic tick"
        assert h.windows()["telegram"]["launch_cmd"] == "telegram-desktop"
        assert h.windows()["x"]["launch_cmd"] == "brave --app=https://x.com/"
        assert h.windows()["x"]["monitor"] == "TEST"

        h.clients[-1]["workspace"] = {"id": 2, "name": "2"}
        await h.emit("movewindow>>0xccc,2")
        assert h.windows()["x"]["workspace"] == "2"
        h.clients[-1]["floating"] = True
        await h.emit("changefloatingmode>>0xccc,1")
        assert h.windows()["x"]["floating"]
        saved = h.snapshot.stat().st_mtime_ns
        await h.emit("movewindow>>0xccc,2")
        await h.emit("changefloatingmode>>0xccc,1")
        await h.emit("openwindow>>0xddd,1,xdg-desktop-portal-gtk,Excluded")
        assert h.snapshot.stat().st_mtime_ns == saved, "unchanged or excluded event saved"

        for window in h.clients[:]:
            h.clients.remove(window)
            await h.emit(f'closewindow>>{window["address"]}')
        assert h.snapshot.stat().st_mtime_ns == saved, "close bypassed settling"
        await h.stop()
        assert set(h.windows()) == {"base", "telegram", "x"}, "empty shutdown lost apps"


async def reduced_events(binary, root):
    async with harness(binary, root, six_clients()) as h:
        await h.save()
        original = h.snapshot.read_bytes()
        await h.close_to_helper()
        h.clients[0]["workspace"] = {"id": 2, "name": "2"}
        await h.emit("movewindow>>0x1,2")
        assert h.snapshot.read_bytes() == original, "move after close saved a partial capture"
        h.clients[0]["floating"] = True
        start = len(h.logs)
        await h.emit("changefloatingmode>>0x1,1")
        assert h.snapshot.read_bytes() == original, "float after close bypassed the deadline"
        await asyncio.wait_for(h.logged("saved session", start), timeout=4)
        assert len(h.windows()) == 1, "settled shrink did not save at the scheduler deadline"
        assert h.windows()["app0"]["floating"]
        await h.stop()
        assert len(h.windows()) == 1


async def startup_closes(binary, root):
    async with harness(binary, root, six_clients(), seed=six_clients()) as h:
        original = h.snapshot.read_bytes()
        await h.close_to_helper()
        await h.stop()
        assert h.snapshot.read_bytes() == original, "shutdown before first save lost initial members"


async def startup_partial(binary, root):
    async with harness(binary, root, six_clients()[:1], seed=six_clients()) as h:
        original = h.snapshot.read_bytes()
        await h.stop()
        assert h.snapshot.read_bytes() == original, "startup helper replaced the disk capture"


async def startup_periodic(binary, root):
    async with harness(binary, root, six_clients()[:1], seed=six_clients(), period=1) as h:
        original = h.snapshot.read_bytes()
        await h.logged("deferring reduced session capture")
        assert h.snapshot.read_bytes() == original, "first periodic tick lost the startup capture"
        await asyncio.wait_for(h.logged("saved session"), timeout=3)
        assert len(h.windows()) == 1, "startup protection prevented a settled shrink"
        h.clients.append(client("0x2", "app1"))
        await h.emit("openwindow>>0x2,1,app1,Test")
        assert len(h.windows()) == 2, "new window did not save immediately"
        start = len(h.logs)
        await h.close_to_helper()
        await asyncio.wait_for(h.logged("saved session", start), timeout=4)
        assert len(h.windows()) == 1, "later periodic shrink remained blocked by the startup count"
        await h.stop()
        assert len(h.windows()) == 1, "startup count remained a permanent floor"


async def startup_explicit(binary, root):
    async with harness(binary, root, six_clients()[:1], seed=six_clients()) as h:
        original = h.snapshot.read_bytes()
        h.clients[0]["workspace"] = {"id": 2, "name": "2"}
        await h.emit("movewindow>>0x1,2")
        assert h.snapshot.read_bytes() == original
        await h.save()
        await h.stop()
        assert len(h.windows()) == 1, "explicit save did not accept the smaller startup capture"


async def ipc_errors(binary, root):
    async with harness(binary, root, six_clients()) as h:
        await h.save()
        original = h.snapshot.read_bytes()
        for kind, workspace in [("clients", "2"), ("monitors", "3")]:
            h.ipc_failure = kind
            h.clients[0]["workspace"] = {"id": int(workspace), "name": workspace}
            await h.emit(f"movewindow>>0x1,{workspace}")
            assert h.snapshot.read_bytes() == original, f"{kind} IPC failure wrote stale state"
        h.ipc_failure = None
        await h.stop()


async def restored_burst(binary, root):
    async with harness(binary, root, [], seed=six_clients(), restore=True) as h:
        await h.emit("restore-save-barrier")
        await h.save()
        assert len(h.windows()) == 6
        prior = h.snapshot.with_name(".last.toml.prev")
        assert len(tomllib.loads(prior.read_text())["window"]) == 6, "queued restore opens saved partial state"
        assert all("(6 apps)" in line for line in h.logs if "saved session" in line)
        await h.stop()


async def pending_open(binary, root, failure):
    async with harness(binary, root, [], period=1) as h:
        h.ipc_failure = failure
        await h.emit("openwindow>>0xabc,1,telegram,New")
        assert not h.snapshot.exists(), "unqueried open persisted placeholder geometry"
        h.ipc_failure = None
        window = client("0xabc", "")
        window["workspace"] = {"id": 2, "name": "2"}
        h.clients.append(window)
        await asyncio.wait_for(h.logged("saved session"), timeout=3)
        saved = h.windows()["telegram"]
        assert saved["launch_cmd"] == "telegram-desktop", "pending open lost resolver metadata"
        assert saved["size"] == [800, 600] and saved["position"] == [100, 200]
        assert saved["workspace"] == "2" and saved["monitor"] == "TEST"
        await h.stop()


async def pending_open_absent(binary, root):
    await pending_open(binary, root, None)


async def pending_open_query_error(binary, root):
    await pending_open(binary, root, "clients")


async def pending_open_geometry(binary, root):
    async with harness(binary, root, [], period=1) as h:
        window = client("0xabc", "telegram")
        window["size"] = [0, 0]
        h.clients.append(window)
        await h.emit("openwindow>>0xabc,1,telegram,New")
        await asyncio.wait_for(h.logged("refreshed geometry for 0/0 windows"), timeout=2)
        assert not h.snapshot.exists(), "pending open saved zero geometry"
        window["size"] = [800, 600]
        await asyncio.wait_for(h.logged("saved session"), timeout=2)
        assert h.windows()["telegram"]["size"] == [800, 600]
        await h.stop()


async def pending_open_closed(binary, root):
    async with harness(binary, root, []) as h:
        await h.emit("openwindow>>0xabc,1,telegram,New")
        await h.emit("closewindow>>0xAbC")
        h.clients.append(client("0xabc", "telegram"))
        start = len(h.logs)
        h.daemon.send_signal(signal.SIGUSR1)
        await h.logged("received SIGUSR1", start)
        await h.emit("closed-open-barrier")
        assert not h.snapshot.exists(), "closed pending open was resurrected"
        await h.stop()


async def pending_open_refresh_errors(binary, root):
    async with harness(binary, root, []) as h:
        await h.emit("openwindow>>0xabc,1,telegram,New")
        h.clients.append(client("0xabc", "telegram"))
        for failure in ["clients", "monitors"]:
            h.ipc_failure = failure
            start = len(h.logs)
            h.daemon.send_signal(signal.SIGUSR1)
            await h.logged("received SIGUSR1", start)
            await h.emit("pending-error-barrier")
            assert not h.snapshot.exists(), f"{failure} failure consumed or saved pending open"
        h.ipc_failure = None
        await h.save()
        assert h.windows()["telegram"]["launch_cmd"] == "telegram-desktop"
        await h.stop()


async def startup_filtered_helper(binary, root):
    clients = [client("0x1", "app0"), client("0x2", "helper")]
    async with harness(binary, root, clients, seed=six_clients()[:2],
                       commands={"helper": ""}) as h:
        original = h.snapshot.read_bytes()
        await h.stop()
        assert h.snapshot.read_bytes() == original, "unresolved helper masked a missing saved app"


async def startup_dedup(binary, root):
    clients = [client("0x1", "app0"), client("0x2", "app0")]
    async with harness(binary, root, clients, seed=six_clients()[:2], per_window=False) as h:
        original = h.snapshot.read_bytes()
        await h.stop()
        assert h.snapshot.read_bytes() == original, "duplicate windows masked a missing saved app"


async def startup_filtered_periodic(binary, root):
    clients = [client("0x1", "app0"), client("0x2", "helper")]
    async with harness(binary, root, clients, seed=six_clients()[:2], period=1,
                       commands={"helper": ""}) as h:
        original = h.snapshot.read_bytes()
        await asyncio.wait_for(h.logged("deferring reduced session capture"), timeout=2)
        assert h.snapshot.read_bytes() == original, "first tick saved a filtered startup subset"
        await asyncio.wait_for(h.logged("saved session"), timeout=3)
        assert set(h.windows()) == {"app0"}, "filtered count permanently blocked intentional shrink"
        await h.stop()


async def written_baseline(binary, root, per_window):
    extra = "helper" if per_window else "app0"
    clients = [client("0x1", "app0"), client("0x2", extra)]
    async with harness(binary, root, clients, per_window=per_window,
                       commands={"helper": ""}) as h:
        await h.save()
        assert set(h.windows()) == {"app0"}
        h.clients.pop()
        await h.emit("closewindow>>0x2")
        h.clients[0]["workspace"] = {"id": 2, "name": "2"}
        await h.stop()
        assert h.windows()["app0"]["workspace"] == "2", "unsaved window polluted shutdown baseline"


async def written_filtered_baseline(binary, root):
    await written_baseline(binary, root, True)


async def written_dedup_baseline(binary, root):
    await written_baseline(binary, root, False)


async def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/hypr-persist").resolve()
    cases = [normal_events, reduced_events, startup_closes, startup_partial,
             startup_periodic, startup_explicit, ipc_errors, restored_burst,
             pending_open_absent, pending_open_query_error, pending_open_geometry,
             pending_open_closed, pending_open_refresh_errors, startup_filtered_helper,
             startup_dedup, startup_filtered_periodic, written_filtered_baseline,
             written_dedup_baseline]
    selected = set(sys.argv[2:])
    with tempfile.TemporaryDirectory(prefix="event-save-") as directory:
        for case in cases:
            if not selected or case.__name__ in selected:
                await asyncio.wait_for(case(binary, Path(directory) / case.__name__), timeout=15)
                print(f"PASS: {case.__name__}")


if __name__ == "__main__":
    asyncio.run(main())
