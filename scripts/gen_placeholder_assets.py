# Rusty-Tuber placeholder-asset generator.
#
# Produces the layered placeholder art (base body + mouth levels + eye frames)
# so the server and OBS Browser Source can be exercised without real art. All
# layers share a 512x512 transparent canvas so they composite pixel-aligned.
#
# This regenerates the WHOLE character folder's placeholder layers. If you've
# dropped in real art, back it up first or edit the paths below.
#
# Usage:  python3 scripts/gen_placeholder_assets.py
# Requires Pillow.

from PIL import Image, ImageDraw, ImageFont
import os

ROOT = os.path.join("assets", "characters", "default_macaw")
W = H = 512

# Mouth level -> mouth-ellipse radius (closed is a thin line, not an ellipse).
MOUTH_R = {"closed": 0, "partial": 12, "medium": 26, "open": 44}
MOUTH_LEVELS = ["closed", "partial", "medium", "open"]

BODY = (60, 130, 200, 255)      # macaw-ish blue body
EYE_WHITE = (255, 255, 255, 255)
EYE_PUPIL = (20, 20, 20, 255)
MOUTH_FILL = (60, 15, 30, 255)
LID = (30, 20, 40, 255)


def draw_body(d):
    """The static body layer: a rounded body shape with a beak hint."""
    d.rounded_rectangle([40, 60, W - 40, H - 40], radius=70, fill=BODY)
    d.text((60, 70), "base/body", fill=(255, 255, 255, 230), font=FONT)


def draw_eye_layer(closed):
    """An eye layer: two eyes (open) or closed lids."""
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    for ex in (W // 2 - 70, W // 2 + 40):
        if closed:
            d.rounded_rectangle([ex - 4, 192, ex + 34, 202], radius=4, fill=LID)
        else:
            d.ellipse([ex, 180, ex + 30, 210], fill=EYE_WHITE)
            d.ellipse([ex + 8, 188, ex + 24, 204], fill=EYE_PUPIL)
    return img


def draw_mouth_layer(level):
    """A mouth layer for one aperture level."""
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    cx, cy = W // 2 - 15, 320
    rx = 70
    if level == "closed":
        d.rounded_rectangle(
            [cx - rx // 2, cy - 3, cx + rx, cy + 3], radius=3, fill=MOUTH_FILL
        )
    else:
        ry = MOUTH_R[level]
        d.ellipse([cx, cy - ry, cx + rx, cy + ry], fill=MOUTH_FILL)
    d.text((60, 70), f"mouths/{level}", fill=(255, 255, 255, 230), font=FONT)
    return img


def save(img, *parts):
    out = os.path.join(ROOT, *parts)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    img.save(out)
    print("wrote", out)


def main():
    # base/body.png
    body = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    draw_body(ImageDraw.Draw(body))
    save(body, "base", "body.png")

    # mouths/<level>.png
    for level in MOUTH_LEVELS:
        save(draw_mouth_layer(level), "mouths", f"{level}.png")

    # eyes/open.png + eyes/closed.png
    save(draw_eye_layer(closed=False), "eyes", "open.png")
    save(draw_eye_layer(closed=True), "eyes", "closed.png")

    print("done")


FONT = None
if __name__ == "__main__":
    try:
        FONT = ImageFont.load_default()
    except Exception:
        FONT = None
    main()
