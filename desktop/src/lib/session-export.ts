const BACKSLASH = String.fromCharCode(92);

/** Characters that are invalid in a file name on Windows, macOS or Linux. */
function isHostileFileNameChar(character: string): boolean {
  return character === BACKSLASH
    || '<>:"/|?*'.includes(character)
    || character.charCodeAt(0) < 32;
}

/** Suggested file name for a Markdown export. Only characters that are
 * invalid on Windows/macOS/Linux are replaced; the title stays readable. */
export function exportFileStem(title: string): string {
  const name = [...title]
    .map(character => (isHostileFileNameChar(character) ? '_' : character))
    .join('')
    .slice(0, 56);
  return (name || 'Kroma 会话') + '.md';
}
