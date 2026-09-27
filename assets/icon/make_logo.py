#!/usr/bin/env python3
"""由 make_icon.py 的同一套几何生成 assets/logo.svg，README 用。

图标是 PNG（macOS 要 .icns），README 需要矢量，所以两边共用参数而不是
共用产物。超椭圆用采样点铺成路径，圆角矩形近似在 1024 视口下看得出差别。
"""
import numpy as np

TILE, OFF = 824, 100
INSET, BAR_F, DOT_R, SPREAD = 38, 0.34, 36, 0.20
FRAME_TOP, FRAME_BOT = '#2e333c', '#171a20'
TERM, SIDE, RULE = '#ffffff', '#e3e5e8', '#cfd2d6'
GREEN, ACCENT, IDLE, INK = '#3c843c', '#b35c00', '#a7adb5', '#1f2328'


def superellipse(cx, cy, half, n=5.0, pts=360):
    """|x|^n + |y|^n = 1 的参数化采样，与图标的掩码同一条曲线。"""
    t = np.linspace(0, 2 * np.pi, pts, endpoint=False)
    c, s = np.cos(t), np.sin(t)
    x = np.sign(c) * np.abs(c) ** (2 / n) * half + cx
    y = np.sign(s) * np.abs(s) ** (2 / n) * half + cy
    d = f"M{x[0]:.1f} {y[0]:.1f}" + "".join(f"L{a:.1f} {b:.1f}" for a, b in zip(x[1:], y[1:]))
    return d + "Z"


half_t = TILE / 2
cx = cy = OFF + half_t
iw = TILE - 2 * INSET
half_i = iw / 2
x0 = y0 = OFF + INSET
bar = int(iw * BAR_F)
tcy = y0 + iw / 2

sx, gw, gh, sw = x0 + bar + 66, 84, 72, 44
bx, cw, ch = sx + gw + 60, 112, 126

dots = "".join(
    f'\n  <circle cx="{x0 + bar / 2:.0f}" cy="{y0 + iw / 2 + iw * SPREAD * k:.0f}" '
    f'r="{DOT_R}" fill="{c}"/>'
    for k, c in ((-1, GREEN), (0, ACCENT), (1, IDLE))
)

svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024" role="img" aria-label="berth logo">
  <title>berth</title>
  <defs>
    <linearGradient id="body" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{FRAME_TOP}"/>
      <stop offset="1" stop-color="{FRAME_BOT}"/>
    </linearGradient>
    <clipPath id="win"><path d="{superellipse(cx, cy, half_i)}"/></clipPath>
  </defs>

  <!-- 机身 -->
  <path d="{superellipse(cx, cy, half_t)}" fill="url(#body)"/>

  <!-- 窗口：左侧栏 + 右终端，整体按超椭圆裁剪 -->
  <g clip-path="url(#win)">
    <rect x="{x0}" y="{y0}" width="{iw}" height="{iw}" fill="{TERM}"/>
    <rect x="{x0}" y="{y0}" width="{bar}" height="{iw}" fill="{SIDE}"/>
    <rect x="{x0 + bar - 3}" y="{y0}" width="3" height="{iw}" fill="{RULE}"/>
  </g>

  <!-- session 状态点：运行 / 需关注 / 空闲 -->{dots}

  <!-- 行首提示符与块光标 -->
  <path d="M{sx} {tcy - gh:.0f}L{sx + gw} {tcy:.0f}L{sx} {tcy + gh:.0f}" fill="none"
        stroke="{ACCENT}" stroke-width="{sw}" stroke-linecap="round" stroke-linejoin="round"/>
  <rect x="{bx}" y="{tcy - ch / 2:.0f}" width="{cw}" height="{ch}" rx="12" fill="{INK}"/>
</svg>
'''
open('/Users/xsser/projects/berth/assets/logo.svg', 'w').write(svg)
print("assets/logo.svg")
