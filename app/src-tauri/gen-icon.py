# Generates app-icon.png (1024x1024): the tray's Material "sync" glyph —
# same geometry as tray_icon_ex() in lib.rs — drawn white on the app's
# accent-blue rounded square. Bundle icons (notifications, dock, taskbar,
# installers) are regenerated from it with `npx tauri icon`.
import numpy as np
from PIL import Image, ImageDraw

SS = 4096  # supersample; downscaled 4x to 1024
OUT = 1024

# --- glyph mask, exact geometry from lib.rs (24pt space) ---------------
scale = SS / 24.0
c = 12.0 * scale
r_in, r_out = 6.0 * scale, 8.0 * scale
tri = [(12.0 * scale, 1.0 * scale), (12.0 * scale, 9.0 * scale), (8.0 * scale, 5.0 * scale)]

ys, xs = np.mgrid[0:SS, 0:SS].astype(np.float32)
xs += 0.5
ys += 0.5

def sign(px, py, a, b):
    return (px - b[0]) * (a[1] - b[1]) - (a[0] - b[0]) * (py - b[1])

def in_tri(px, py):
    d1 = sign(px, py, tri[0], tri[1])
    d2 = sign(px, py, tri[1], tri[2])
    d3 = sign(px, py, tri[2], tri[0])
    has_neg = (d1 < 0) | (d2 < 0) | (d3 < 0)
    has_pos = (d1 > 0) | (d2 > 0) | (d3 > 0)
    return ~(has_neg & has_pos)

def half(px, py):
    dx, dy = px - c, py - c
    d = np.sqrt(dx * dx + dy * dy)
    ang = np.arctan2(dy, dx)
    ring = (d >= r_in) & (d <= r_out) & (ang >= -np.pi / 2) & (ang <= np.pi / 4)
    return ring | in_tri(px, py)

glyph = half(xs, ys) | half(2 * c - xs, 2 * c - ys)

# Scale glyph to sit inside the plate: shrink to 62% around the center.
g_img = Image.fromarray((glyph * 255).astype(np.uint8), "L")
inner = int(SS * 0.62)
g_img = g_img.resize((inner, inner), Image.LANCZOS)
glyph_full = Image.new("L", (SS, SS), 0)
glyph_full.paste(g_img, ((SS - inner) // 2, (SS - inner) // 2))

# --- rounded-square plate with vertical accent gradient ----------------
inset = int(SS * 0.055)          # small margin like macOS icon grid
radius = int(SS * 0.225)         # Apple-ish corner radius
plate_mask = Image.new("L", (SS, SS), 0)
ImageDraw.Draw(plate_mask).rounded_rectangle(
    [inset, inset, SS - inset, SS - inset], radius=radius, fill=255
)
top, bottom = (10, 132, 255), (0, 106, 220)   # #0a84ff -> #006adc
grad = np.zeros((SS, SS, 3), dtype=np.uint8)
t = np.linspace(0.0, 1.0, SS)[:, None]
for i in range(3):
    grad[:, :, i] = ((1 - t) * top[i] + t * bottom[i]).astype(np.uint8)
plate = Image.fromarray(grad, "RGB").convert("RGBA")
plate.putalpha(plate_mask)

# --- composite white glyph over the plate -------------------------------
white = Image.new("RGBA", (SS, SS), (255, 255, 255, 255))
icon = Image.composite(white, plate, glyph_full)
icon.putalpha(Image.composite(Image.new("L", (SS, SS), 255), plate_mask, glyph_full))
# alpha: plate everywhere, opaque where glyph (glyph never leaves the plate)

icon = icon.resize((OUT, OUT), Image.LANCZOS)
icon.save("app-icon.png")
print("wrote app-icon.png", icon.size)
