"""vx-overlay: a status bar pinned to the bottom of the game window, so a long load never looks like a hang.

Reads the emulator's log (the launcher's stderr file) and the window title, and shows:
  line 1: fps, ms per frame, draws, shaders still being built, shader cache hits/misses, textures waiting,
          data files opened, CPU cores busy, free commit memory, time since launch;
  line 2: what is happening now. Green while frames flow; yellow "WORKING: ..." when no new frame came for a
          while (building shaders, loading files, uploading textures, or the game's own loading); red when the
          game has exited.
The bar is click-through and never takes focus (keyboard input stays with the game), and it hides while
another window is in front. Standard library only (tkinter + ctypes). Offline; reads files, writes nothing.

usage: pythonw vx-overlay.py --pid <game pid> --log <stderr file>
"""
import argparse, collections, ctypes, ctypes.wintypes as wt, os, re, time, tkinter as tk

ap = argparse.ArgumentParser()
ap.add_argument("--pid", type=int, default=0)
ap.add_argument("--log", default="")
ap.add_argument("--replay", help="read this log once, print what the bar would show from it, and exit (a test)")
ap.add_argument("--stall", type=float, default=2.5, help="seconds without a new frame before WORKING shows")
ap.add_argument("--always", action="store_true", help="show the bar even when another window is in front")
args = ap.parse_args()

try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)  # physical pixels, as GetClientRect gives them
except Exception:
    pass
user32, kernel32, psapi = ctypes.windll.user32, ctypes.windll.kernel32, ctypes.windll.psapi

# ---- the game process and its window -------------------------------------------------------------------
PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE = 0x1000, 0x00100000
hproc = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, False, args.pid)
EnumWindowsProc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


def game_window():
    best = [None, 0]

    def cb(hwnd, _):
        pid = wt.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        if pid.value == args.pid and user32.IsWindowVisible(hwnd):
            r = wt.RECT()
            user32.GetClientRect(hwnd, ctypes.byref(r))
            area = (r.right - r.left) * (r.bottom - r.top)
            if area > best[1]:
                best[:] = [hwnd, area]
        return True

    user32.EnumWindows(EnumWindowsProc(cb), 0)
    return best[0]


def title_of(hwnd):
    n = user32.GetWindowTextLengthW(hwnd)
    buf = ctypes.create_unicode_buffer(n + 1)
    user32.GetWindowTextW(hwnd, buf, n + 1)
    return buf.value


def client_rect_on_screen(hwnd):
    r = wt.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(r))
    p = wt.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(p))
    return p.x, p.y, r.right - r.left, r.bottom - r.top


def exit_code():
    code = wt.DWORD()
    if not hproc or not kernel32.GetExitCodeProcess(hproc, ctypes.byref(code)):
        return None
    return None if code.value == 259 else code.value  # 259: STILL_ACTIVE


class FILETIME(ctypes.Structure):
    _fields_ = [("lo", wt.DWORD), ("hi", wt.DWORD)]


def cpu_seconds():
    c, e, k, u = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    if not kernel32.GetProcessTimes(hproc, ctypes.byref(c), ctypes.byref(e), ctypes.byref(k), ctypes.byref(u)):
        return None
    return ((k.hi << 32 | k.lo) + (u.hi << 32 | u.lo)) / 1e7


class MEMSTAT(ctypes.Structure):
    _fields_ = [("len", wt.DWORD), ("load", wt.DWORD), ("totalPhys", ctypes.c_ulonglong), ("availPhys", ctypes.c_ulonglong),
                ("totalPage", ctypes.c_ulonglong), ("availPage", ctypes.c_ulonglong), ("totalVirt", ctypes.c_ulonglong),
                ("availVirt", ctypes.c_ulonglong), ("availExt", ctypes.c_ulonglong)]


def free_commit_gb():
    m = MEMSTAT()
    m.len = ctypes.sizeof(MEMSTAT)
    kernel32.GlobalMemoryStatusEx(ctypes.byref(m))
    return m.availPage / 2**30


