#!/usr/bin/env python3
"""berth 应用图标。

两份图稿，按 Apple 的「每个尺寸段单独设计」惯例：
  full   —— 64px 及以上：侧栏 + 三个状态点 + 提示符 + 块光标
  small  —— 16/32px：去掉提示符与分隔线，点和光标放大，只保留轮廓能读的部分

几何按 macOS 图标网格：1024 画布内 824 的超椭圆方块居中。
用法：python3 make_icon.py [输出目录]
"""
import sys
import numpy as np
from PIL import Image, ImageDraw

S, N = 4, 4096
TILE = 824 * S
OFF = (N - TILE) // 2

FRAME_TOP, FRAME_BOT = '#2e333c', '#171a20'   # 机身渐变
TERM, SIDE, RULE = '#ffffff', '#e3e5e8', '#cfd2d6'
GREEN, ACCENT, IDLE, INK = '#3c843c', '#b35c00', '#a7adb5', '#1f2328'


def hexc(h):
    h = h.lstrip('#')
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4)) + (255,)


def squircle(size, n=5.0):
    """硬边超椭圆；抗锯齿由 4x 超采样后的 LANCZOS 缩小提供。"""
    t = (np.arange(size) + 0.5) / size * 2 - 1
    x, y = np.meshgrid(t, t)
    return Image.fromarray(((np.abs(x) ** n + np.abs(y) ** n <= 1.0) * 255).astype(np.uint8), 'L')


def vgrad(size, top, bot):
    a = np.linspace(0, 1, size)[:, None]
    rgb = (np.array(top[:3], float) * (1 - a) + np.array(bot[:3], float) * a)
    rgb = rgb[:, None, :].repeat(size, axis=1).astype(np.uint8)
    return Image.fromarray(np.dstack([rgb, np.full((size, size, 1), 255, np.uint8)]), 'RGBA')


def draw(detail):
    inset = (38 if detail == 'full' else 20) * S
    img = Image.new('RGBA', (N, N), (0, 0, 0, 0))
    tile = vgrad(TILE, hexc(FRAME_TOP), hexc(FRAME_BOT))
    tile.putalpha(squircle(TILE))
    img.alpha_composite(tile, (OFF, OFF))

    iw = TILE - 2 * inset
    bar = int(iw * (0.34 if detail == 'full' else 0.38))
    win = Image.new('RGBA', (iw, iw), hexc(TERM))
    wd = ImageDraw.Draw(win)
    wd.rectangle([0, 0, bar - 1, iw - 1], fill=hexc(SIDE))
    if detail == 'full':                       # 细分隔线在 32px 下只会变脏
        wd.rectangle([bar - 3 * S, 0, bar - 1, iw - 1], fill=hexc(RULE))
    win.putalpha(squircle(iw))                 # 一次成形，侧栏不会溢出圆角
    x0 = y0 = OFF + inset
    img.alpha_composite(win, (x0, y0))

    d = ImageDraw.Draw(img)
    r = (36 if detail == 'full' else 50) * S
    cx = x0 + bar // 2
    spread = 0.20 if detail == 'full' else 0.25
    for k, col in ((-1, GREEN), (0, ACCENT), (1, IDLE)):
        cy = y0 + iw // 2 + int(iw * spread) * k
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=hexc(col))

    tcy = y0 + iw // 2
    if detail == 'full':
        sx = x0 + bar + 66 * S
        gw, gh, sw = 84 * S, 72 * S, 44 * S
        d.line([(sx, tcy - gh), (sx + gw, tcy), (sx, tcy + gh)],
               fill=hexc(ACCENT), width=sw, joint='curve')
        for e in ((sx, tcy - gh), (sx + gw, tcy), (sx, tcy + gh)):
            d.ellipse([e[0] - sw // 2, e[1] - sw // 2,
                       e[0] + sw // 2, e[1] + sw // 2], fill=hexc(ACCENT))
        bx, cw, ch = sx + gw + 60 * S, 112 * S, 126 * S
    else:
        cw, ch = 160 * S, 180 * S
        bx = x0 + bar + (iw - bar - cw) // 2
    d.rounded_rectangle([bx, tcy - ch // 2, bx + cw, tcy + ch // 2],
                        12 * S, fill=hexc(INK))
    return img.resize((1024, 1024), Image.LANCZOS)


if __name__ == '__main__':
    out = sys.argv[1] if len(sys.argv) > 1 else '.'
    for name in ('full', 'small'):
        draw(name).save(f'{out}/icon-{name}-1024.png')
        print(f'{out}/icon-{name}-1024.png')
