/** Composer "@file" references. The token is resolved from the caret, never
 * from a global search, so a mid-word `@` or an email address is left alone. */
export interface FileToken {
  start: number;
  query: string;
}

export function atTokenRange(value: string, caret: number): FileToken | null {
  const before = value.slice(0, caret);
  const at = before.lastIndexOf('@');
  if (at < 0) return null;
  if (at > 0 && before[at - 1].trim() !== '') return null;
  const query = before.slice(at + 1);
  if (query.length > 60) return null;
  for (const character of query) {
    if (character.trim() === '') return null;
  }
  return { start: at, query };
}

/** Replace the token under the caret with a project path. */
export function insertFileReference(
  value: string, caret: number, path: string,
): { value: string; caret: number } | null {
  const token = atTokenRange(value, caret);
  if (!token) return null;
  const inserted = '@' + path + ' ';
  return {
    value: value.slice(0, token.start) + inserted + value.slice(caret),
    caret: token.start + inserted.length,
  };
}
