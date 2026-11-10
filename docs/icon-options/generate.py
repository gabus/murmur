#!/usr/bin/env python3
"""Generates the Murmur icon options and the comparison sheet (all-options.svg).
Render with: rsvg-convert -o all-options.png all-options.svg"""
import math, os

def squircle(gid, c1, c2):
    return (f'<defs><linearGradient id="{gid}" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="{c1}"/>'
            f'<stop offset="1" stop-color="{c2}"/></linearGradient><filter id="s{gid}" x="-10%" y="-10%" width="120%" height="130%">'
            f'<feDropShadow dx="0" dy="3" stdDeviation="3" flood-opacity=".25"/></filter></defs>'
            f'<rect x="10" y="10" width="108" height="108" rx="28" fill="url(#{gid})" filter="url(#s{gid})"/>')

def flock(x0, x1, cy, amp, thick, n, rmin, rmax, lanes=3):
    """A ribbon of dots along a sine: a stylised starling murmuration."""
    out = []
    for i in range(n):
        t = i / (n - 1)
        x = x0 + (x1 - x0) * t
        y = cy + amp * math.sin(2 * math.pi * 0.85 * t + 0.6)
        dy = amp * 2 * math.pi * 0.85 / (x1 - x0) * math.cos(2 * math.pi * 0.85 * t + 0.6)
        nx, ny = -dy / math.hypot(1, dy), 1 / math.hypot(1, dy)
        w = thick * math.sin(math.pi * t) ** 0.8
        for lane in range(lanes):
            o = (lane - (lanes - 1) / 2) * w
            jitter = 0.35 * w * math.sin(i * 1.9 + lane * 2.7)
            r = rmin + (rmax - rmin) * math.sin(math.pi * t) ** 1.5 * (1 - 0.25 * abs(lane - 1))
            out.append((x + nx * (o + jitter), y + ny * (o + jitter), r))
    return out

def wave_then_line(x0, xw, x1, y, amp, periods, steps=40):
    pts = [(x0 + (xw - x0) * s / steps,
            y - amp * math.sin(2 * math.pi * periods * s / steps) * (1 - 0.45 * s / steps)) for s in range(steps + 1)]
    return "M" + " L".join(f"{px:.2f} {py:.2f}" for px, py in pts) + f" L{x1} {y}"

