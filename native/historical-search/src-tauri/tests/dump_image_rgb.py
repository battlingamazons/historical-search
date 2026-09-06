"""Test helper only: decodes an image with PIL (the reference implementation
the prototype used) and dumps its dimensions plus raw RGB bytes as base64,
so the Rust decoder can be compared against it pixel-for-pixel.

Usage: python3 dump_image_rgb.py <path-to-image>
"""
import sys
import json
import base64
from PIL import Image

Image.MAX_IMAGE_PIXELS = None

with Image.open(sys.argv[1]) as im:
    rgb = im.convert("RGB")
    print(json.dumps({
        "width": rgb.width,
        "height": rgb.height,
        "rgb_b64": base64.b64encode(rgb.tobytes()).decode("ascii"),
    }))
