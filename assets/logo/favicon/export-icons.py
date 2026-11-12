"""由 1024px 源位图下采样出整套图标；配合 export-icons.sh 使用。"""
import os

from PIL import Image

D = os.path.dirname(os.path.abspath(__file__))


def src(name):
    return Image.open(os.path.join(D, name)).convert("RGBA")


def put(im, path, size):
    im.resize((size, size), Image.LANCZOS).save(os.path.join(D, path), optimize=True)


tile, full, mark, glow = src("_src-tile.png"), src("_src-full.png"), src("_src-mark.png"), src("_src-glow.png")
mono = src("_src-mono.png")

for s in (16, 32, 48, 64, 128, 256):
    put(tile, f"favicon-{s}.png", s)
put(full, "apple-touch-icon.png", 180)
put(full, "icon-192.png", 192)
put(full, "icon-512.png", 512)
for s in (64, 128, 256, 512):
    put(mark, f"mark-{s}.png", s)
    put(glow, f"mark-glow-{s}.png", s)
    put(mono, f"mark-mono-{s}.png", s)

frames = [tile.resize((s, s), Image.LANCZOS) for s in (16, 32, 48)]
frames[0].save(os.path.join(D, "favicon.ico"), format="ICO",
               sizes=[(16, 16), (32, 32), (48, 48)], append_images=frames[1:])

for f in ("_src-tile.png", "_src-full.png", "_src-mark.png", "_src-glow.png", "_src-mono.png"):
    os.remove(os.path.join(D, f))

for n in sorted(os.listdir(D)):
    if n.endswith((".png", ".ico")):
        im = Image.open(os.path.join(D, n))
        a = im.convert("RGBA")
        painted = sum(1 for p in a.getdata() if p[3] > 10)
        print(f"{n:22} {im.size[0]:>4}px  ink={100 * painted / (im.size[0] ** 2):5.1f}%")
