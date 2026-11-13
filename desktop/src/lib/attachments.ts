/** Read-only attachment previews. Nothing here stores image bytes: a preview
 * lives in memory as a Blob URL and dies with the component that made it. */

const PNG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/** The MIME type of bytes the backend already verified. Deriving it from the
 * magic keeps the Blob honest even if the file name lies. */
export function imageMime(bytes: Uint8Array): string | null {
  const startsWith = (magic: number[]) =>
    bytes.length >= magic.length && magic.every((byte, index) => bytes[index] === byte);
  if (startsWith(PNG)) return 'image/png';
  if (startsWith([0xff, 0xd8, 0xff])) return 'image/jpeg';
  if (startsWith([0x52, 0x49, 0x46, 0x46])
    && bytes[8] === 0x57 && bytes[9] === 0x45 && bytes[10] === 0x42 && bytes[11] === 0x50) {
    return 'image/webp';
  }
  return null;
}

/** The file name alone, for a chip that must not show a private path. */
export function attachmentName(path: string): string {
  return path.slice(Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\')) + 1);
}

const IMAGE_EXTENSION = /\.(png|jpe?g|webp)$/i;

/** Whether a draft chip is worth a thumbnail at all. The backend still checks
 * the real bytes, so a wrong extension only degrades to the file icon. */
export function looksLikeImage(path: string): boolean {
  return IMAGE_EXTENSION.test(attachmentName(path));
}

/** A missing `preview_image` command means an older Desktop binary, not a
 * broken picture, and it must say so instead of echoing an IPC error. */
export function previewErrorMessage(failure: unknown): string {
  const message = failure instanceof Error ? failure.message : String(failure);
  if (/preview_image|not found|not allowed|not permitted|unknown command/i.test(message)) {
    return '图片预览暂不可用：需要更新并重启 Desktop 后再试';
  }
  return message;
}

/** Why a restored draft attachment is not usable any more. */
export function attachmentStateMessage(state: string): string {
  switch (state) {
    case 'missing': return '文件已不存在';
    case 'not_file': return '不再是普通文件';
    case 'too_large': return '文件超过 32 MiB';
    case 'image_too_large': return '图片超过 8 MiB';
    default: return '文件不可用';
  }
}

/** A suggested save-as name. Only the label is used: the native command reads
 * the stored file itself and never trusts a name as a directory. */
export function suggestedAttachmentName(name: string, path: string): string {
  const base = (name || attachmentName(path)).trim();
  const cleaned = [...base]
    .map(character => (character === '\\' || '<>:"/|?*'.includes(character)
      || character.charCodeAt(0) < 32 ? '_' : character))
    .join('')
    .replace(/^\.+/, '')
    .slice(0, 80);
  return cleaned || 'attachment';
}
