import { describe, expect, it } from 'vitest';
import {
  BACKUP_IMPORT_WARNING, BACKUP_WARNING, backupFileStem, backupReportMessage, importReportMessage,
} from './session-backup';

describe('session backup wording', () => {
  it('suggests a .kroma-backup name from the session title', () => {
    expect(backupFileStem('修复登录问题')).toBe('修复登录问题.kroma-backup');
    expect(backupFileStem('a/b:c')).toBe('a_b_c.kroma-backup');
    expect(backupFileStem('')).toBe('Kroma 会话.kroma-backup');
  });

  it('states that a backup may contain sensitive content', () => {
    expect(BACKUP_WARNING).toContain('可能含敏感内容');
    expect(BACKUP_WARNING).toContain('不会额外打包保存的 API 密钥');
    expect(BACKUP_WARNING).toContain('对话内容中已有的敏感信息');
    expect(BACKUP_WARNING).toContain('128 MiB');
    expect(BACKUP_IMPORT_WARNING).toContain('不可信');
    expect(BACKUP_IMPORT_WARNING).toContain('新会话');
    expect(BACKUP_IMPORT_WARNING).toContain('不会自动发送');
  });

  it('reports what a backup actually contains, including omissions', () => {
    expect(backupReportMessage({title:'t', messages:83, attachments:2, external_references:1}))
      .toContain('2 个附件');
    expect(backupReportMessage({title:'t', messages:83, attachments:2, external_references:1}))
      .toContain('未包含');
    expect(backupReportMessage({title:'t', messages:10, attachments:0, external_references:0}))
      .not.toContain('未包含');
    expect(importReportMessage({id:'s1', title:'标题', messages:20, attachments:1}))
      .toContain('已导入为新会话');
  });
});
