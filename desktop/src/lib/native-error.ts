/** A native binary that predates a new command answers with an IPC "command
 * not found" error. The window says what to do instead of echoing that. */
export function missingNativeCommand(failure: unknown): boolean {
  const message = failure instanceof Error ? failure.message : String(failure);
  return /command .*not found|unknown command|not allowed by ACL/i.test(message);
}

export function desktopCommandMessage(feature: string, failure: unknown): string {
  if (missingNativeCommand(failure)) {
    return `${feature}需要更新并重启 Desktop 后才能使用`;
  }
  return failure instanceof Error ? failure.message : String(failure);
}