# ---- the log ---------------------------------------------------------------------------------------------
ANSI = re.compile(r"\x1b\[[0-9;]*m")
FPS = re.compile(r"([\d.]+) fps, ([\d.]+) ms/frame.*?(\d+) draws/frame, dropped (\d+) without a host pipeline \((\d+) being made\)")
TEXQ = re.compile(r"textures picked by index: (\d+) uploaded so far \((\d+) MB\), (\d+) waiting")
CACHE = re.compile(r"shaders from the cache and translated so far hits=(\d+) misses=(\d+)")
OPENED = re.compile(r'file opened path="([^"]+)"')
BIG = re.compile(r"big host pipeline made .*?ms=(\d+)")
TITLE = re.compile(r"([\d.]+) fps ([\d.]+) ms/frame (\d+) draws/frame")


class Log:
    def __init__(self, path):
        self.path, self.pos, self.rest = path, 0, ""
        self.fps = self.ms = self.draws = self.making = None
        self.tex_wait = self.tex_up = None
        self.hits = self.misses = None
        self.files = 0
        self.last_file = ""
        self.events = collections.deque()  # (time, kind)
        self.last_warn = ("", 0.0)

    def poll(self):
        try:
            with open(self.path, "r", encoding="utf-8", errors="replace") as f:
                f.seek(self.pos)
                data = f.read(4 << 20)
                self.pos = f.tell()
        except OSError:
            return
        now = time.time()
        lines = (self.rest + data).split("\n")
        self.rest = lines.pop()
        for line in lines:
            line = ANSI.sub("", line)
            if " fps, " in line:
                m = FPS.search(line)
                if m:
                    self.fps, self.ms, self.draws, _, self.making = float(m[1]), float(m[2]), int(m[3]), int(m[4]), int(m[5])
                t = TEXQ.search(line)
                if t:
                    self.tex_up, self.tex_wait = int(t[1]), int(t[3])
            elif "shader translated" in line:
                self.events.append((now, "translated"))
            elif "host pipeline" in line or "host mesh pipeline" in line:
                self.events.append((now, "pipeline"))
            elif "file opened" in line:
                m = OPENED.search(line)
                if m:
                    self.files += 1
                    self.last_file = m[1].replace("\\\\", "\\")
                    self.events.append((now, "file"))
            elif "so far hits=" in line:
                m = CACHE.search(line)
                if m:
                    self.hits, self.misses = int(m[1]), int(m[2])
            elif " ERROR " in line:
                self.last_warn = ("ERROR " + line.split(" ERROR ", 1)[1][:140], now)
        while self.events and now - self.events[0][0] > 5:
            self.events.popleft()

    def recent(self, kind):
        return sum(1 for _, k in self.events if k == kind)


log = Log(args.log)

if args.replay:
    r = Log(args.replay)
    while True:
        before = r.pos
        r.poll()
        if r.pos == before:
            break
    print("fps %s  ms %s  draws %s  shaders building %s  cache %s hit / %s new  textures waiting %s (uploaded %s)  files %d (last %s)  last error: %s" % (
        r.fps, r.ms, r.draws, r.making, r.hits, r.misses, r.tex_wait, r.tex_up, r.files, os.path.basename(r.last_file), r.last_warn[0][:80] or "none"))
    raise SystemExit(0)

# ---- the bar ---------------------------------------------------------------------------------------------
root = tk.Tk()
root.overrideredirect(True)
root.attributes("-topmost", True)
root.attributes("-alpha", 0.82)
root.configure(bg="black")
font = ("Consolas", 10)
l1 = tk.Label(root, text="vx-overlay: waiting for the game window", fg="#d0d0d0", bg="black", font=font, anchor="w")
l2 = tk.Label(root, text="", fg="#7CFC00", bg="black", font=font, anchor="w")
l1.pack(fill="x", padx=6)
l2.pack(fill="x", padx=6)
root.update_idletasks()

GWL_EXSTYLE = -20
WS_EX_LAYERED, WS_EX_TRANSPARENT, WS_EX_TOOLWINDOW, WS_EX_NOACTIVATE, WS_EX_TOPMOST = 0x80000, 0x20, 0x80, 0x8000000, 0x8
user32.GetWindowLongPtrW.restype = ctypes.c_ssize_t
user32.SetWindowLongPtrW.argtypes = [wt.HWND, ctypes.c_int, ctypes.c_ssize_t]
me = user32.GetParent(root.winfo_id()) or root.winfo_id()
style = user32.GetWindowLongPtrW(me, GWL_EXSTYLE)
user32.SetWindowLongPtrW(me, GWL_EXSTYLE, style | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST)

