import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { contrastRatio, hexLuminance, readableInk, themeDefaults } from './theme';

describe('theme colour roles', () => {
  it('keeps native startup canvas in sync with the frontend default', () => {
    const css = readFileSync(new URL('../tokens.css', import.meta.url), 'utf8');
    const background = css.match(/--bg:\s*(#[\da-f]{6})\s*;/i)?.[1];
    const config = JSON.parse(readFileSync(new URL('../../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
    expect(background).toBeDefined();
    expect(config.app.windows.find((window: {label: string}) => window.label === 'main').backgroundColor).toBe(background);
  });

  it('calculates WCAG luminance for both permitted hex formats', () => {
    expect(hexLuminance('#000')).toBe(0);
    expect(hexLuminance('#fff')).toBe(1);
    expect(hexLuminance(' #aBc ')).toBe(hexLuminance('#aabbcc'));
    expect(hexLuminance('#ff0000')).toBeCloseTo(.2126);
    expect(hexLuminance('#00ff00')).toBeCloseTo(.7152);
    expect(hexLuminance('#0000ff')).toBeCloseTo(.0722);
  });

  it('never accepts CSS expressions or malformed colours', () => {
    for (const value of ['red', 'abc', '#ab', '#abcd', '#12345678', '#ggg', 'var(--accent)', '#fff;background:red']) {
      expect(hexLuminance(value)).toBeNull();
      expect(contrastRatio(value, '#fff')).toBeNull();
    }
  });

  it('uses symmetric contrast ratios with the expected endpoints', () => {
    expect(contrastRatio('#000', '#fff')).toBe(21);
    expect(contrastRatio('#fff', '#000')).toBe(21);
    expect(contrastRatio('#abc', '#abc')).toBe(1);
  });

  it('keeps accent labels legible across dark, light and middle-grey colours', () => {
    for (let value = 0; value <= 255; value += 1) {
      const grey = '#' + value.toString(16).padStart(2, '0').repeat(3);
      expect(contrastRatio(grey, readableInk(grey))).toBeGreaterThanOrEqual(4.5);
    }
    for (const accent of ['#b8ace8', '#6850a4', '#262060', '#f4c66b', '#7c647a', '#8fc79a']) {
      expect(contrastRatio(accent, readableInk(accent))).toBeGreaterThanOrEqual(4.5);
    }
  });

  it('adapts native controls and readable fallback text to a light background', () => {
    const light = themeDefaults('#f6f5f2');
    expect(light.scheme).toBe('light');
    expect(contrastRatio('#f6f5f2', light.text)).toBeGreaterThanOrEqual(4.5);
    expect(contrastRatio('#f6f5f2', light.muted)).toBeGreaterThanOrEqual(4.5);
    expect(themeDefaults('#19191e').scheme).toBe('dark');
  });

  it('does not sacrifice legibility to softer grey labels in a mid-tone theme', () => {
    const grey = themeDefaults('#777');
    expect(contrastRatio('#777', grey.text)).toBeGreaterThanOrEqual(4.5);
    expect(contrastRatio('#777', grey.muted)).toBeGreaterThanOrEqual(4.5);
  });
});
