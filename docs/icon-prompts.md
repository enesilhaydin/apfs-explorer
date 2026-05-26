# Icon & Branding Asset Prompts

AI image generation prompts for all visual assets required by apfs-explorer.
Each entry includes tool recommendations, negative prompts, and style parameters.

---

## 1. Tray Icon (16x16 / 32x32 ICO multi-resolution)

### Concept
"A compact disk silhouette merged with a window frame, communicating APFS volume
access on Windows." Must remain legible at 16x16 — no fine detail.

---

### Variant A — Minimalist Line

**Prompt (DALL-E 3 / Flux Dev):**
```
Minimalist single-color line icon, hard disk platter viewed from slight angle merged
with a window frame outline, 2-tone: dark charcoal #1A1A2E lines on fully transparent
background, ultra-clean vector-style, no gradients, no text, no labels, readable at
16 pixels, icon design, SVG aesthetic
```

**Negative prompt:**
```
text, letters, numbers, logo copy, Apple logo, Windows logo, photorealism, drop shadow,
glow, 3D render, noise, grain, color fringing, blurry edges, AI artifacts
```

**Tool:** DALL-E 3 (square 1:1), post-process in Inkscape to true SVG, then export ICO.
**Seed:** Try seeds 42, 137, 512 for variation.
**Style params (Midjourney):** `--style raw --stylize 50 --ar 1:1 --v 6.1`

---

### Variant B — Geometric Solid

**Prompt (Midjourney / Stable Diffusion XL):**
```
Geometric solid icon, flat design, hexagonal hard disk shape combined with small
window-pane grid in upper-right corner, two-color palette: deep navy #0D1B2A fill
with ice-blue #A8DADC accent, pixel-perfect at 16x16, no anti-aliasing artifacts,
icon sheet style
```

**Negative prompt:**
```
text, drop shadow, 3D bevel, photorealistic, gradient mesh, more than 2 colors,
thin strokes below 2px, Apple branding, Windows flag logo, AI hallucinations
```

**Tool:** Stable Diffusion XL + ControlNet (lineart), cfg=7, steps=30.
**Style params (MJ):** `--style raw --no text,shadow --ar 1:1 --v 6.1`

---

### Variant C — Monochrome Glyph

**Prompt (Flux Schnell / DALL-E 3):**
```
Single monochrome glyph icon, pure black on white, storage disk merged with
mount-point arrow pointing upward, bold strokes 3px minimum, pictogram style,
works perfectly at 16x16 and 32x32, suitable for system tray, no decoration
```

**Negative prompt:**
```
color, gray, gradient, text, ornament, 3D, shadows, Apple logo, trademark symbols,
thin lines, complex detail, background pattern
```

**Tool:** Flux Schnell (fast iteration), export as PNG then trace to SVG in Inkscape.

---

## 2. App Icon (256x256 + ICO multi-res: 16/32/48/64/128/256)

### Concept
Full-size application icon conveying "APFS bridge for Windows" — recognizable,
branded, professional. Must look good in Windows taskbar and file explorer.

---

### Variant A — Gradient Modern

**Prompt (DALL-E 3 / Midjourney):**
```
Modern app icon 256x256, rounded-square canvas, foreground: stylized hard disk
with subtle APFS waveform pattern etched on platter surface, background: deep
space blue to teal gradient #0D1B2A to #1B4F72, small upward-pointing mount arrow
badge bottom-right in amber #F4A261, clean glass morphism reflection, professional
software icon style, no text
```

**Negative prompt:**
```
text, wordmark, Apple logo, Windows flag, photorealistic photo, stock photo,
AI face artifacts, lens flare, more than 3 colors dominant, kitsch, cartoon eyes
```

**Tool:** Midjourney v6.1, upscale 2x, export PNG-32 with alpha.
**Style params:** `--style raw --stylize 200 --ar 1:1 --v 6.1`

---

### Variant B — Isometric

**Prompt (Stable Diffusion XL / Flux Dev):**
```
Isometric icon illustration, hard disk drive rendered isometrically, front face shows
APFS text replaced by abstract waveform glyph, Windows-style frame overlaid
isometrically, flat isometric palette: navy, teal, white highlights, no perspective
distortion, clean edges, icon design 256x256
```

**Negative prompt:**
```
text characters, realistic photo, fisheye, round corners on disk, Apple logo,
more than 4 colors, bokeh, noise, artifacts, floating elements
```

**Tool:** SDXL + isometric LoRA, cfg=8, steps=35.

---

### Variant C — Retro Pixel

