// shelfgauge: the Custom theme's palette, derived from two colors (loaded without
// `defer`, before theme.js).
//
// `shelfgaugePalette.derive(bg, accent)` turns a background and an accent (`#rrggbb`)
// into every custom property a theme block of style.css sets, plus the search
// highlights of reader.css, keeping the contrast limits of the built-in themes:
//
// - text at least 7:1 on the background and the sidebar (aiming at about 12:1);
//   muted text at least 4.5:1 on them and on the hover color; the accent, the error
//   color and each profile color at least 4.5:1 on the background and the sidebar;
// - the text on the accent and on each profile color at least 4.5:1;
// - the text at least 4.5:1 on the selection, the banner and the warning background.
//
// Colors are moved in OKLCH (steps of lightness look alike on every hue); contrast is
// WCAG 2's. A color that cannot meet a limit keeps its hue and changes its lightness
// (and its chroma, to stay in sRGB); `notes` says which of the two given colors was
// changed. Checked by .debug/TESTS/palette_check.mjs; the vectors in
// tests/fixtures/palette/ are rechecked by tests/palette.rs.
"use strict";

const shelfgaugePalette = (() => {
  // ---------- Color math ----------

  function parseHex(hex) {
    const m = /^#([0-9a-f]{6})$/i.exec(String(hex));
    if (!m) {
      return null;
    }
    const n = parseInt(m[1], 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255].map((v) => v / 255);
  }

  function toLinear(c) {
    return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  }

  function fromLinear(c) {
    return c <= 0.0031308 ? c * 12.92 : 1.055 * c ** (1 / 2.4) - 0.055;
  }

  // sRGB (0..1) to OKLCH: [L 0..1, C, H degrees].
  function rgbToOklch(rgb) {
    const [r, g, b] = rgb.map(toLinear);
    const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
    const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
    const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
    const L = 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s;
    const A = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
    const B = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
    const C = Math.hypot(A, B);
    const H = C < 1e-7 ? 0 : ((Math.atan2(B, A) * 180) / Math.PI + 360) % 360;
    return [L, C, H];
  }

  // OKLCH to linear sRGB, unclamped (outside 0..1 when out of gamut).
  function oklchToLinear([L, C, H]) {
    const h = (H * Math.PI) / 180;
    const A = C * Math.cos(h);
    const B = C * Math.sin(h);
    const l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3;
    const m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3;
    const s = (L - 0.0894841775 * A - 1.291485548 * B) ** 3;
    return [
      4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
      -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
      -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
    ];
  }

  function inGamut(lin) {
    return lin.every((v) => v >= -1e-6 && v <= 1 + 1e-6);
  }

  // OKLCH to `#rrggbb`: the chroma is reduced until the color fits sRGB.
  function toHex(lch) {
    const L = Math.min(1, Math.max(0, lch[0]));
    let C = Math.max(0, lch[1]);
    let lin = oklchToLinear([L, C, lch[2]]);
    if (!inGamut(lin)) {
      let lo = 0;
      let hi = C;
      for (let i = 0; i < 16; i += 1) {
        const mid = (lo + hi) / 2;
        if (inGamut(oklchToLinear([L, mid, lch[2]]))) {
          lo = mid;
        } else {
          hi = mid;
        }
      }
      C = lo;
      lin = oklchToLinear([L, C, lch[2]]);
    }
    return (
      "#" +
      lin
        .map((v) => Math.round(fromLinear(Math.min(1, Math.max(0, v))) * 255))
        .map((v) => v.toString(16).padStart(2, "0"))
        .join("")
    );
  }

  // Luminances computed so far: the searches ask for the same colors again and again.
  const LUMINANCE = new Map();

  function luminance(hex) {
    let value = LUMINANCE.get(hex);
    if (value === undefined) {
      const [r, g, b] = parseHex(hex).map(toLinear);
      value = 0.2126 * r + 0.7152 * g + 0.0722 * b;
      if (LUMINANCE.size > 50000) {
        LUMINANCE.clear();
      }
      LUMINANCE.set(hex, value);
    }
    return value;
  }

  function contrast(a, b) {
    const la = luminance(a);
    const lb = luminance(b);
    return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
  }

  function lch(hex) {
    return rgbToOklch(parseHex(hex));
  }

  // The lowest contrast of `hex` against each color of `others`.
  function worst(hex, others) {
    return Math.min(...others.map((o) => contrast(hex, o)));
  }

  // `color` (OKLCH) as `#rrggbb`, its lightness moved towards `towards` (1 lighter,
  // 0 darker) as little as needed for `min` against each of `others` (checked on the
  // rounded color). The end itself when even that fails.
  function reach(color, others, min, towards) {
    const at = (t) => toHex([color[0] + (towards - color[0]) * t, color[1], color[2]]);
    const start = at(0);
    if (worst(start, others) >= min) {
      return start;
    }
    let lo = 0;
    let hi = 1;
    for (let i = 0; i < 18; i += 1) {
      const mid = (lo + hi) / 2;
      if (worst(at(mid), others) >= min) {
        hi = mid;
      } else {
        lo = mid;
      }
    }
    return at(hi);
  }

  // Black or white, whichever has more contrast on `hex`.
  function onColor(hex) {
    return contrast(hex, "#000000") >= contrast(hex, "#ffffff") ? "#000000" : "#ffffff";
  }

  // ---------- The palette ----------

  // Lightness steps from the background towards the text (OKLCH L), tuned on the
  // built-in themes (palette_check.mjs compares them).
  // Their chroma grows with the step (`SURFACE_CHROMA` per unit of lightness).
  const STEPS = {
    light: { sidebar: 0.024, hover: 0.055, cover: 0.068, border: 0.105, input: -0.022 },
    dark: { sidebar: 0.028, hover: 0.072, cover: 0.08, border: 0.12, input: -0.022 },
  };
  const SURFACE_CHROMA = 4;
  // The selection, the banner and the warning background: a lightness step towards
  // the text, a chroma (at most the accent's for the first two) and a hue; the step
  // shrinks while the text on them is under `TINT_TEXT`.
  const TINTS = {
    light: { active: [0.05, 0.035], banner: [0.045, 0.03], warning: [0.028, 0.07, 100] },
    dark: { active: [0.14, 0.085], banner: [0.12, 0.085], warning: [0.12, 0.061, 90] },
  };
  const TINT_TEXT = 6;
  // Text aims at this contrast on the background; muted text at this share of the
  // text's contrast, at most `MUTED_AIM`.
  const TEXT_AIM = { light: 14, dark: 12.5 };
  // A background with less chroma than this has no hue of its own: the text takes
  // the accent's (a trace of it, as Light's grays are a little blue).
  const NEUTRAL = 0.015;
  const MUTED_SHARE = 0.62;
  const MUTED_AIM = 6;
  // The hue and chroma of the profile colors 1-7 (green, purple, orange, red, teal,
  // pink, brown): those of style.css's Light and Dark palettes.
  const PROFILE = {
    light: ["#1a7f37", "#8250df", "#bc4c00", "#cf222e", "#0b7285", "#bf3989", "#8a5a2b"],
    dark: ["#3fb950", "#ab7df8", "#f0883e", "#ff7b72", "#39c5cf", "#f778ba", "#c9a26d"],
  };
  const ERROR = { light: "#cf222e", dark: "#ff7b72" };
  // A profile color nearer than this in hue to a colorful accent turns away from it.
  const HUE_GAP = 20;
  const FIND = {
    light: ["rgb(0 140 255 / 0.22)", "rgb(255 128 0 / 0.5)"],
    dark: ["rgb(80 160 255 / 0.32)", "rgb(255 150 50 / 0.5)"],
  };

  // White text reaches 7:1 (and a hair) on a dark background of this luminance at
  // most, black text on a light one of this luminance at least; the text is white or
  // black when nothing nearer the background's hue reaches its aim.
  const DARK_MAX = 1.05 / 7.05 - 0.05;
  const LIGHT_MIN = 7.05 * 0.05 - 0.05;

  // Whether `hex` is a dark background: white text has more contrast on it.
  function isDark(hex) {
    return contrast(hex, "#ffffff") >= contrast(hex, "#000000");
  }

  // The surface `step` away from background `bg` towards the text (`dir` 1 lighter,
  // -1 darker), its chroma growing with the step.
  function surfaceOf(bg, dir, step) {
    const [L, C, H] = lch(bg);
    return toHex([L + dir * step, C * (1 + SURFACE_CHROMA * Math.abs(step)), H]);
  }

  // The background moved out of the middle band of luminance, its hue kept; the
  // sidebar (`step` towards the text) is held to the same band.
  function background(hex, dark, step) {
    const inBand = (v) => (dark ? luminance(v) <= DARK_MAX : luminance(v) >= LIGHT_MIN);
    const ok = (v) => inBand(v) && inBand(surfaceOf(v, dark ? 1 : -1, step));
    if (ok(hex)) {
      return hex;
    }
    const [L, C, H] = lch(hex);
    // Search the lightness between the color and black (dark) or white (light).
    let lo = 0;
    let hi = 1;
    const at = (t) => toHex([L + ((dark ? 0 : 1) - L) * t, C, H]);
    for (let i = 0; i < 18; i += 1) {
      const mid = (lo + hi) / 2;
      if (ok(at(mid))) {
        hi = mid;
      } else {
        lo = mid;
      }
    }
    return at(hi);
  }

  function hueDistance(a, b) {
    const d = Math.abs(a - b) % 360;
    return d > 180 ? 360 - d : d;
  }

  /**
   * The Custom theme of background `bg` and accent `accent` (`#rrggbb`).
   *
   * Returns `{ vars, scheme, bg, accent, notes }`: `vars` maps each custom property
   * (`--bg`, …, `--find-current-bg`) to its value, `scheme` is `light` or `dark`
   * (for `color-scheme`), `bg` and `accent` the two colors in use, `notes` lists
   * `"background"` and/or `"accent"` when they differ from the given ones. Invalid
   * input is read as the Light theme's colors.
   */
  function derive(bgIn, accentIn) {
    const given = {
      bg: parseHex(bgIn) ? String(bgIn).toLowerCase() : "#ffffff",
      accent: parseHex(accentIn) ? String(accentIn).toLowerCase() : "#0969da",
    };
    const dark = isDark(given.bg);
    const scheme = dark ? "dark" : "light";
    const toText = dark ? 1 : 0;
    const dir = dark ? 1 : -1;
    const steps = STEPS[scheme];
    const bg = background(given.bg, dark, steps.sidebar);
    const [bgL, bgC, bgH] = lch(bg);
    const surface = (step) => surfaceOf(bg, dir, step);
    const sidebar = surface(steps.sidebar);
    const hover = surface(steps.hover);
    // Muted text is read on hovered rows too; links and colors on the page and the
    // sidebar (the built-in themes' limits).
    const panels = [bg, sidebar, hover];
    const pages = [bg, sidebar];

    // Text: the background's hue, nearly gray, about `TEXT_AIM` (or white or black,
    // which the background's band keeps at 7:1 at least).
    const textH = bgC < NEUTRAL ? lch(given.accent)[2] : bgH;
    const fgC = Math.min(bgC * 0.6 + 0.004, 0.026);
    const fg = reach([bgL, fgC, textH], [bg], TEXT_AIM[scheme], toText);
    const mutedAim = Math.min(MUTED_AIM, MUTED_SHARE * contrast(fg, bg));
    const mutedC = Math.min(bgC * 1.2 + 0.008, 0.045);
    let muted = reach([bgL, mutedC, textH], [bg], mutedAim, toText);
    if (worst(muted, panels) < 4.6) {
      muted = reach(lch(muted), panels, 4.6, toText);
    }

    // The accent: its hue and chroma, the lightness moved as little as needed.
    let accent = given.accent;
    if (worst(accent, pages) < 4.5 || worst(accent, [onColor(accent)]) < 4.5) {
      accent = reach(lch(accent), pages, 4.6, toText);
    }
    const onAccent = onColor(accent);

    // The selection, the banner, the warning background (`TINTS`).
    const [, accentC, accentH] = lch(accent);
    const tinted = ([step, chroma, hue]) => {
      const at = (d) => toHex([bgL + dir * d, chroma, hue]);
      if (contrast(fg, at(step)) >= TINT_TEXT) {
        return at(step);
      }
      let lo = 0;
      let hi = step;
      for (let i = 0; i < 12; i += 1) {
        const mid = (lo + hi) / 2;
        if (contrast(fg, at(mid)) >= TINT_TEXT) {
          lo = mid;
        } else {
          hi = mid;
        }
      }
      return at(lo);
    };
    const tints = TINTS[scheme];
    const accentTint = ([step, chroma]) => tinted([step, Math.min(chroma, accentC), accentH]);

    const vars = {
      "--bg": bg,
      "--fg": fg,
      "--muted": muted,
      "--border": surface(steps.border),
      "--sidebar-bg": sidebar,
      "--hover": hover,
      "--active-bg": accentTint(tints.active),
      "--accent": accent,
      "--on-accent": onAccent,
      "--banner-bg": accentTint(tints.banner),
      "--error": reach(lch(ERROR[scheme]), pages, 4.6, toText),
      "--warning-bg": tinted(tints.warning),
      "--input-bg": surface(steps.input),
      "--cover-bg": surface(steps.cover),
      "--pcolor-0": accent,
      "--on-pcolor-0": onAccent,
    };
    PROFILE[scheme].forEach((base, i) => {
      let [L, C, H] = lch(base);
      const gap = hueDistance(H, accentH);
      if (accentC > 0.04 && gap < HUE_GAP) {
        const sign = ((H - accentH + 540) % 360) - 180 >= 0 ? 1 : -1;
        H = (accentH + sign * HUE_GAP + 360) % 360;
      }
      let color = reach([L, C, H], pages, 4.6, toText);
      if (worst(color, [onColor(color)]) < 4.5) {
        color = reach(lch(color), pages, 4.6, toText);
      }
      vars[`--pcolor-${i + 1}`] = color;
      vars[`--on-pcolor-${i + 1}`] = onColor(color);
    });
    vars["--find-bg"] = FIND[scheme][0];
    vars["--find-current-bg"] = FIND[scheme][1];

    const notes = [];
    if (bg !== given.bg) {
      notes.push("background");
    }
    if (accent !== given.accent) {
      notes.push("accent");
    }
    return { vars, scheme, bg, accent, notes };
  }

  return { derive, contrast, luminance, isDark };
})();

if (typeof module !== "undefined") {
  module.exports = shelfgaugePalette;
}
