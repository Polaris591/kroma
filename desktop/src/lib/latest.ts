/** Token stream for "newest request wins". A caller takes a token and may only
 * apply (or display) its response while it is still the newest one, so a slow
 * reply can never overwrite fresher state. */
export interface RequestTokens {
  begin(): number;
  isCurrent(token: number): boolean;
}

export function createRequestTokens(): RequestTokens {
  let current = 0;
  return {
    begin: () => ++current,
    isCurrent: token => token === current,
  };
}
