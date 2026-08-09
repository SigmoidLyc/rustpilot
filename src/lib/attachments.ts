import type { AttachmentInput } from "./types";

export const MAX_ATTACHMENTS = 8;
export const MAX_ATTACHMENT_BYTES = 25 * 1024 * 1024;
export const MAX_TOTAL_ATTACHMENT_BYTES = 50 * 1024 * 1024;

const MIME_BY_EXTENSION: Record<string, string> = {
  bmp: "image/bmp",
  csv: "text/csv",
  gif: "image/gif",
  htm: "text/html",
  html: "text/html",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
  json: "application/json",
  md: "text/markdown",
  pdf: "application/pdf",
  png: "image/png",
  svg: "image/svg+xml",
  toml: "application/toml",
  txt: "text/plain",
  webp: "image/webp",
  xml: "application/xml",
  yaml: "application/yaml",
  yml: "application/yaml"
};

export function mimeForName(name: string): string {
  const extension = name.split(/[.\\/]/).at(-1)?.toLowerCase() ?? "";
  return MIME_BY_EXTENSION[extension] ?? "application/octet-stream";
}

export function fileMime(file: File): string {
  if (file.type.trim()) return file.type.split(";", 1)[0].trim().toLowerCase();
  return mimeForName(file.name);
}

export async function fileToBase64(file: File): Promise<string> {
  const dataUrl = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      if (typeof reader.result !== "string") {
        reject(new Error(`Unable to read ${file.name}.`));
        return;
      }
      resolve(reader.result);
    };
    reader.onerror = () => reject(reader.error ?? new Error(`Unable to read ${file.name}.`));
    reader.onabort = () => reject(new Error(`Reading ${file.name} was cancelled.`));
    reader.readAsDataURL(file);
  });
  const separator = dataUrl.indexOf(",");
  if (separator < 0) throw new Error(`Unable to encode ${file.name}.`);
  return dataUrl.slice(separator + 1);
}

export async function serializeFiles(files: File[]): Promise<AttachmentInput[]> {
  if (files.length > MAX_ATTACHMENTS) {
    throw new Error(`You can attach at most ${MAX_ATTACHMENTS} files to one message.`);
  }
  const totalSize = files.reduce((total, file) => total + file.size, 0);
  const oversized = files.find((file) => file.size > MAX_ATTACHMENT_BYTES);
  if (oversized) {
    throw new Error(`${oversized.name} is too large. The per-file limit is 25 MB.`);
  }
  if (totalSize > MAX_TOTAL_ATTACHMENT_BYTES) {
    throw new Error("The combined attachment limit is 50 MB.");
  }

  return Promise.all(
    files.map(async (file) => ({
      name: file.name,
      mime: fileMime(file),
      data: await fileToBase64(file)
    }))
  );
}
