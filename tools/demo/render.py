#!/usr/bin/env python3
"""ANSI frame (tmux capture-pane -e) -> an HTML page, for a headless browser
to take a picture of. Usage: render.py in.ansi out.html [caption]"""
import html, re, sys

PALETTE = {  # a dark theme; indexes are the 16 ANSI colours
    0: "#15161e", 1: "#f7768e", 2: "#9ece6a", 3: "#e0af68", 4: "#7aa2f7",
    5: "#bb9af7", 6: "#7dcfff", 7: "#c0caf5", 8: "#6b7394", 9: "#ff7a93",
    10: "#b9f27c", 11: "#ff9e64", 12: "#7da6ff", 13: "#bb9af7", 14: "#0db9d7",
    15: "#ffffff",
}
FG, BG = "#c0caf5", "#1a1b26"
SGR = re.compile(r"\x1b\[([0-9;]*)m")
# What is on the screen is one person's machine, and the name on it is
# theirs to show or not: TIMELESS_DEMO_MASK=name:stand-in puts one for the
# other, and keeps every column where it was.
import os
MASK = tuple(os.environ["TIMELESS_DEMO_MASK"].split(":", 1)) if os.environ.get("TIMELESS_DEMO_MASK") else None


def colour(index):
    if index in PALETTE:
        return PALETTE[index]
    if index >= 232:
        level = 8 + 10 * (index - 232)
        return f"#{level:02x}{level:02x}{level:02x}"
    index -= 16
    steps = [0, 95, 135, 175, 215, 255]
    return "#%02x%02x%02x" % (steps[index // 36], steps[index // 6 % 6], steps[index % 6])


def cells(line):
    style = {"fg": None, "bg": None, "bold": False, "dim": False, "rev": False}
    out, at = [], 0
    for match in SGR.finditer(line):
        out += [(ch, dict(style)) for ch in line[at:match.start()]]
        at = match.end()
        codes = [int(c) if c else 0 for c in match.group(1).split(";")]
        i = 0
        while i < len(codes):
            c = codes[i]
            if c == 0:
                style = {"fg": None, "bg": None, "bold": False, "dim": False, "rev": False}
            elif c == 1: style["bold"] = True
            elif c == 2: style["dim"] = True
            elif c == 7: style["rev"] = True
            elif c == 22: style["bold"] = style["dim"] = False
            elif c == 27: style["rev"] = False
            elif 30 <= c <= 37: style["fg"] = colour(c - 30)
            elif c == 39: style["fg"] = None
            elif 40 <= c <= 47: style["bg"] = colour(c - 40)
            elif c == 49: style["bg"] = None
            elif 90 <= c <= 97: style["fg"] = colour(c - 82)
            elif c in (38, 48) and i + 2 < len(codes) and codes[i + 1] == 5:
                style["fg" if c == 38 else "bg"] = colour(codes[i + 2]); i += 2
            elif c in (38, 48) and i + 4 < len(codes) and codes[i + 1] == 2:
                style["fg" if c == 38 else "bg"] = "#%02x%02x%02x" % tuple(codes[i + 2:i + 5]); i += 4
            i += 1
    out += [(ch, dict(style)) for ch in line[at:]]
    return out


def mask(row):
    """Put one name for another, and keep every column where it was."""
    if not MASK:
        return row
    old, new = MASK
    text = "".join(ch for ch, _ in row)
    at = text.find(old)
    while at >= 0:
        end = at + len(old)
        stop = end
        # A word ends at a space, and at the edge of a box it is drawn in.
        while stop < len(text) and not text[stop].isspace() and text[stop] not in "\u2502\u2510\u2518\u2524\u251c\u2026":
            stop += 1
        short = len(old) - len(new)
        pad_style = row[stop][1] if stop < len(row) else row[stop - 1][1]
        row = (row[:at] + [(ch, row[at][1]) for ch in new] + row[end:stop]
               + [(" ", pad_style)] * short + row[stop:])
        text = "".join(ch for ch, _ in row)
        at = text.find(old, at + len(new))
    return row


def span(text, style):
    fg, bg = style["fg"] or FG, style["bg"] or BG
    if style["rev"]:
        fg, bg = bg, fg
    css = [f"color:{fg}"]
    if bg != BG: css.append(f"background:{bg}")
    if style["bold"]: css.append("font-weight:700")
    if style["dim"]: css.append("opacity:.6")
    return f'<span style="{";".join(css)}">{html.escape(text)}</span>'


def page(lines, caption):
    rows = []
    for line in lines:
        row = mask(cells(line))
        parts, run, current = [], "", None
        for ch, style in row:
            if style != current and run:
                parts.append(span(run, current)); run = ""
            current = style; run += ch
        if run: parts.append(span(run, current))
        rows.append("".join(parts))
    body = "\n".join(rows)
    note = f'<div class="cap">{html.escape(caption)}</div>' if caption else '<div class="cap">&nbsp;</div>'
    return f"""<!doctype html><meta charset="utf-8"><style>
html,body{{margin:0;background:{BG};}}
.frame{{padding:14px 16px 6px 16px;}}
pre{{margin:0;font-family:'CaskaydiaMono Nerd Font Mono','CaskaydiaMono NFM',monospace;
font-size:15px;line-height:19px;color:{FG};font-variant-ligatures:none;}}
.cap{{font-family:'Adwaita Sans','Inter',sans-serif;font-size:17px;font-weight:600;color:#e0af68;
padding:8px 18px 12px 18px;height:22px;}}
</style><div class="frame"><pre>{body}</pre></div>{note}"""


if __name__ == "__main__":
    lines = open(sys.argv[1], encoding="utf-8").read().rstrip("\n").split("\n")
    open(sys.argv[2], "w", encoding="utf-8").write(page(lines, sys.argv[3] if len(sys.argv) > 3 else ""))
