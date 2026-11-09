import {describe, expect, it} from 'vitest';
import {createRequestTokens} from './latest';

describe('newest request wins', () => {
  it('refuses a response that a newer request superseded', () => {
    const tokens = createRequestTokens();
    const first = tokens.begin();
    expect(tokens.isCurrent(first)).toBe(true);
    const second = tokens.begin();
    expect(tokens.isCurrent(first)).toBe(false);
    expect(tokens.isCurrent(second)).toBe(true);
    expect(tokens.isCurrent(second + 1)).toBe(false);
  });

  it('keeps the newest request current until another one starts', () => {
    const tokens = createRequestTokens();
    const only = tokens.begin();
    for (let round = 0; round < 5; round++) expect(tokens.isCurrent(only)).toBe(true);
    expect(tokens.begin()).toBeGreaterThan(only);
  });
});
