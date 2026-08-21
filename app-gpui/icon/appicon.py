#!/usr/bin/env python3
"""Render `app-gpui/AppIcon.icns` and the two SVG marks from the geometry
constants below.

Both artifacts are committed, and this script is not a build step: nothing in
`make app` runs it. It exists so the icon is a *source* rather than a binary
somebody has to open a design tool to touch, and so the bundle icon and the
About window's art cannot drift — they are two renderings of the one set of
numbers in this file.

Pure standard library on purpose. The obvious way to make an `.icns` is
`iconutil -c icns` over an `.iconset`, which needs a Mac; the obvious way to
rasterize is a real SVG renderer, which needs a C library. Neither is present
in a Builder VM, and requiring either would mean the icon could only be
regenerated on the one machine that also happens to be able to install the app.
So: a PNG encoder (zlib + struct), an ICNS packer, and an analytic rasterizer
for the two shapes the mark is made of.

    python3 app-gpui/icon/appicon.py [--check]

`--check` re-renders and compares against what is committed without writing,
exiting non-zero on a difference. Output is deterministic — no timestamps, a
fixed zlib level — so a regeneration on any machine is byte-identical.
"""

import argparse
import pathlib
import struct
import sys
import zlib

# --- the mark ---------------------------------------------------------------
#
# The double diamond: the architecture this whole project is built around
# (README's diagram, issue #744). Two diamonds meeting at a shared centre
# vertex — parallel exploration converging on a spec, then a serial build
# converging on a merge.
#
# Both lobes are *solid*. An outlined left lobe reads better at 1024 and is
# what the diagram looks like, but the mark is ~9px wide at 16px, so each lobe
# is ~4.5px and any stroke that survives there is a slab at full size. Two
# flat colours carry the same before/after without a stroke to lose.
#
# Colours follow the app's existing gruvbox lean (`workspace.rs`'s MIC_SVG is
# #928374). Aqua → gold is cool → warm, which reads as direction at any size,
# and neither lobe is a value-faded version of the other, so nothing looks
# broken or half-loaded when it is 5px across.

CANVAS = 1024.0  # the macOS icon grid

FIELD_SIDE = 824.0  # the art box every macOS icon sits in: 1024 less 2x100 margin
FIELD_RADIUS = 185.0

FIELD_TOP = (0x3C, 0x38, 0x36)  # gruvbox dark1
FIELD_BOTTOM = (0x1D, 0x20, 0x21)  # gruvbox dark0_hard

DIAMOND_HALF_W = 172.0
DIAMOND_HALF_H = 200.0
DIAMOND_GAP = 0.0  # they share the centre vertex, as the diagram does

LEFT_COLOR = (0x83, 0xA5, 0x98)  # gruvbox aqua — the scouts
RIGHT_COLOR = (0xFA, 0xBD, 0x2F)  # gruvbox bright yellow — what ships

# --- the icns member list ---------------------------------------------------
#
# Exactly what `iconutil -c icns` emits for a full `.iconset`, in its order.
# 32, 256 and 512 each appear twice under two OSTypes: a reader selects by
# type (a @1x slot and a @2x slot of a smaller nominal size), not by pixels,
# so dropping the "duplicate" costs the icon one of the two slots.
MEMBERS = [
    (b"icp4", 16),  # icon_16x16
    (b"ic11", 32),  # icon_16x16@2x
    (b"icp5", 32),  # icon_32x32
    (b"ic12", 64),  # icon_32x32@2x
    (b"ic07", 128),  # icon_128x128
    (b"ic13", 256),  # icon_128x128@2x
    (b"ic08", 256),  # icon_256x256
    (b"ic14", 512),  # icon_256x256@2x
    (b"ic09", 512),  # icon_512x512
    (b"ic10", 1024),  # icon_512x512@2x
]


