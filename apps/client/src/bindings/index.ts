import { invoke as __TAURI_INVOKE } from "@tauri-apps/api/core";

/** Commands */
export const commands = {
  async greet(): Promise<string> {
    return await __TAURI_INVOKE("greet");
  },
  async mediaImport(paths: string[]): Promise<ImportedMedia[]> {
    return await __TAURI_INVOKE("mediaImport", { paths });
  },
  async quotaGet(): Promise<QuotaInfo> {
    return await __TAURI_INVOKE("quota_get");
  },
  async quotaSet(newCapBytes: number): Promise<void> {
    await __TAURI_INVOKE("quota_set", { newCapBytes });
  },
  async libraryScan(): Promise<ScanResult> {
    return await __TAURI_INVOKE("library_scan");
  },
  async mediaResolveUrl(mediaId: string): Promise<string> {
    return await __TAURI_INVOKE("media_resolve_url", { mediaId });
  },
  async identityGet(displayName: string): Promise<Identity> {
    return await __TAURI_INVOKE("identity_get", { displayName });
  },
  async identityRotate(displayName: string): Promise<Identity> {
    return await __TAURI_INVOKE("identity_rotate", { displayName });
  },
  async identitySetDisplayName(displayName: string): Promise<Identity> {
    return await __TAURI_INVOKE("identity_set_display_name", { displayName });
  },
};

/* Types */
export type Identity = {
  user_id: string;
  public_key: string;
  display_name: string;
};

export type ImportedMedia = {
  id: string;
  sha256: string;
  blake3: string;
  size_bytes: number;
  filename: string;
  relative_path: string;
};

export type QuotaInfo = {
  used_bytes: number;
  cap_bytes: number;
};

export type ScanResult = {
  files_scanned: number;
  files_upserted: number;
  files_orphans_discovered: number;
  files_missing: number;
  files_failed: number;
  bytes_total: number;
};
