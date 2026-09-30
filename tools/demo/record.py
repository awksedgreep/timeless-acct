#!/usr/bin/env python3
"""Drive the viewer in a private tmux server and keep what it draws:
<frames>/NNNN.ansi and <frames>/manifest.tsv (file, milliseconds, caption).

The story: now; go to a moment; step across it; what ended then; look for
something among it; its record; the jobs; the timeline zoomed out; now."""
import argparse, os, subprocess, time

parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument("--bin", required=True, help="the timeless-acct binary")
parser.add_argument("--data", required=True, help="the store's directory")
parser.add_argument("--at", required=True, help='the moment to go to, as the viewer takes it: "2026-09-29 19:48:20"')
parser.add_argument("--look-for", required=True, help="what to look for among the exits: rustc")
parser.add_argument("--found", default="What was it? What matches, with what each used",
                    help="the caption under what was found")
parser.add_argument("--frames", required=True, help="where the frames go")
parser.add_argument("--settle", type=float, default=80,
                    help="seconds to wait before the first frame: long enough for the viewer's own terminal to be in the store")
args = parser.parse_args()

FRAMES = args.frames
BIN, DATA = args.bin, args.data
TMUX = ["tmux", "-L", "timeless-acct-demo"]
COLS, ROWS = 118, 32
frames = []


def tmux(*args):
    return subprocess.run(TMUX + list(args), capture_output=True, text=True).stdout


def key(*keys):
    tmux("send-keys", "-t", "demo", *keys)


def text(chars):
    tmux("send-keys", "-t", "demo", "-l", chars)


def shot(ms, caption):
    name = f"{len(frames):04d}.ansi"
    open(os.path.join(FRAMES, name), "w").write(tmux("capture-pane", "-t", "demo", "-e", "-p"))
    frames.append((name, ms, caption))


def typed(chars, caption, each=95):
    for ch in chars:
        text(ch)
        time.sleep(0.12)
        shot(each, caption)


os.makedirs(FRAMES, exist_ok=True)
for old in os.listdir(FRAMES):
    os.remove(os.path.join(FRAMES, old))
subprocess.run(TMUX + ["kill-server"], capture_output=True)
subprocess.run(TMUX + ["-f", "/dev/null", "new-session", "-d", "-s", "demo", "-x", str(COLS), "-y", str(ROWS),
                       f"TIMELESS_ACCT_DATA={DATA} {BIN} watch"])
# Long enough for the viewer's own terminal to be in the store, and out
# of the way at the top of the list.
time.sleep(args.settle)

def wait_for(found, timeout=60):
    """Until the screen says so: a search of a busy hour takes seconds."""
    end = time.time() + timeout
    while time.time() < end:
        if found(tmux("capture-pane", "-t", "demo", "-p").split("\n")):
            return
        time.sleep(0.2)
    raise SystemExit("the screen never showed what was waited for")


c = "Live: every service, container and application on the host"
for _ in range(3):
    shot(1000, c)
    time.sleep(1.0)

c = "t   go to a moment: a date and a time, or -2d, or 14:30"
key("t"); time.sleep(0.4); shot(700, c)
typed(args.at, c)
shot(600, c)

key("Enter")
wait_for(lambda rows: "\u25c0" in rows[0]); time.sleep(0.8)
shot(3600, "A spike on the timeline, and \u25b2 is where we are on it")

c = "\u2190 \u2192  ten seconds     , .  a minute     < >  ten minutes     [ ]  an hour"
for _ in range(3):
    key("Left"); time.sleep(0.9); shot(430, c)
for _ in range(7):
    key("Right"); time.sleep(0.9); shot(430, c)
shot(1000, c)

key("4")
wait_for(lambda rows: rows[7].startswith("ENDED")); time.sleep(0.5)
shot(2600, "4   every process that ended, however briefly it lived")

c = "/   only what matches: a command, a unit, a status, a user"
key("/"); time.sleep(0.5); shot(500, c)
typed(args.look_for, c, each=130)
key("Enter")
wait_for(lambda rows: rows[6].rstrip().endswith("only " + args.look_for)); time.sleep(0.5)
shot(3800, args.found)

before = tmux("capture-pane", "-t", "demo", "-p")
key("Enter")
wait_for(lambda rows: "\n".join(rows) != before.rstrip("\n") and "\n".join(rows).strip() != before.strip())
time.sleep(0.6)
shot(4200, "enter   the whole record: what it ran, what it used, how it ended")
key("Enter"); time.sleep(0.8)                 # any key closes the record
key("Escape")                                 # and this, what was looked for
wait_for(lambda rows: "only" not in rows[6]); time.sleep(0.5)

key("3")
wait_for(lambda rows: rows[7].startswith("STARTED")); time.sleep(0.8)
shot(3800, "3   jobs: each pipeline, build step and service run, as the tree it was")

c = "-  +   the timeline, from ten minutes to a week"
key("1"); wait_for(lambda rows: rows[7].startswith("UNIT")); time.sleep(0.5)
key("-"); time.sleep(2.0); shot(1700, c)
key("-"); time.sleep(2.0); shot(3000, c)

key("l")
wait_for(lambda rows: "LIVE" in rows[0]); time.sleep(1.2)
shot(3000, "l   back to now")

key("q"); time.sleep(0.5)
subprocess.run(TMUX + ["kill-server"], capture_output=True)
with open(os.path.join(FRAMES, "manifest.tsv"), "w") as out:
    for name, ms, caption in frames:
        out.write(f"{name}\t{ms}\t{caption}\n")
print(len(frames), "frames,", sum(ms for _, ms, _ in frames) / 1000, "seconds")
