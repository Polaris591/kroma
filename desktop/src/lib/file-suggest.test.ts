import { describe, expect, it } from 'vitest';
import { atTokenRange, insertFileReference } from './file-suggest';

describe('atTokenRange', () => {
  it('finds the token under the caret', () => {
    expect(atTokenRange('see @src/li', 11)).toEqual({ start: 4, query: 'src/li' });
  });

  it('ignores a mid-word or email @', () => {
    expect(atTokenRange('mail me@host', 13)).toBeNull();
    expect(atTokenRange('foo@bar', 7)).toBeNull();
  });

  it('rejects whitespace and over-long queries', () => {
    expect(atTokenRange('@src lib', 8)).toBeNull();
    expect(atTokenRange('@' + 'a'.repeat(61), 62)).toBeNull();
  });

  it('returns an empty query right after the @', () => {
    expect(atTokenRange('look @', 6)).toEqual({ start: 5, query: '' });
  });
});

describe('insertFileReference', () => {
  it('replaces the token and reports the caret after the inserted path', () => {
    const result = insertFileReference('open @sr now', 8, 'src/lib/api.ts');
    expect(result).toEqual({ value: 'open @src/lib/api.ts  now', caret: 21 });
  });

  it('returns null when the caret is not in a reference', () => {
    expect(insertFileReference('plain text', 10, 'src/a.ts')).toBeNull();
  });

  it('keeps trailing text after the caret', () => {
    expect(insertFileReference('a @b c', 4, 'x.ts')).toEqual({ value: 'a @x.ts  c', caret: 8 });
  });
});