**Prompt (Stable Diffusion XL pixel-art model / Aseprite manual):**
```
Retro pixel art app icon 64x64 upscaled cleanly to 256x256, hard disk with
mount-point symbol, 16-color palette restricted to: black, white, 4 blues,
2 teals, 2 ambers, transparent background, crisp pixel-perfect edges, no
anti-aliasing, 8-bit aesthetic
```

**Negative prompt:**
```
blurry upscale, bilinear interpolation, text, logos, more than 16 colors,
gradient, photorealism, soft edges
```

**Tool:** Stable Diffusion XL (pixel-art checkpoint), then nearest-neighbor scale to 256.

---

## 3. GitHub Social Preview (1280x640)

### Concept
Repository card shown on github.com/user/apfs-explorer. Should communicate
project purpose at a glance without reading fine text.

**Prompt (DALL-E 3 / Midjourney):**
```
Wide banner image 1280x640, dark background #0D1B2A, left side: large stylized
hard disk icon in teal and amber, center: bold clean sans-serif placeholder for
"apfs-explorer" (leave blank area for text overlay), right side: abstract APFS
waveform data visualization fading into background, professional open-source
project banner style, no clutter, generous whitespace
```

**Negative prompt:**
```
faces, people, Apple logo, Windows logo, copyright symbols, watermarks,
stock photo style, busy background, small unreadable text, neon colors,
AI artifacts, lens distortion
```

**Tool:** Midjourney v6.1 `--ar 2:1 --style raw --stylize 150`

**Post-process in Figma/Canva:**
- Overlay: "apfs-explorer" in Inter Bold 96pt, white
- Subtitle: "APFS volumes on Windows, zero drivers" in Inter Regular 36pt, teal #A8DADC
- Bottom-right: small "Rust" badge

---

## 4. Installer Banner (493x312 BMP — WiX Welcome Screen)

### Concept
Full splash shown on WiX installer welcome dialog. Subtle, professional, trustworthy.

**Prompt (DALL-E 3):**
```
Installer welcome screen illustration 493x312 pixels, soft dark blue gradient
background, centered subtle hard disk icon with gentle ambient glow, minimal and
professional, enterprise software aesthetic, large blank area on right two-thirds
for text overlay, no text in image, muted palette: deep navy to slate blue
```

**Negative prompt:**
```
text, logos, faces, photorealistic photo, bright neon, comic style, cartoon,
cluttered elements, Apple logo, trademark, watermark, AI artifacts
```

**Tool:** DALL-E 3 (closest to 3:2 ratio crop), export as BMP 24-bit 493x312.

---

## 5. Installer Header (493x58 BMP — WiX Top Banner)

### Concept
Narrow banner at top of installer dialogs. Very constrained — must work at 58px height.

**Prompt (DALL-E 3 / Flux Schnell):**
```
Narrow horizontal banner 493x58 pixels, dark navy gradient left to right,
small disk icon on left side, rest is empty for text, ultra minimal,
professional Windows installer style, no text in image
```

**Negative prompt:**
```
text, complex illustration, faces, logos, gradients going vertical,
more than 2 colors, noise, artifacts
```

**Tool:** DALL-E 3, crop to exact 493x58, export BMP 24-bit.

---

## 6. Demo GIF Storyboard (5-Frame Description)

This is not an AI image gen target — it is a screen-capture storyboard.
Use ShareX or OBS to capture, then compile with gifski or ezgif.

| Frame | Duration | Content | Notes |
|-------|----------|---------|-------|
| 1 | 2 s | System tray — apfs-mount tray icon visible in notification area | Hover tooltip: "apfs-mount running" |
| 2 | 1.5 s | USB drive plugged in, Windows hardware notification balloon | Show Device Manager briefly |
| 3 | 2 s | apfs-mount notification: "APFS volume mounted as Z:" | Toast notification bottom-right |
| 4 | 3 s | File Explorer opens Z: drive — files visible, APFS directory listing | Navigate one folder deep |
| 5 | 2 s | Right-click tray icon → Eject Z: — drive disappears from Explorer | Clean eject confirmation |

**Total:** ~10.5 s loop. Encode at 15 fps, palette=256, lossy=80 with gifski.

---

## Tool Quick Reference

| Asset | Recommended Tool | Fallback |
|-------|-----------------|---------|
| Tray icon (line/glyph) | Flux Schnell + manual SVG trace | DALL-E 3 |
| Tray icon (solid) | Stable Diffusion XL + ControlNet | Midjourney v6.1 |
| App icon | Midjourney v6.1 | DALL-E 3 |
| GitHub preview | Midjourney v6.1 | DALL-E 3 + Figma overlay |
| Installer banner/header | DALL-E 3 | Flux Dev |
| Demo GIF | ShareX + gifski | OBS + ezgif |
