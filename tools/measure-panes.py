"""Print the colour profile across the right-hand side of the window.

The border scan said the reader pane starts at x=1345, which would make it 15 CSS px wide. That
is a strong claim, so this dumps the actual pixels to show where --surface (list pane) gives way
to --bg (reader pane).
"""

from PIL import Image

SHOT = r"C:\D\agent\easy-email\target\mail.png"
img = Image.open(SHOT).convert("RGB")
px = img.load()

SURFACE = (23, 29, 41)   # --surface
BG = (16, 21, 31)        # --bg
BORDER = (42, 51, 69)    # --border


def name(colour):
    def near(t, tol=6):
        return all(abs(a - b) <= tol for a, b in zip(colour, t))

    if near(BORDER):
        return "BORDER"
    if near(SURFACE):
        return "surface"
    if near(BG):
        return "bg"
    return str(colour)


for y in (250, 400, 600):
    print(f"\n--- row y={y} ---")
    runs = []
    for x in range(1200, 1370):
        label = name(px[x, y])
        if not runs or runs[-1][0] != label:
            runs.append([label, x, x])
        else:
            runs[-1][2] = x
    for label, a, b in runs:
        print(f"  {label:<10} x={a}..{b}  ({b - a + 1}px)")
