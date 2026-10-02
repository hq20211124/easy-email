"""Generate the Easy Email application icon.

Produces a multi-resolution .ico so Windows picks a crisp variant for the taskbar, the alt-tab
switcher and the installer. Run from the repository root:

    python tools/make_icon.py
"""

from pathlib import Path

from PIL import Image, ImageDraw

# Windows asks for these sizes; including all of them avoids a blurry upscale in the taskbar.
SIZES = [16, 24, 32, 48, 64, 128, 256]

# A calm blue that reads well on both light and dark taskbars.
BG_TOP = (37, 99, 235)
BG_BOTTOM = (29, 78, 216)
PAPER = (255, 255, 255)
ACCENT = (96, 165, 250)


def rounded_mask(size: int, radius_ratio: float = 0.22) -> Image.Image:
    """An anti-aliased rounded-square mask, drawn at 4x and downsampled."""
    scale = 4
    big = size * scale
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, big - 1, big - 1), radius=int(big * radius_ratio), fill=255
    )
    return mask.resize((size, size), Image.LANCZOS)


def gradient(size: int) -> Image.Image:
    """Vertical gradient background."""
    img = Image.new("RGB", (1, size))
    for y in range(size):
        t = y / max(size - 1, 1)
        img.putpixel(
            (0, y),
            tuple(round(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOTTOM)),
        )
    return img.resize((size, size), Image.BILINEAR)


def envelope(size: int) -> Image.Image:
    """A white envelope with a folded flap, drawn at 4x for smooth edges."""
    scale = 4
    big = size * scale
    layer = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)

    margin = round(big * 0.20)
    top = round(big * 0.30)
    bottom = round(big * 0.72)
    box = (margin, top, big - margin, bottom)
    radius = max(round(big * 0.045), 2)

    d.rounded_rectangle(box, radius=radius, fill=PAPER)

    # The flap: two strokes meeting at the centre, giving the envelope its read-at-a-glance shape.
    left, upper, right, lower = box
    mid_x = (left + right) / 2
    flap_y = top + (lower - top) * 0.52
    width = max(round(big * 0.028), 2)
    d.line([(left, top), (mid_x, flap_y)], fill=ACCENT, width=width, joint="curve")
    d.line([(mid_x, flap_y), (right, top)], fill=ACCENT, width=width, joint="curve")

    return layer.resize((size, size), Image.LANCZOS)


def render(size: int) -> Image.Image:
    base = gradient(size).convert("RGBA")
    base.putalpha(rounded_mask(size))
    base.alpha_composite(envelope(size))
    return base


def main() -> None:
    out_dir = Path(__file__).resolve().parent.parent / "src-tauri" / "icons"
    out_dir.mkdir(parents=True, exist_ok=True)

    frames = [render(s) for s in SIZES]
    ico_path = out_dir / "icon.ico"
    frames[-1].save(ico_path, format="ICO", sizes=[(s, s) for s in SIZES])

    # Tauri also wants a PNG for non-Windows targets and for the installer UI.
    frames[-1].save(out_dir / "icon.png", format="PNG")

    print(f"wrote {ico_path} ({ico_path.stat().st_size} bytes, sizes={SIZES})")
    print(f"wrote {out_dir / 'icon.png'}")


if __name__ == "__main__":
    main()
