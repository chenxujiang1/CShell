"""Rebuild the code-native CShell identity. Requires Pillow; no network or user data."""
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1] / "apps/desktop/assets/icons"
ROOT.mkdir(parents=True, exist_ok=True)
SVG = '''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect x="1" y="1" width="62" height="62" rx="14" fill="#121c25" stroke="#344957" stroke-width="1.5"/><path d="M44 16H24Q16 16 16 24V40Q16 48 24 48H44" fill="none" stroke="#56c9b6" stroke-width="6" stroke-linecap="round" stroke-linejoin="round"/><path d="M29 25L37 32L29 39" fill="none" stroke="#f0f5f7" stroke-width="4.5" stroke-linecap="round" stroke-linejoin="round"/><path d="M42 40H49" stroke="#56c9b6" stroke-width="4" stroke-linecap="round"/></svg>'''
(ROOT / "cshell.svg").write_text(SVG, encoding="utf-8")
def icon(size):
    scale = size * 4 / 64
    picture = Image.new("RGBA", (size * 4, size * 4))
    draw = ImageDraw.Draw(picture)
    def p(x, y): return (round(x * scale), round(y * scale))
    draw.rounded_rectangle([p(1, 1), p(63, 63)], round(14 * scale), fill="#121c25", outline="#344957", width=max(1, round(1.5 * scale)))
    # Quadratic C corners are sampled, retaining the same master geometry.
    points = [(44,16),(24,16)]
    points += [((1-t)**2*24+2*(1-t)*t*16+t*t*16,(1-t)**2*16+2*(1-t)*t*16+t*t*24) for t in [i/16 for i in range(1,17)]]
    points += [(16,40)]
    points += [((1-t)**2*16+2*(1-t)*t*16+t*t*24,(1-t)**2*40+2*(1-t)*t*48+t*t*48) for t in [i/16 for i in range(1,17)]]
    points += [(44,48)]
    def line(points, width, color):
        draw.line([p(x,y) for x,y in points], fill=color, width=round(width*scale), joint="curve")
        r=width*scale/2
        for x,y in (points[0], points[-1]):
            cx,cy=p(x,y); draw.ellipse([cx-r,cy-r,cx+r,cy+r],fill=color)
    line(points,6,"#56c9b6"); line([(29,25),(37,32),(29,39)],4.5,"#f0f5f7"); line([(42,40),(49,40)],4,"#56c9b6")
    return picture.resize((size,size),Image.Resampling.LANCZOS)
for size in (16,24,32,48,64,128,256,512):
    icon(size).save(ROOT / f"cshell-{size}.png")
icon(256).save(ROOT / "cshell.ico", sizes=[(s,s) for s in (16,24,32,48,64,128,256)])
(ROOT / "cshell-64.rgba").write_bytes(icon(64).tobytes())
icon(512).save(ROOT / "cshell.icns", sizes=[(s,s) for s in (16,32,64,128,256,512)])
