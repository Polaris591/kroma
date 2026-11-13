/// Profile names are also folder-safe config names. `-` must be escaped for
/// browsers that compile `pattern` with the `v` flag.
export const PROFILE_NAME_PATTERN = String.raw`[A-Za-z0-9][A-Za-z0-9_.\-]{0,47}`;
