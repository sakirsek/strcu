"""Makes strcu.ico (the exe, built in by build.rs) and touch.png (the panel on a phone's home screen) from strcu.svg.

Needs Chrome, which draws the SVG at each size, and Pillow: `python icon/make.py`. Commit what it writes.
"""
import os
import pathlib
import subprocess
import sys
import tempfile

from PIL import Image

HERE = pathlib.Path(__file__).resolve().parent
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 256]
TOUCH = 180  # iOS rounds the corners itself, so this one is square
CHROME = os.environ.get('CHROME', r'C:\Program Files\Google\Chrome\Application\chrome.exe')


def main():
    svg = (HERE / 'strcu.svg').read_text(encoding='utf-8')
    # Every size side by side on one transparent page, each drawn at its own size rather than scaled down
    x, spots, cells = 0, [], []
    for s in ICO_SIZES + [TOUCH]:
        bg = 'background:#171A1E;' if s == TOUCH else ''
        cells.append(f'<div style="position:absolute;left:{x}px;top:0;width:{s}px;height:{s}px;{bg}">'
                     + svg.replace('<svg ', f'<svg width="{s}" height="{s}" ', 1) + '</div>')
        spots.append((s, x))
        x += s + 8
    page = ('<!doctype html><html><body style="margin:0;background:transparent;overflow:hidden">'
            + ''.join(cells) + '</body></html>')
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        (tmp / 'page.html').write_text(page, encoding='utf-8')
        shot = tmp / 'shot.png'
        subprocess.run([CHROME, '--headless', '--disable-gpu', '--hide-scrollbars', '--force-device-scale-factor=1',
                        '--default-background-color=00000000', f'--user-data-dir={tmp / "profile"}',
                        f'--window-size={x},{max(ICO_SIZES + [TOUCH])}', f'--screenshot={shot}',
                        (tmp / 'page.html').as_uri()], check=True, capture_output=True, timeout=120)
        sheet = Image.open(shot).convert('RGBA')
        icons = {s: sheet.crop((left, 0, left + s, s)) for s, left in spots}
    icons[TOUCH].convert('RGB').save(HERE / 'touch.png', optimize=True)
    big = icons[256]
    big.save(HERE / 'strcu.ico', sizes=[(s, s) for s in ICO_SIZES], append_images=[icons[s] for s in ICO_SIZES[:-1]])
    print('wrote strcu.ico', ICO_SIZES, 'and touch.png', TOUCH)


if __name__ == '__main__':
    sys.exit(main())
