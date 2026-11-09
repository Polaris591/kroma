/** Sources at the end of a report line are written in three shapes by the
 * model, and all of them should read as one quiet chip:
 *   …本轮尚未关闭。（TechCrunch）
 *   …路演预计下周启动。(<a href="…">WSJ、彭博</a>)
 *   …防控链条断裂。(<a href="…">路透</a>)
 * Only a trailing parenthesis that reads like an outlet qualifies; prose,
 * figures and dates stay as plain text.
 */
const TRAILING_SOURCE = /\s*[（(]\s*([^（）()]{1,40}?)\s*[）)]\s*$/;
const CLOSING_ONLY = /^\s*[）)]\s*$/;
const OPENING_TAIL = /[（(]\s*$/;
const NESTED_BLOCK = 'li, p, ul, ol, table, pre, blockquote';
const INLINE_SIBLING = /^(A|STRONG|EM|B|I|SPAN|CODE|SMALL)$/;
const MIN_BODY = 10;
const MAX_SIBLING_WALK = 8;

export function looksLikeSource(source: string): boolean {
  return !/[，。；：！？|%$¥€—…]/.test(source)
    && !/^[0-9]/.test(source)
    && !/^(周[一二三四五六日]|星期[一二三四五六日]|详见|见|注|略)/.test(source);
}

/** Text-only form: split "…正文。（来源）" into its body and source. */
export function splitTrailingSource(text: string): { body: string; source: string } | null {
  const match = text.match(TRAILING_SOURCE);
  if (!match) return null;
  const source = match[1].trim();
  if (!looksLikeSource(source)) return null;
  return { body: text.slice(0, match.index).replace(/\s+$/, ''), source };
}

function lastTextNode(element: Element): Text | null {
  let last: Text | null = null;
  const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (node.nodeValue?.trim()) last = node as Text;
  }
  return last;
}

function chip(content: Node | string): HTMLElement {
  const element = document.createElement('span');
  element.className = 'source-chip';
  if (typeof content === 'string') element.textContent = content;
  else element.appendChild(content);
  return element;
}

/** Walk back from the closing paren to the inline nodes that form the source. */
function sourceNodes(closing: Text): { opener: Text; nodes: Node[] } | null {
  const nodes: Node[] = [];
  let steps = 0;
  for (let node = closing.previousSibling; node && steps < MAX_SIBLING_WALK; node = node.previousSibling) {
    steps += 1;
    if (node.nodeType === Node.TEXT_NODE) {
      const text = node.nodeValue ?? '';
      if (OPENING_TAIL.test(text)) return { opener: node as Text, nodes };
      // Keep the separator that joins two outlets ("CNN、泰晤士报") inside the
      // chip; whitespace between nodes is not meaningful.
      if (!text.trim()) continue;
      if (text.trim().length > 2) return null;
      nodes.unshift(node);
      continue;
    }
    if (node.nodeType === Node.ELEMENT_NODE && INLINE_SIBLING.test((node as Element).tagName)) {
      nodes.unshift(node);
      continue;
    }
    return null;
  }
  return null;
}

/** Rewrites trailing parenthetical sources into chips. Runs on sanitized
 * HTML before it reaches the DOM, so no model-authored markup is created. */
export function decorateCitations(root: ParentNode): void {
  for (const element of Array.from(root.querySelectorAll('li, p, td'))) {
    if (element.querySelector(NESTED_BLOCK)) continue;
    const last = lastTextNode(element);
    if (!last?.nodeValue) continue;
    const value = last.nodeValue;

    const split = splitTrailingSource(value);
    if (split) {
      if (element.textContent!.trim().length - (value.length - split.body.length) < MIN_BODY) continue;
      last.nodeValue = split.body;
      last.parentNode?.insertBefore(chip(split.source), last.nextSibling);
      continue;
    }

    if (!CLOSING_ONLY.test(value)) continue;
    const grouped = sourceNodes(last);
    if (!grouped) continue;
    const source = grouped.nodes.map(node => node.textContent ?? '').join('').trim();
    if (!source || !looksLikeSource(source)) continue;
    if (element.textContent!.trim().length - source.length - 2 < MIN_BODY) continue;
    const wrapper = document.createElement('span');
    wrapper.className = 'source-chip';
    for (const node of grouped.nodes) wrapper.appendChild(node);
    grouped.opener.nodeValue = grouped.opener.nodeValue!.replace(OPENING_TAIL, '').replace(/\s+$/, '');
    grouped.opener.parentNode?.insertBefore(wrapper, last);
    last.parentNode?.removeChild(last);
  }
}
