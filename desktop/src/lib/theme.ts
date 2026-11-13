/** Theme colours are deliberately plain hex, matching the plugin contract. */
export function hexLuminance(value: string): number | null {
  const hex = value.trim().replace(/^#/, '');
  if (!value.trim().startsWith('#') || !/^([\da-f]{3}|[\da-f]{6})$/i.test(hex)) return null;
  const full = hex.length === 3 ? [...hex].map(part => part + part).join('') : hex;
  const rgb = [0, 2, 4].map(offset => {
    const channel = parseInt(full.slice(offset, offset + 2), 16) / 255;
    return channel <= .04045 ? channel / 12.92 : ((channel + .055) / 1.055) ** 2.4;
  });
  return rgb[0] * .2126 + rgb[1] * .7152 + rgb[2] * .0722;
}

export function contrastRatio(first: string, second: string): number | null {
  const a = hexLuminance(first), b = hexLuminance(second);
  if (a === null || b === null) return null;
  return (Math.max(a, b) + .05) / (Math.min(a, b) + .05);
}

/** Choose a readable label even if a plugin supplies a very dark accent.
 * Near middle grey, neither softened ink nor white is sufficient: use black. */
export function readableInk(background: string): string {
  if ((contrastRatio(background, '#211b32') ?? 0) >= 4.5) return '#211b32';
  if ((contrastRatio(background, '#ffffff') ?? 0) >= 4.5) return '#ffffff';
  return '#000000';
}

export function themeDefaults(background: string) {
  const light = (hexLuminance(background) ?? 0) > .179;
  const text = readableInk(background);
  const softText = light ? '#706977' : '#aaa7b4';
  return {
    scheme: light ? 'light' : 'dark',
    text,
    muted: (contrastRatio(background, softText) ?? 0) >= 4.5 ? softText : text,
    line: light ? '#2b21312b' : '#aaa4bc30',
  };
}
