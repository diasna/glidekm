"""Decky Loader backend for glidekm: manages the glidekm-client process.

Runs as root (plugin flag "root"), so /dev/uinput access needs no udev rule.
"""

import asyncio
import json
import os
import socket

import decky  # provided by Decky Loader

BIN = os.path.join(decky.DECKY_PLUGIN_DIR, "bin", "glidekm-client")
SETTINGS = os.path.join(decky.DECKY_SETTINGS_DIR, "settings.json")
CONFIG = os.path.join(decky.DECKY_SETTINGS_DIR, "client.conf")
STATUS = os.path.join(decky.DECKY_RUNTIME_DIR, "status.json")
LOG = os.path.join(decky.DECKY_PLUGIN_LOG_DIR, "client.log")

DEFAULTS = {"enabled": False, "passphrase": "", "port": 4242, "width": 0, "height": 0}


def _load() -> dict:
    try:
        with open(SETTINGS) as f:
            return {**DEFAULTS, **json.load(f)}
    except Exception:
        return dict(DEFAULTS)


def _save(s: dict) -> None:
    os.makedirs(os.path.dirname(SETTINGS), exist_ok=True)
    with open(SETTINGS, "w") as f:
        json.dump(s, f)
    os.chmod(SETTINGS, 0o600)
    lines = [f"passphrase = {s['passphrase']}", f"port = {int(s['port'])}"]
    if s.get("width") and s.get("height"):
        lines += [f"width = {int(s['width'])}", f"height = {int(s['height'])}"]
    with open(CONFIG, "w") as f:
        f.write("\n".join(lines) + "\n")
    os.chmod(CONFIG, 0o600)


def _local_ip() -> str:
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.connect(("192.0.2.1", 9))  # no packet is sent for UDP connect
            return s.getsockname()[0]
    except Exception:
        return ""


class Plugin:
    proc = None
    watcher = None
    error = ""

    async def _start(self):
        if self.proc and self.proc.returncode is None:
            return
        os.chmod(BIN, 0o755)
        try:
            os.remove(STATUS)
        except FileNotFoundError:
            pass
        logf = open(LOG, "ab")
        self.proc = await asyncio.create_subprocess_exec(
            BIN, "--config", CONFIG, "--status-file", STATUS,
            stdout=logf, stderr=logf, stdin=asyncio.subprocess.DEVNULL,
        )
        self.error = ""
        decky.logger.info(f"glidekm-client started (pid {self.proc.pid})")

    async def _stop(self):
        p, self.proc = self.proc, None
        if p and p.returncode is None:
            p.terminate()
            try:
                await asyncio.wait_for(p.wait(), 3)
            except asyncio.TimeoutError:
                p.kill()
        decky.logger.info("glidekm-client stopped")

    async def _watch(self):
        """Restart the client if it crashes while enabled."""
        while True:
            await asyncio.sleep(2)
            s = _load()
            if not s["enabled"]:
                continue
            if self.proc is None or self.proc.returncode is not None:
                if self.proc is not None:
                    self.error = f"client exited ({self.proc.returncode}), restarting"
                    decky.logger.warning(self.error)
                try:
                    await self._start()
                except Exception as e:  # noqa: BLE001
                    self.error = str(e)

    async def get_state(self) -> dict:
        s = _load()
        st = {}
        try:
            with open(STATUS) as f:
                st = json.load(f)
        except Exception:
            pass
        running = self.proc is not None and self.proc.returncode is None
        return {
            "enabled": s["enabled"],
            "running": running,
            "passphrase_set": bool(s["passphrase"]),
            "port": int(s["port"]),
            "width": int(s.get("width") or 0),
            "height": int(s.get("height") or 0),
            "connected": bool(st.get("connected")) if running else False,
            "server": st.get("server", ""),
            "active": bool(st.get("active")) if running else False,
            "capture": bool(st.get("capture")) if running else False,
            "screen": f"{st['width']}x{st['height']}" if running and "width" in st else "",
            "ip": _local_ip(),
            "error": self.error,
        }

    async def set_enabled(self, enabled: bool) -> dict:
        s = _load()
        s["enabled"] = bool(enabled)
        _save(s)
        if enabled:
            await self._start()
        else:
            await self._stop()
        return await self.get_state()

    async def save_settings(self, passphrase: str, port: int, width: int, height: int) -> dict:
        s = _load()
        if passphrase:
            s["passphrase"] = passphrase
        s["port"] = int(port) if 0 < int(port) < 65536 else 4242
        s["width"], s["height"] = (int(width), int(height)) if width and height else (0, 0)
        _save(s)
        if s["enabled"]:
            await self._stop()
            await self._start()
        return await self.get_state()

    async def _main(self):
        s = _load()
        _save(s)  # regenerate client.conf
        if s["enabled"]:
            try:
                await self._start()
            except Exception as e:  # noqa: BLE001
                self.error = str(e)
        self.watcher = asyncio.get_event_loop().create_task(self._watch())

    async def _unload(self):
        if self.watcher:
            self.watcher.cancel()
        await self._stop()

    async def _uninstall(self):
        await self._stop()
