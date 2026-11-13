import type { SessionEntry } from './api';

/** Give the actual OS home folder a readable label, while preserving
 * the real project path for every action and title tooltip. Other folders
 * keep their basename (including folders owned by other users).
 */
export function workspaceDisplayName(project: string, userHome?: string | null): string {
  const normalized = (path: string) =>
    path.replace(/\\/g, '/').replace(/\/$/, '').toLocaleLowerCase();
  if (userHome && normalized(project) === normalized(userHome)) return '用户主目录';
  return project.split(/[\\/]/).filter(Boolean).pop() || project;
}

/** Two workspaces can share a last path segment; then the parent disambiguates. */
export function workspaceLabels(projects: string[], userHome?: string | null): Record<string, string> {
  const labels: Record<string, string> = {};
  const counts = new Map<string, number>();
  for (const project of projects) {
    const base = workspaceDisplayName(project, userHome);
    counts.set(base, (counts.get(base) ?? 0) + 1);
  }
  for (const project of projects) {
    const base = workspaceDisplayName(project, userHome);
    if ((counts.get(base) ?? 0) < 2) {
      labels[project] = base;
      continue;
    }
    const parts = project.split(/[\\/]/).filter(Boolean);
    const parent = parts[parts.length - 2];
    labels[project] = parent ? `${parent}/${base}` : base;
  }
  return labels;
}

/** Display task names, not internal journal IDs, in the navigation. */export function sessionLabel(session: SessionEntry): string {
  const name = session.name?.trim();
  if (name) return name;
  if (session.id === 'default') return '默认会话';
  return '新对话';
}

const WEEKDAYS = '日一二三四五六';

/** Small local-time cue for untitled legacy sessions. This represents
 * last activity, not creation time, and never changes the stored title.
 * Recent days read as words so a column of dates does not look like noise.
 */
export function sessionActivityLabel(timestamp: number | null | undefined, now = new Date()): string {
  if (timestamp == null || !Number.isFinite(timestamp) || timestamp < 0) return '';
  const date = new Date(timestamp * 1000);
  if (!Number.isFinite(date.getTime())) return '';
  const dayStart = (value: Date) =>
    new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();
  const days = Math.round((dayStart(now) - dayStart(date)) / 86_400_000);
  if (days === 0) {
    return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
  }
  if (days === 1) return '昨天';
  if (days > 1 && days < 7) return `周${WEEKDAYS[date.getDay()]}`;
  if (date.getFullYear() === now.getFullYear()) {
    return `${date.getMonth() + 1}月${date.getDate()}日`;
  }
  return `${date.getFullYear()}/${date.getMonth() + 1}/${date.getDate()}`;
}

/** Filter only visible session metadata; no transcript content is indexed. */
export function filterSessions(sessions: SessionEntry[], query: string): SessionEntry[] {
  const normalized = query.trim().toLocaleLowerCase();
  if (!normalized) return sessions;
  return sessions.filter(session =>
    (session.name || '').toLocaleLowerCase().includes(normalized)
    || session.id.toLocaleLowerCase().includes(normalized)
  );
}

/** Keep the navigation concise without hiding a selected older session.
 * Search results always show all matches; unfiltered groups stay short.
 */
export function visibleSessionsForGroup(
  sessions: SessionEntry[],
  query: string,
  limit: number,
  selectedId: string | undefined,
): SessionEntry[] {
  const matching = filterSessions(sessions, query);
  if (query.trim()) return matching;
  const initial = matching.slice(0, Math.max(0, limit));
  if (selectedId && !initial.some(s => s.id === selectedId)) {
    const selected = matching.find(s => s.id === selectedId);
    if (selected) initial.unshift(selected);
  }
  return initial;
}

/** Resolve a workspace's accordion state. An explicit user choice always
 * overrides the default (current workspace) and the search expansion.
 * Pass the state map into the Svelte template rather than hiding this
 * dependency inside an untracked component helper.
 */
export function isWorkspaceExpanded(
  overrides: Readonly<Record<string, boolean>>,
  project: string,
  currentProject?: string,
  defaultProject?: string | null,
  search = '',
): boolean {
  return overrides[project] ?? (
    project === currentProject
    || project === defaultProject
    || search.trim().length > 0
  );
}

export function sidebarIsVisible(
  narrow: boolean,
  choice: boolean | null,
  declaredDefault: 'visible' | 'hidden' | null,
): boolean {
  return choice ?? (declaredDefault ? declaredDefault === 'visible' : !narrow);
}

/** Serialize navigation through the single active Agent. While an IPC call
 * is in flight, keep only the latest requested destination rather than
 * silently ignoring additional clicks or racing multiple session locks.
 * Completion/error callbacks belong to the caller (UI state is not stored
 * in this utility). */
export function createLatestNavigation<T>(
  perform: (destination: T) => Promise<void>,
  settled: () => void,
  failed: (error: unknown) => void,
) {
  let queued: T | null = null;
  let active = false;

  async function drain() {
    if (active) return;
    active = true;
    try {
      while (queued !== null) {
        const destination = queued;
        queued = null;
        try {
          await perform(destination);
        } catch (error) {
          failed(error);
        }
      }
    } finally {
      active = false;
      settled();
    }
  }

  return {
    request(destination: T) {
      queued = destination;
      void drain();
    },
    get isActive() { return active; },
    get hasPending() { return queued !== null; },
  };
}

/** Keep the same line in view after older history is prepended above it. */
export function scrollTopAfterPrepend(top: number, previousHeight: number, nextHeight: number): number {
  return Math.max(0, top + Math.max(0, nextHeight - previousHeight));
}