# --- rasterizer -------------------------------------------------------------
#
# Both shapes are closed-form, so there is no scene graph and no SVG renderer:
# the rounded rect has an exact signed distance and a diamond is
# |x|/a + |y|/b <= 1. Antialiasing is 4x4 supersampling, taken only for pixels
# a boundary can actually cross — everything else is one sample.

_SS = 4
_SS_OFFSETS = [(i + 0.5) / _SS for i in range(_SS)]


def _field_sdf(x, y):
    """Signed distance to the rounded rect, negative inside."""
    half = FIELD_SIDE / 2.0 - FIELD_RADIUS
    dx = abs(x - CANVAS / 2.0) - half
    dy = abs(y - CANVAS / 2.0) - half
    ox, oy = max(dx, 0.0), max(dy, 0.0)
    return (ox * ox + oy * oy) ** 0.5 + min(max(dx, dy), 0.0) - FIELD_RADIUS


_LEFT_CX = CANVAS / 2.0 - DIAMOND_HALF_W - DIAMOND_GAP / 2.0
_RIGHT_CX = CANVAS / 2.0 + DIAMOND_HALF_W + DIAMOND_GAP / 2.0
_DIAMOND_SCALE = (DIAMOND_HALF_W**2 + DIAMOND_HALF_H**2) ** 0.5


def _diamond(x, y, cx):
    """<= 0 inside. Scaled so |value| is a lower bound on distance."""
    v = (
        abs(x - cx) * DIAMOND_HALF_H
        + abs(y - CANVAS / 2.0) * DIAMOND_HALF_W
        - DIAMOND_HALF_W * DIAMOND_HALF_H
    )
    return v / _DIAMOND_SCALE


def _sample(x, y):
    """(r, g, b) of one sample inside the field, or None if outside it."""
    if _field_sdf(x, y) > 0.0:
        return None
    if _diamond(x, y, _LEFT_CX) <= 0.0:
        return LEFT_COLOR
    if _diamond(x, y, _RIGHT_CX) <= 0.0:
        return RIGHT_COLOR
    t = (y - (CANVAS - FIELD_SIDE) / 2.0) / FIELD_SIDE
    t = 0.0 if t < 0.0 else (1.0 if t > 1.0 else t)
    return tuple(
        int(round(FIELD_TOP[i] + (FIELD_BOTTOM[i] - FIELD_TOP[i]) * t)) for i in range(3)
    )


