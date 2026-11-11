/** Desktop navigation tabs. Shared by the shell and the panels it renders. */
export type Tab = 'chat' | 'settings' | 'plugins' | `plugin:${string}`;

/** A session addressed by project + id, with its display title for dialogs. */
export interface SessionTarget {
  project: string;
  id: string;
  title: string;
}

/** A pending cross-project navigation request. */
export interface SessionDestination {
  project: string;
  id: string;
}

export type TaskResultStatus = 'completed' | 'cancelled' | 'failed' | '';
