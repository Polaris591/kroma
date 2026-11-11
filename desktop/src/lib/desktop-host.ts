import type { Bootstrap, SessionSnapshot, TurnDiffSummary } from './api';
import type { OperationGuard } from './snapshot';
import type { Tab } from './ui-types';

export interface RewindTarget {
  messageIndex: number;
  snapshotTurn: number | null;
  files: number;
  added: number;
  removed: number;
}

/** The shell owns all rendered state; command modules mutate it through these
 * synchronous accessors so Svelte's reactivity stays in the component. */
export interface DesktopHost {
  get data(): Bootstrap | null;
  set data(value: Bootstrap | null);
  get snapshot(): SessionSnapshot | null;
  get tab(): Tab;
  set tab(value: Tab);
  get busy(): boolean;
  set busy(value: boolean);
  get running(): boolean;
  get initialized(): boolean;
  set initialized(value: boolean);
  get compacting(): boolean;
  set compacting(value: boolean);
  get rewinding(): boolean;
  set rewinding(value: boolean);
  get rewindTarget(): RewindTarget | null;
  set rewindTarget(value: RewindTarget | null);
  get connectedProfile(): string;
  get reasoningLevel(): string;
  get sessionName(): string;
  get taskGeneration(): number;
  set taskGeneration(value: number);
  get mcpDismissed(): boolean;
  set mcpDismissed(value: boolean);
  readonly guard: OperationGuard;
  clearError(): void;
  clearNotice(): void;
  setRunning(value: boolean): void;
  resetTaskResult(): void;
  clearLiveActivities(): void;
  saveDraft(): void;
  resetHistoryWindow(): void;
  collapseSidebarOnNarrow(): void;
  endRename(): void;
  clearDeletedSessions(): void;
  clearWorkspaceReview(): void;
  expandProject(project: string): void;
  showError(failure: unknown): void;
  toast(message: string): void;
  syncHistory(): void;
  restoreDraft(): void;
  restoreFollowups(): void;
  refreshReview(): void;
  scrollDown(): Promise<void>;
  scrollIfBottom(): Promise<void>;
  focusComposer(): void;
  setComposerDraft(text: string): void;
}

/** Ask for a rewind target; only the numbers a dialog needs are kept. */
export function rewindTargetFor(
  messageIndex: number, snapshotTurn: number | null, diff: TurnDiffSummary | null,
): RewindTarget {
  return {
    messageIndex,
    snapshotTurn,
    files: diff?.files ?? 0,
    added: diff?.added ?? 0,
    removed: diff?.removed ?? 0,
  };
}