def process_start():
    c, e, k, u = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    if hproc and kernel32.GetProcessTimes(hproc, ctypes.byref(c), ctypes.byref(e), ctypes.byref(k), ctypes.byref(u)):
        return (c.hi << 32 | c.lo) / 1e7 - 11644473600.0  # FILETIME (1601) to the Unix epoch
    return time.time()


started = process_start()
state = {"title": None, "title_at": time.time(), "cpu": (cpu_seconds(), time.time()), "cores": 0.0, "hwnd": None, "gone_at": None, "shown": True}


def show(on):
    if on != state["shown"]:
        (root.deiconify if on else root.withdraw)()
        state["shown"] = on


def tick():
    try:
        log.poll()
        now = time.time()
        code = exit_code()
        if code is not None:
            state["gone_at"] = state["gone_at"] or now
            l1.config(text="game exited (code %d) after %s" % (code, time.strftime("%M:%S", time.gmtime(now - started))))
            l2.config(text=log.last_warn[0] or "log: " + args.log, fg="#ff5050")
            if now - state["gone_at"] > 8:
                root.destroy()
                return
            root.after(500, tick)
            return
        hwnd = state["hwnd"] if state["hwnd"] and user32.IsWindow(state["hwnd"]) else game_window()
        state["hwnd"] = hwnd
        if not hwnd:
            root.after(500, tick)
            return
        fg = user32.GetForegroundWindow()
        show((args.always or fg == hwnd) and not user32.IsIconic(hwnd))
        x, y, w, h = client_rect_on_screen(hwnd)
        bar_h = l1.winfo_reqheight() + l2.winfo_reqheight() + 4
        root.geometry("%dx%d+%d+%d" % (max(w, 200), bar_h, x, y + h - bar_h))
        # liveness: the window title changes with every frame statistic the emulator posts
        title = title_of(hwnd)
        if title != state["title"]:
            state["title"], state["title_at"] = title, now
        stalled = now - state["title_at"]
        m = TITLE.search(title)
        fps, ms, draws = (float(m[1]), float(m[2]), int(m[3])) if m else (log.fps, log.ms, log.draws)
        c0, t0 = state["cpu"]
        c1 = cpu_seconds()
        if c1 is not None and c0 is not None and now - t0 >= 1.0:
            state["cores"] = (c1 - c0) / (now - t0)
            state["cpu"] = (c1, now)
        parts = ["VX %s" % time.strftime("%M:%S", time.gmtime(now - started))]
        if fps is not None:
            parts.append("%.1f fps %4.0f ms %5d draws" % (fps, ms, draws))
        parts.append("shaders building %s" % (log.making if log.making is not None else "?"))
        if log.hits is not None:
            parts.append("cache %d hit / %d new" % (log.hits, log.misses))
        if log.tex_wait is not None:
            parts.append("textures waiting %d" % log.tex_wait)
        parts.append("files %d" % log.files)
        parts.append("CPU %.1f cores" % state["cores"])
        parts.append("free mem %.1f GB" % free_commit_gb())
        l1.config(text="  |  ".join(parts))
        # what is happening now
        tr, pl, fi = log.recent("translated"), log.recent("pipeline"), log.recent("file")
        doing = []
        if log.making:
            doing.append("building shaders: %d left (%d translated, %d built in 5 s)" % (log.making, tr, pl))
        elif tr or pl:
            doing.append("building shaders: %d translated, %d built in 5 s" % (tr, pl))
        if fi:
            doing.append("loading files: %d in 5 s (%s)" % (fi, os.path.basename(log.last_file)))
        if log.tex_wait and log.tex_wait > 1000:
            doing.append("uploading textures: %d waiting" % log.tex_wait)
        if log.last_warn[0] and now - log.last_warn[1] < 10:
            l2.config(text=log.last_warn[0], fg="#ff5050")
        elif stalled > args.stall:
            what = "; ".join(doing) or ("the game is loading (CPU %.1f cores busy)" % state["cores"])
            l2.config(text="WORKING, not frozen: no new frame for %.0f s - %s" % (stalled, what), fg="#ffd700")
        else:
            l2.config(text=("; ".join(doing)) or "running", fg="#7CFC00")
    except Exception as e:  # never let the bar take the game down with it
        l2.config(text="overlay error: %s" % e, fg="#ff5050")
    root.after(500, tick)


root.after(200, tick)
root.mainloop()
