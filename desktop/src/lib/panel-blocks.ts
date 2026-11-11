// A plugin panel may return structured blocks. Rendering is generic: a block
// is styled by its type, never by the plugin that sent it.
export type PanelBlock =
  | {type: 'heading'; text: string}
  | {type: 'text'; text: string}
  | {type: 'list'; items: string[]}
  | {type: 'keyvalue'; rows: {key: string; value: string}[]}
  | {type: 'progress'; label: string; value: number; max: number}
  | {type: 'table'; headers: string[]; rows: string[][]}
  | {type: 'divider'};

export interface PanelView {
  title: string;
  blocks: PanelBlock[];
}

/// Bar width in percent. A missing or zero maximum, a negative value or an
/// overflowing one still leaves a usable bar.
export function progressPercent(value: number, max: number): number {
  if (!Number.isFinite(value) || !Number.isFinite(max) || max <= 0) return 0;
  return Math.min(100, Math.max(0, Math.round((value / max) * 100)));
}

/// The columns a table needs: whichever is wider, its headers or its longest
/// row, so a short row never shifts the cells of the rows below it.
export function tableWidth(headers: string[], rows: string[][]): number {
  return rows.reduce((width, row) => Math.max(width, row.length), headers.length);
}

/// Headers and rows padded to that width; a table scrolls sideways rather than
/// dropping a column a plugin did send.
export function tableGrid(headers: string[], rows: string[][]):
  {headers: string[]; rows: string[][]} {
  const width = tableWidth(headers, rows);
  const pad = (cells: string[]) =>
    Array.from({length: width}, (_, index) => cells[index] ?? '');
  return {headers: pad(headers), rows: rows.map(pad)};
}
