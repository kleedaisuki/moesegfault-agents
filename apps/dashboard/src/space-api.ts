import { request } from "./api";
import { locale } from "./i18n";

export type SpaceInfo = {
  id: string;
  object: "space";
  name: string;
  created_at: number;
  updated_at: number;
  usage: { bytes: number; files: number; limit_bytes: number };
};

export type SpaceFile = {
  id: string;
  object: "file";
  bytes: number;
  created_at: number;
  filename: string;
  content_type?: string;
  purpose: string;
  status: string;
  sha256: string;
};

export type SpacePage<T> = {
  object: "list";
  data: T[];
  has_more: boolean;
  last_id?: string | null;
};

export type WorkspaceVersion = {
  id: string;
  object: "workspace.version";
  name: string;
  session_id: string;
  created_at_ms: number;
  generation: number;
  sha256: string;
  format_version: number;
  archive_bytes: number;
  logical_bytes: number;
  entries: number;
};

export function spacePage<T>(
  path: string,
  after: string | undefined,
  signal: AbortSignal,
) {
  const query = new URLSearchParams({ limit: "20" });
  if (after) query.set("after", after);
  return request<SpacePage<T>>(`${path}?${query}`, { signal });
}

export async function uploadSpaceFile(file: File): Promise<SpaceFile> {
  const body = new FormData();
  body.set("file", file);
  body.set("purpose", "user_data");
  return request<SpaceFile>("/v1/space/files", { method: "POST", body });
}

export function formattedBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const exponent = bytes
    ? Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
    : 0;
  return `${new Intl.NumberFormat(locale(), { maximumFractionDigits: exponent ? 1 : 0 }).format(bytes / 1024 ** exponent)} ${units[exponent]}`;
}
