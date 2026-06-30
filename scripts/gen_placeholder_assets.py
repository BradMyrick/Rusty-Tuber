# Rusty-Tuber placeholder-asset generator.
#
# Produces simple, visually-distinct PNGs (coloured body + mouth that opens by
# state, plus `-blink` eyes-closed variants) so the server and OBS Browser
# Source can be exercised without real art.
#
# IMPORTANT: `calm` holds real macaw art supplied by the artist and is intentionally
# NOT regenerated here. Only placeholder emotions are (re)generated.
#
# Usage:  python3 scripts/gen_placeholder_assets.py
# Requires Pillow.

from PIL import Image, ImageDraw, ImageFont
import colorsys
import os

ROOT = os.path.join("assets", "characters", "default_macaw")
W = H = 512

# Placeholder emotions only (calm is real art — do not touch).
EMOTIONS = {
    "surprised": (0.02, ["closed", "open"]),
    "pleased": (0.13, ["closed", "open"]),
    "laughing": (0.33, ["closed", "open"]),
}
MOUTH_R = {"closed": 4, "slight": 12, "medium": 26, "open": 44}


def rgb(h, s=0.6, v=0.9):
    r, g, b = colorsys.hsv_to_rgb(h, s, v)
    return (int(r * 255), int(g * 255), int(b * 255))


def draw_eyes(d, closed):
    for ex in (W // 2 - 70, W // 2 + 40):
        if closed:
            # closed eyelid: a thin rounded bar over the eye socket
            d.rounded_rectangle([ex - 4, 192, ex + 34, 202], radius=4, fill=(30, 20, 40, 255))
        else:
            d.ellipse([ex, 180, ex + 30, 210], fill=(255, 255, 255, 255))
            d.ellipse([ex + 8, 188, ex + 24, 204], fill=(20, 20, 20, 255))


def main():
    try:
        font = ImageFont.load_default()
    except Exception:
        font = None
    for emotion, (hue, mouths) in EMOTIONS.items():
        body = rgb(hue)
        for mouth in mouths:
            # eyes-open frame: <mouth>.png
            img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
            d = ImageDraw.Draw(img)
            d.rounded_rectangle([40, 60, W - 40, H - 40], radius=70, fill=body + (255,))
            draw_eyes(d, closed=False)
            cx, cy, rx, ry = W // 2 - 15, 320, 70, MOUTH_R[mouth]
            if mouth == "closed":
                d.rounded_rectangle([cx - rx // 2, cy - 3, cx + rx, cy + 3], radius=3, fill=(40, 20, 30, 255))
            else:
                d.ellipse([cx, cy - ry, cx + rx, cy + ry], fill=(60, 15, 30, 255))
            d.text((60, 70), f"{emotion}/{mouth}", fill=(255, 255, 255, 230), font=font)
            out = os.path.join(ROOT, emotion, f"{mouth}.png")
            os.makedirs(os.path.dirname(out), exist_ok=True)
            img.save(out)
            print("wrote", out)

            # eyes-closed frame: <mouth>-blink.png
            img2 = Image.new("RGBA", (W, H), (0, 0, 0, 0))
            d2 = ImageDraw.Draw(img2)
            d2.rounded_rectangle([40, 60, W - 40, H - 40], radius=70, fill=body + (255,))
            draw_eyes(d2, closed=True)
            if mouth == "closed":
                d2.rounded_rectangle([cx - rx // 2, cy - 3, cx + rx, cy + 3], radius=3, fill=(40, 20, 30, 255))
            else:
                d2.ellipse([cx, cy - ry, cx + rx, cy + ry], fill=(60, 15, 30, 255))
            d2.text((60, 70), f"{emotion}/{mouth}-blink", fill=(255, 255, 255, 230), font=font)
            out2 = os.path.join(ROOT, emotion, f"{mouth}-blink.png")
            img2.save(out2)
            print("wrote", out2)
    print("done")


if __name__ == "__main__":
    main()
