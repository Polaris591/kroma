/** Presentation for the local session backup: file names, honest limits and
 * the wording that keeps a backup distinct from a Markdown export. */
import { exportFileStem } from './session-export';

/** The backup is not a Markdown transcript: it is the only form that can be
 * restored, and it therefore carries everything the transcript contains. */
export const BACKUP_WARNING =
  '备份包含完整对话、系统提示词、模型思考与工具记录，可能含敏感内容，请妥善保管。'
  + '不会额外打包保存的 API 密钥、模型/插件配置；对话内容中已有的敏感信息（例如命令输出中出现过的密钥）仍会包含。'
  + '不包含工作区文件与工具私密日志；单文件上限 128 MiB，单个附件上限 32 MiB（图片 8 MiB）。';
export const BACKUP_IMPORT_WARNING =
  '备份可能来自不可信的来源。Kroma 会先校验版本、对话与上下文一致性、附件映射与文件大小，'
  + '校验失败不会创建半个会话；导入只会恢复为当前工作区中的新会话（新的会话 ID），'
  + '不会覆盖已有会话，不会自动发送消息，也不会重放工具调用。';

export function backupFileStem(title: string): string {
  return exportFileStem(title).replace(/\.md$/i, '.kroma-backup');
}

export interface BackupReport {
  title: string;
  messages: number;
  attachments: number;
  external_references: number;
}

export function backupReportMessage(report: BackupReport): string {
  const external = report.external_references
    ? `；${report.external_references} 个外部文件引用（如工具日志）未包含`
    : '';
  return `已备份 ${report.messages} 条记录、${report.attachments} 个附件${external}`;
}

export interface ImportedSummary {
  id: string;
  title: string;
  messages: number;
  attachments: number;
}

export function importReportMessage(imported: ImportedSummary): string {
  return `已导入为新会话「${imported.title || '未命名'}」：${imported.messages} 条记录、${imported.attachments} 个附件`;
}
