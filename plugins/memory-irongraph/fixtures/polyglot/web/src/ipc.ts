import { invoke } from '@tauri-apps/api/core';

declare const ipcRenderer: { invoke(channel: string, ...args: unknown[]): Promise<unknown> };

export function loadSettings(key: string) {
  return ipcRenderer.invoke('settings:get', key);
}

export function saveDocument(path: string, body: string) {
  return invoke('save_file', { path, body });
}
