# Rusty-Tuber placeholder-asset generator.
# Produces simple, visually-distinct PNGs (coloured body + mouth that opens by
# state) so the server and OBS Browser Source can be exercised without real art.
#
# Usage:  python3 scripts/gen_placeholder_assets.py
# Requires Pillow.

from PIL import Image, ImageDraw, ImageFont
import colorsys
import os

ROOT = os.path.join("assets", "characters", "default_macaw")
W = H = 512

EMOTIONS = {
    "calm": (0.55, ["closed", "slight", "medium", "open"]),
    "surprised": (0.02, ["closed", "open"]),
    "pleased": (0.13, ["closed", "open"]),
    "laughing": (0.33, ["closed", "open"]),
}
MOUTH_R = {"closed": 4, "slight": 12, "medium": 26, "open": 44}


def rgb(h, s=0.6, v=0.9):
    r, g, b = colorsys.hsv_to_rgb(h, s, v)
    return (int(r * 255), int(g * 255), int(b * 255))


def main():
    try:
        font = ImageFont.load_default()
    except Exception:
        font = None
    for emotion, (hue, frames) in EMOTIONS.items():
        body = rgb(hue)
        for mouth in frames:
            img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
            d = ImageDraw.Draw(img)
            d.rounded_rectangle([40, 60, W - 40, H - 40], radius=70, fill=body + (255,))
            for ex in (W // 2 - 70, W // 2 + 40):
                d.ellipse([ex, 180, ex + 30, 210], fill=(255, 255, 255, 255))
                d.ellipse([ex + 8, 188, ex + 24, 204], fill=(20, 20, 20, 255))
            cx, cy = W // 2 - 15, 320
            rx = 70
            ry = MOUTH_R[mouth]
            if mouth == "closed":
                d.rounded_rectangle(
                    [cx - rx // 2, cy - 3, cx + rx, cy + 3], radius=3, fill=(40, 20, 30, 255)
                )
            else:
                d.ellipse([cx, cy - ry, cx + rx, cy + ry], fill=(60, 15, 30, 255))
            d.text((60, 70), f"{emotion}/{mouth}", fill=(255, 255, 255, 230), font=font)
            out = os.path.join(ROOT, emotion, f"{mouth}.png")
            os.makedirs(os.path.dirname(out), exist_ok=True)
            img.save(out)
            print("wrote", out)
    print("done")


if __name__ == "__main__":
    main()
