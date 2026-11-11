import {
  isPermissionGranted, requestPermission, sendNotification,
} from '@tauri-apps/plugin-notification';
import { getCurrentWindow } from '@tauri-apps/api/window';
import type { Message } from './api';
import { taskNotification } from './notify';

/** Desktop-only duty: a long task must reach the user when they are looking at
 * another window. Never notify while the app has focus. */
export async function announceTaskResult(options: {
  status: string;
  sessionName: string;
  messages: Message[];
}): Promise<void> {
  const payload = taskNotification(options.status, options.sessionName, options.messages);
  if (!payload) return;
  let focused = true;
  try { focused = await getCurrentWindow().isFocused(); }
  catch { /* assume focused */ }
  if (focused) return;
  if (!(await ensureNotificationPermission())) return;
  try { sendNotification(payload); }
  catch { /* notifications are best-effort */ }
}

/** Ask once, per delivered notification, so an old macOS build can still run. */
export async function ensureNotificationPermission(): Promise<boolean> {
  try {
    if (await isPermissionGranted()) return true;
    return (await requestPermission()) === 'granted';
  } catch { return false; }
}