OPTS = {
 "1-speech-wave": ("1 · Speech wave",
   squircle("g1", "#6d5dfc", "#b44bd8") +
   '<path d="M38 30h52a14 14 0 0 1 14 14v34a14 14 0 0 1-14 14H62l-18 14V92h-6a14 14 0 0 1-14-14V44a14 14 0 0 1 14-14z" fill="#fff" fill-opacity=".18"/>'
   '<g fill="#fff"><rect x="40" y="54" width="8" height="18" rx="4"/><rect x="52" y="45" width="8" height="36" rx="4"/>'
   '<rect x="64" y="38" width="8" height="50" rx="4"/><rect x="76" y="47" width="8" height="32" rx="4"/><rect x="88" y="55" width="8" height="16" rx="4"/></g>',
   '<path d="M4 1.5h8A2.5 2.5 0 0 1 14.5 4v6A2.5 2.5 0 0 1 12 12.5H7.6L4.5 14.8v-2.3H4A2.5 2.5 0 0 1 1.5 10V4A2.5 2.5 0 0 1 4 1.5z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/>'
   '<g fill="currentColor"><rect x="4.6" y="5.6" width="1.4" height="2.8" rx=".7"/><rect x="7.3" y="3.9" width="1.4" height="6.2" rx=".7"/><rect x="10" y="5" width="1.4" height="4" rx=".7"/></g>'),
 "2-wave-m": ("2 · Wave M",
   squircle("g2", "#0ea5a4", "#2563eb") +
   '<path d="M26 86 C34 86 36 40 44 40 C52 40 56 76 64 76 C72 76 76 40 84 40 C92 40 94 86 102 86" fill="none" stroke="#fff" stroke-width="11" stroke-linecap="round" stroke-linejoin="round"/>',
   '<path d="M1.6 12.2 C3 12.2 3.4 3.8 5 3.8 C6.5 3.8 7 10.2 8 10.2 C9 10.2 9.5 3.8 11 3.8 C12.6 3.8 13 12.2 14.4 12.2" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>'),
 "3-voice-cursor": ("3 · Voice cursor",
   squircle("g3", "#ff8a4c", "#ff3d71") +
   '<circle cx="32" cy="64" r="7" fill="#fff"/><g fill="none" stroke="#fff" stroke-width="7" stroke-linecap="round">'
   '<path d="M44 48 A20 20 0 0 1 44 80"/><path d="M54 36 A34 34 0 0 1 54 92" stroke-opacity=".75"/></g>'
   '<g fill="#fff"><rect x="80" y="34" width="8" height="60" rx="4"/><rect x="70" y="30" width="28" height="7" rx="3.5"/><rect x="70" y="91" width="28" height="7" rx="3.5"/></g>',
   '<circle cx="2.8" cy="8" r="1.4" fill="currentColor"/><path d="M5.2 5.2 A3.6 3.6 0 0 1 5.2 10.8" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>'
   '<g fill="currentColor"><rect x="10.8" y="2.6" width="1.5" height="10.8" rx=".75"/><rect x="9" y="1.8" width="5.1" height="1.4" rx=".7"/><rect x="9" y="12.8" width="5.1" height="1.4" rx=".7"/></g>'),
 "4-murmuration": ("4 · Murmuration",
   squircle("g4", "#1e3a8a", "#db2777") + '<g fill="#fff">' +
   "".join(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r:.2f}"/>' for x, y, r in flock(26, 102, 64, 16, 7, 15, 1.4, 4.2)) + '</g>',
   '<g fill="currentColor">' +
   "".join(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{r:.2f}"/>' for x, y, r in flock(1.8, 14.2, 8, 3.3, 0, 7, 0.75, 1.55, lanes=1)) + '</g>'),
 "5-wave-to-text": ("5 · Wave to text",
   squircle("g5", "#10b981", "#0d9488") +
   f'<path d="{wave_then_line(28, 62, 100, 46, 11, 2.0)}" fill="none" stroke="#fff" stroke-width="7" stroke-linecap="round" stroke-linejoin="round"/>'
   '<g stroke="#fff" stroke-width="7" stroke-linecap="round" stroke-opacity=".85"><path d="M28 70 H100"/><path d="M28 89 H76"/></g>',
   f'<path d="{wave_then_line(1.8, 7.2, 14.2, 4.6, 2.3, 1.5)}" fill="none" stroke="currentColor" stroke-width="1.35" stroke-linecap="round" stroke-linejoin="round"/>'
   '<g stroke="currentColor" stroke-width="1.35" stroke-linecap="round"><path d="M1.8 9 H14.2"/><path d="M1.8 12.6 H10"/></g>'),
}

def main():
    here = os.path.dirname(os.path.abspath(__file__))
    for key, (_, app, tray) in OPTS.items():
        os.makedirs(os.path.join(here, key), exist_ok=True)
        with open(os.path.join(here, key, "app.svg"), "w") as f:
            f.write(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128" width="128" height="128">{app}</svg>\n')
        with open(os.path.join(here, key, "tray-symbolic.svg"), "w") as f:
            f.write(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" width="16" height="16" fill="none" color="#232629">{tray}</svg>\n')
    W, RH, TOP = 1060, 175, 70
    def tray_at(t, x, y, size, color):
        return f'<svg x="{x}" y="{y}" width="{size}" height="{size}" viewBox="0 0 16 16" color="{color}">{t}</svg>'
    rows = []
    for i, (key, (label, app, tray)) in enumerate(OPTS.items()):
        y = TOP + i * RH
        a = app.replace('id="g', f'id="r{i}g').replace('url(#g', f'url(#r{i}g').replace('id="sg', f'id="r{i}sg').replace('url(#sg', f'url(#r{i}sg')
        r = f'<text x="20" y="{y+80}" font-family="sans-serif" font-size="17" font-weight="600" fill="#222">{label}</text>'
        for (x, yy, s) in ((185, y, 128), (330, y + 32, 64), (408, y + 48, 32)):
            r += f'<svg x="{x}" y="{yy}" width="{s}" height="{s}" viewBox="0 0 128 128">{a}</svg>'
        r += f'<rect x="470" y="{y+20}" width="250" height="40" rx="6" fill="#2a2e32"/>' + tray_at(tray, 488, y + 29, 22, "#fcfcfc") + tray_at(tray, 522, y + 29, 22, "#da4453")
        r += f'<text x="556" y="{y+45}" font-family="sans-serif" font-size="12" fill="#bbb">idle · listening (22 px)</text>'
        r += f'<rect x="470" y="{y+72}" width="250" height="40" rx="6" fill="#eff0f1" stroke="#d0d0d0"/>' + tray_at(tray, 488, y + 81, 22, "#232629") + tray_at(tray, 522, y + 81, 22, "#da4453")
        r += f'<text x="556" y="{y+97}" font-family="sans-serif" font-size="12" fill="#555">light panel</text>'
        r += f'<rect x="745" y="{y+8}" width="112" height="112" rx="10" fill="#2a2e32"/>' + tray_at(tray, 755, y + 18, 92, "#fcfcfc")
        r += f'<rect x="875" y="{y+8}" width="112" height="112" rx="10" fill="#eff0f1" stroke="#d0d0d0"/>' + tray_at(tray, 885, y + 18, 92, "#232629")
        rows.append(r)
    hdr = ('<text x="20" y="40" font-family="sans-serif" font-size="22" font-weight="700" fill="#111">Murmur icon options</text>'
           '<text x="185" y="60" font-family="sans-serif" font-size="12" fill="#666">app icon 128 · 64 · 32</text>'
           '<text x="470" y="60" font-family="sans-serif" font-size="12" fill="#666">tray in a panel (actual size)</text>'
           '<text x="745" y="60" font-family="sans-serif" font-size="12" fill="#666">tray glyph enlarged</text>')
    with open(os.path.join(here, "all-options.svg"), "w") as f:
        f.write(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{TOP+5*RH}" viewBox="0 0 {W} {TOP+5*RH}">'
                f'<rect width="100%" height="100%" fill="#fafafa"/>{hdr}{"".join(rows)}</svg>')

if __name__ == "__main__":
    main()