def render(size):
    """RGBA bytes, straight (non-premultiplied) alpha."""
    step = CANVAS / size
    # A boundary can only cross a pixel whose centre is within half a diagonal
    # of it; outside that, one sample is exact.
    slack = step * 0.7072
    sub = [o * step for o in _SS_OFFSETS]
    out = bytearray(size * size * 4)
    at = 0
    for py in range(size):
        cy = (py + 0.5) * step
        y0 = py * step
        for px in range(size):
            cx = (px + 0.5) * step
            near = (
                abs(_field_sdf(cx, cy)) < slack
                or abs(_diamond(cx, cy, _LEFT_CX)) < slack
                or abs(_diamond(cx, cy, _RIGHT_CX)) < slack
            )
            if not near:
                c = _sample(cx, cy)
                if c is None:
                    at += 4
                    continue
                out[at] = c[0]
                out[at + 1] = c[1]
                out[at + 2] = c[2]
                out[at + 3] = 255
                at += 4
                continue
            r = g = b = 0
            hits = 0
            x0 = px * step
            for sy in sub:
                yy = y0 + sy
                for sx in sub:
                    c = _sample(x0 + sx, yy)
                    if c is not None:
                        r += c[0]
                        g += c[1]
                        b += c[2]
                        hits += 1
            if hits:
                out[at] = (r + hits // 2) // hits
                out[at + 1] = (g + hits // 2) // hits
                out[at + 2] = (b + hits // 2) // hits
                out[at + 3] = (hits * 255 + _SS * _SS // 2) // (_SS * _SS)
            at += 4
    return bytes(out)


def halve(rgba, size):
    """Exact 2x2 area average. Composing these is the same box filter as one
    direct downsample, so the 16px member is an exact area average of the 1024
    render rather than a chain of approximations. Averaging is done on
    premultiplied alpha, or transparent pixels drag colour toward black along
    the outer edge."""
    half = size // 2
    out = bytearray(half * half * 4)
    at = 0
    for y in range(half):
        r0 = (2 * y) * size * 4
        r1 = (2 * y + 1) * size * 4
        for x in range(half):
            i0 = r0 + 8 * x
            i1 = r1 + 8 * x
            a = 0
            acc = [0, 0, 0]
            for i in (i0, i0 + 4, i1, i1 + 4):
                pa = rgba[i + 3]
                a += pa
                for k in range(3):
                    acc[k] += rgba[i + k] * pa
            if a:
                for k in range(3):
                    out[at + k] = min(255, (acc[k] + a // 2) // a)
                out[at + 3] = (a + 2) // 4
            at += 4
    return bytes(out)


# --- PNG --------------------------------------------------------------------


def _chunk(tag, payload):
    return (
        struct.pack(">I", len(payload))
        + tag
        + payload
        + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
    )


def png(rgba, size):
    """8-bit RGBA, no interlace, filter 0 on every scanline. No time chunk, and
    a pinned compression level, so the bytes are reproducible."""
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)
        raw += rgba[y * stride : (y + 1) * stride]
    return (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + _chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + _chunk(b"IEND", b"")
    )


# --- ICNS -------------------------------------------------------------------


def icns(members):
    """`icns` + total length, then (OSType, length-including-header, payload).
    The payload for every modern type is the PNG file verbatim — that is all
    `iconutil` does.

    No `TOC ` chunk. It is optional (10.7+), a wrong one is a hard load
    failure where an absent one is spec-compliant, and nothing gains a member
    by having it. No `is32`/`s8mk`/`il32`/`l8mk` either: those RLE pairs exist
    for 10.6-and-earlier readers and `LSMinimumSystemVersion` here is 15.0."""
    body = b"".join(
        tag + struct.pack(">I", len(data) + 8) + data for tag, data in members
    )
    return b"icns" + struct.pack(">I", len(body) + 8) + body


def verify(blob, members):
    """Re-parse what was just written. Cheap, and the failure it catches — a
    length field off by the 8-byte header — produces a file that looks fine
    until macOS silently shows the generic blank."""
    assert blob[:4] == b"icns", "bad magic"
    assert struct.unpack(">I", blob[4:8])[0] == len(blob), "header length disagrees"
    at, seen = 8, []
    while at < len(blob):
        tag = blob[at : at + 4]
        (n,) = struct.unpack(">I", blob[at + 4 : at + 8])
        assert n >= 8 and at + n <= len(blob), f"chunk {tag!r} overruns"
        payload = blob[at + 8 : at + n]
        assert payload[:8] == b"\x89PNG\r\n\x1a\n", f"chunk {tag!r} is not a PNG"
        w, h = struct.unpack(">II", payload[16:24])
        assert w == h, f"chunk {tag!r} is not square"
        seen.append((tag, w))
        at += n
    assert seen == [(t, s) for t, s in members], f"member list drifted: {seen}"


# --- SVG --------------------------------------------------------------------


def svg(tight=False):
    """The same numbers, for the About window.

    Deliberately dull markup — a `rect`, a `linearGradient`, two `path`s. No
    `clipPath`, no filters, no `use`. gpui rasterizes SVG through resvg and the
    only proof this app can render one at all is `workspace.rs`'s MIC_SVG,
    which is flat-stroked and has no gradient in it.

    The `linearGradient` is the one thing here that MIC_SVG does not exercise,
    and it is kept deliberately rather than inherited: resvg implements SVG 1.1
    gradients in full, `url(#…)` paint references included, and the field is a
    two-stop vertical gradient — the least exotic gradient there is. The
    alternative, a flat field, would make the About window's mark differ from
    the Dock's in the one constant a reader would never guess at, which is
    worse than the risk being taken. What is genuinely untestable off a Mac is
    the *window*, not the gradient, and this file is the wrong place to fix
    that.

    `tight=True` emits the same art with the 100px Dock margin cropped out of
    the viewBox. That margin is required by the `.icns` — every macOS icon sits
    in an 824-of-1024 art box — and is dead weight inline, where it would leave
    the mark sitting optically small and misaligned against the text beside it.
    Same constants, same two paths, one fewer thing for a reader to compensate
    for by eye."""

    def diamond(cx):
        x0, x1 = cx - DIAMOND_HALF_W, cx + DIAMOND_HALF_W
        y0, y1 = CANVAS / 2.0 - DIAMOND_HALF_H, CANVAS / 2.0 + DIAMOND_HALF_H
        return (
            f"M{cx:g} {y0:g}L{x1:g} {CANVAS / 2:g}"
            f"L{cx:g} {y1:g}L{x0:g} {CANVAS / 2:g}Z"
        )

    def hexa(c):
        return "#%02x%02x%02x" % c

    origin = (CANVAS - FIELD_SIDE) / 2.0
    if tight:
        side = FIELD_SIDE
        view = f"{origin:g} {origin:g} {FIELD_SIDE:g} {FIELD_SIDE:g}"
    else:
        side = CANVAS
        view = f"0 0 {CANVAS:g} {CANVAS:g}"
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{side:g}" '
        f'height="{side:g}" viewBox="{view}">'
        '<linearGradient id="f" x1="0" y1="0" x2="0" y2="1">'
        f'<stop offset="0" stop-color="{hexa(FIELD_TOP)}"/>'
        f'<stop offset="1" stop-color="{hexa(FIELD_BOTTOM)}"/>'
        "</linearGradient>"
        f'<rect x="{origin:g}" y="{origin:g}" width="{FIELD_SIDE:g}" '
        f'height="{FIELD_SIDE:g}" rx="{FIELD_RADIUS:g}" ry="{FIELD_RADIUS:g}" '
        'fill="url(#f)"/>'
        f'<path d="{diamond(_LEFT_CX)}" fill="{hexa(LEFT_COLOR)}"/>'
        f'<path d="{diamond(_RIGHT_CX)}" fill="{hexa(RIGHT_COLOR)}"/>'
        "</svg>\n"
    ).encode()


# --- driver -----------------------------------------------------------------


def build():
    """Render 1024 once and halve down. Rendering each member from scratch is
    the same picture and ~16x the work, and successive exact halving is the
    same box filter anyway."""
    ladder = {1024: render(1024)}
    size = 1024
    while size > 16:
        ladder[size // 2] = halve(ladder[size], size)
        size //= 2
    encoded = {s: png(ladder[s], s) for s in sorted({s for _, s in MEMBERS})}
    blob = icns([(tag, encoded[s]) for tag, s in MEMBERS])
    verify(blob, MEMBERS)
    return blob, svg(), svg(tight=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--check",
        action="store_true",
        help="re-render and diff against what is committed; write nothing",
    )
    args = ap.parse_args()

    here = pathlib.Path(__file__).resolve().parent
    blob, mark, tight = build()
    targets = [
        (here.parent / "AppIcon.icns", blob),
        (here / "AppIcon.svg", mark),
        (here / "AppIconMark.svg", tight),
    ]

    bad = False
    for path, data in targets:
        if args.check:
            have = path.read_bytes() if path.exists() else None
            if have != data:
                print(f"{path}: differs from a fresh render", file=sys.stderr)
                bad = True
            else:
                print(f"{path}: up to date ({len(data)} bytes)")
        else:
            path.write_bytes(data)
            print(f"wrote {path} ({len(data)} bytes)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
